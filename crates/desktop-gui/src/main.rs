#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use audioshare_client_engine::{ClientConfig, ClientEngine};
use audioshare_host_engine::app_routing::{
    list_active_audio_apps, restore_all_apps_to_default_sink, route_single_app_to_audioshare,
    AudioAppEntry,
};
use audioshare_host_engine::bt_optimization::apply_automatic_video_lipsync_offset_ms;
use audioshare_host_engine::discovery_and_os::{
    open_os_firewall_ports, set_os_boot_autostart, AdbUsbWatcher, DirectHotspotController,
    DiscoveryBeacon, DiscoveryScanner, VirtualSpeakerAndRadioGuard,
};
use audioshare_host_engine::virtual_mic::{PhoneMicReceiver, PhoneRxRoutingMode, PHONE_MIC_PORT};
use audioshare_host_engine::{
    is_usb_subnet_ip, list_valid_host_interfaces, HostConfig, HostEngine, NetworkInterfaceInfo,
    DEFAULT_PORT,
};
use audioshare_protocol::latency::{codec_mode_name, DESKTOP_OUTPUT_BUFFER_MS, SUBFRAME_MS};
use audioshare_protocol::lpc_lossless::QualityProfile;
use audioshare_protocol::sync_and_eq::{build_qr_connect_uri, ChannelRole};
use eframe::egui;
use qrcode::{Color as QrColor, QrCode};

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([860.0, 720.0])
            .with_min_inner_size([700.0, 560.0])
            .with_title("AudioShare Ultra — Zero-Jitter Lossless Audio Mesh"),
        ..Default::default()
    };

    eframe::run_native(
        "AudioShare Ultra",
        options,
        Box::new(|cc| {
            let app = AudioShareApp::new();
            cc.egui_ctx.set_visuals(if app.dark_mode {
                egui::Visuals::dark()
            } else {
                egui::Visuals::light()
            });
            Ok(Box::new(app))
        }),
    )
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum AppTab {
    HostSender,
    ClientReceiver,
}

struct AudioShareApp {
    tab: AppTab,
    interfaces: Vec<NetworkInterfaceInfo>,
    show_qr_modal: bool,
    dark_mode: bool,
    minimize_on_close: bool,
    autostart_stream_on_launch: bool,
    os_boot_autostart: bool,
    force_quit: bool,

    // Host State
    host_engine: Option<HostEngine>,
    beacon: Option<DiscoveryBeacon>,
    adb_watcher: Option<AdbUsbWatcher>,
    os_guard: Option<VirtualSpeakerAndRadioGuard>,
    quality_profile: QualityProfile,
    target_bitrate_kbps: u16,
    usb_low_latency_pacing: bool,
    enable_smart_dtx: bool,
    enable_virtual_speaker: bool,
    auto_bt_video_lipsync: bool,
    enable_group_sync: bool,
    group_sync_trim_ms: u16,
    last_applied_bt_delay: u16,
    host_master_volume: u8,
    use_test_tone: bool,
    host_status_msg: String,
    last_rate_calc: Instant,
    prev_bytes_sent: u64,
    prev_pkts_sent: u64,
    live_kbps: u32,
    live_pps: u32,

    // 1-Hop Wi-Fi Direct SoftAP State
    direct_hotspot_active: bool,
    hotspot_ssid: String,
    hotspot_pass: String,
    hotspot_status: String,

    // Security, Per-App & Bidirectional Mic State
    enable_pin_encryption: bool,
    pairing_pin: String,
    audio_apps: Vec<AudioAppEntry>,
    selected_app_idx: Option<usize>,
    app_route_status: String,
    phone_mic_receiver: Option<PhoneMicReceiver>,
    phone_rx_mode: PhoneRxRoutingMode,

    // Client & Studio DSP State
    scanner: DiscoveryScanner,
    client_engine: Option<ClientEngine>,
    manual_server_ip: String,
    preamp_gain_db: f32,
    eq_bass_db: f32,
    eq_presence_db: f32,
    eq_treble_db: f32,
    channel_role: ChannelRole,
    binaural_crossfeed: bool,
    night_mode_drc: bool,
    measured_sonar_ms: Option<f32>,
    measured_sonar_at: Option<Instant>,
    latency_debug: bool,
    gaming_hard_lock: bool,
    usb_buffer_mode: bool,
    drift_history: VecDeque<f32>,
    client_status_msg: String,
}

/// Bluetooth A2DP delay reports arrive in 1 ms steps. Re-running the PipeWire
/// offset helper for every step spawns a `pactl` process and can make the
/// video offset audibly hunt. Apply the first non-zero report, reset when the
/// delay returns to zero, and otherwise ignore changes smaller than 10 ms.
fn should_apply_lipsync(new_delay_ms: u16, last_delay_ms: u16) -> bool {
    if new_delay_ms == last_delay_ms {
        return false;
    }
    if new_delay_ms == 0 || last_delay_ms == 0 {
        return true;
    }
    new_delay_ms.abs_diff(last_delay_ms) >= 10
}

impl AudioShareApp {
    fn new() -> Self {
        let mut app = Self {
            tab: AppTab::HostSender,
            interfaces: list_valid_host_interfaces(),
            show_qr_modal: false,
            dark_mode: true,
            minimize_on_close: true,
            autostart_stream_on_launch: false,
            os_boot_autostart: false,
            force_quit: false,
            host_engine: None,
            beacon: None,
            adb_watcher: None,
            os_guard: None,
            quality_profile: QualityProfile::SmartAuto,
            target_bitrate_kbps: 256,
            usb_low_latency_pacing: false,
            enable_smart_dtx: true,
            enable_virtual_speaker: false,
            auto_bt_video_lipsync: true,
            enable_group_sync: true,
            group_sync_trim_ms: 0,
            last_applied_bt_delay: 0,
            host_master_volume: 100,
            use_test_tone: false,
            host_status_msg: "Stopped. Click 'Start Host Server' to broadcast.".into(),
            last_rate_calc: Instant::now(),
            prev_bytes_sent: 0,
            prev_pkts_sent: 0,
            live_kbps: 0,
            live_pps: 0,
            direct_hotspot_active: false,
            hotspot_ssid: "AudioShare_5G".into(),
            hotspot_pass: "audioshare88".into(),
            hotspot_status: "Router LAN Mode (Standard)".into(),
            enable_pin_encryption: false,
            pairing_pin: "4829".into(),
            audio_apps: list_active_audio_apps(),
            selected_app_idx: None,
            app_route_status: "Capturing Full System Audio".into(),
            phone_mic_receiver: None,
            phone_rx_mode: PhoneRxRoutingMode::VirtualMicForCalls,
            scanner: DiscoveryScanner::start(),
            client_engine: None,
            manual_server_ip: format!("127.0.0.1:{}", DEFAULT_PORT),
            preamp_gain_db: 0.0,
            eq_bass_db: 0.0,
            eq_presence_db: 0.0,
            eq_treble_db: 0.0,
            channel_role: ChannelRole::Stereo,
            binaural_crossfeed: false,
            night_mode_drc: false,
            measured_sonar_ms: None,
            measured_sonar_at: None,
            latency_debug: false,
            gaming_hard_lock: false,
            usb_buffer_mode: false,
            drift_history: VecDeque::from(vec![0.0; 120]),
            client_status_msg: "Idle. Select a discovered host or enter IP.".into(),
        };
        app.load_persistent_config();
        let launched_via_autostart = std::env::args().any(|a| a == "--autostart");
        if app.autostart_stream_on_launch || launched_via_autostart {
            app.toggle_host();
        }
        app
    }

    fn config_file_path() -> std::path::PathBuf {
        if let Ok(cfg_dir) = std::env::var("XDG_CONFIG_HOME") {
            let p = std::path::PathBuf::from(cfg_dir);
            if p.is_dir() {
                return p.join("audioshare-ultra.conf");
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            let config_dir = std::path::Path::new(&home).join(".config");
            if config_dir.is_dir() {
                return config_dir.join("audioshare-ultra.conf");
            }
            return std::path::Path::new(&home).join(".audioshare-ultra.conf");
        }
        if let Ok(appdata) = std::env::var("APPDATA") {
            return std::path::Path::new(&appdata).join("audioshare-ultra.conf");
        }
        std::path::PathBuf::from(".audioshare-ultra.conf")
    }

    fn save_persistent_config(&self) {
        let content = format!(
            "dark_mode={}\n\
             minimize_on_close={}\n\
             autostart_stream_on_launch={}\n\
             os_boot_autostart={}\n\
             hotspot_ssid={}\n\
             hotspot_pass={}\n\
             quality_profile={}\n\
             target_bitrate_kbps={}\n\
             usb_low_latency_pacing={}\n\
             enable_smart_dtx={}\n\
             enable_virtual_speaker={}\n\
             auto_bt_video_lipsync={}\n\
             enable_group_sync={}\n\
             group_sync_trim_ms={}\n\
             host_master_volume={}\n\
             enable_pin_encryption={}\n\
             pairing_pin={}\n\
             manual_server_ip={}\n\
             preamp_gain_db={}\n\
             eq_bass_db={}\n\
             eq_presence_db={}\n\
             eq_treble_db={}\n\
             channel_role={}\n\
             binaural_crossfeed={}\n\
             night_mode_drc={}\n\
             latency_debug={}\n\
             gaming_hard_lock={}\n\
             phone_rx_mode={}\n",
            self.dark_mode,
            self.minimize_on_close,
            self.autostart_stream_on_launch,
            self.os_boot_autostart,
            self.hotspot_ssid,
            self.hotspot_pass,
            self.quality_profile as u8,
            self.target_bitrate_kbps,
            self.usb_low_latency_pacing,
            self.enable_smart_dtx,
            self.enable_virtual_speaker,
            self.auto_bt_video_lipsync,
            self.enable_group_sync,
            self.group_sync_trim_ms,
            self.host_master_volume,
            self.enable_pin_encryption,
            self.pairing_pin,
            self.manual_server_ip,
            self.preamp_gain_db,
            self.eq_bass_db,
            self.eq_presence_db,
            self.eq_treble_db,
            self.channel_role as u8,
            self.binaural_crossfeed,
            self.night_mode_drc,
            self.latency_debug,
            self.gaming_hard_lock,
            self.phone_rx_mode as u8,
        );
        let _ = std::fs::write(Self::config_file_path(), content);
    }

    fn load_persistent_config(&mut self) {
        if let Ok(text) = std::fs::read_to_string(Self::config_file_path()) {
            self.parse_config_string(&text);
        }
    }

    fn parse_config_string(&mut self, text: &str) {
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                match k.trim() {
                    "dark_mode" => self.dark_mode = v.trim() != "false",
                    "minimize_on_close" => self.minimize_on_close = v.trim() != "false",
                    "autostart_stream_on_launch" => {
                        self.autostart_stream_on_launch = v.trim() == "true"
                    }
                    "os_boot_autostart" => self.os_boot_autostart = v.trim() == "true",
                    "hotspot_ssid" => self.hotspot_ssid = v.trim().to_string(),
                    "hotspot_pass" => self.hotspot_pass = v.trim().to_string(),
                    "quality_profile" => {
                        if let Ok(n) = v.trim().parse::<u8>() {
                            self.quality_profile = QualityProfile::from_u8(n);
                        }
                    }
                    "target_bitrate_kbps" => {
                        if let Ok(n) = v.trim().parse::<u16>() {
                            self.target_bitrate_kbps = n;
                        }
                    }
                    "usb_low_latency_pacing" => self.usb_low_latency_pacing = v.trim() == "true",
                    "enable_smart_dtx" => self.enable_smart_dtx = v.trim() == "true",
                    "enable_virtual_speaker" => self.enable_virtual_speaker = v.trim() == "true",
                    "auto_bt_video_lipsync" => self.auto_bt_video_lipsync = v.trim() == "true",
                    "enable_group_sync" => self.enable_group_sync = v.trim() != "false",
                    "group_sync_trim_ms" => {
                        if let Ok(n) = v.trim().parse::<u16>() {
                            self.group_sync_trim_ms = n.min(120);
                        }
                    }
                    "host_master_volume" => {
                        if let Ok(n) = v.trim().parse::<u8>() {
                            self.host_master_volume = n.min(100);
                        }
                    }
                    "enable_pin_encryption" => self.enable_pin_encryption = v.trim() == "true",
                    "pairing_pin" => self.pairing_pin = v.trim().to_string(),
                    "manual_server_ip" => self.manual_server_ip = v.trim().to_string(),
                    "preamp_gain_db" => {
                        if let Ok(f) = v.trim().parse::<f32>() {
                            self.preamp_gain_db = f.clamp(-12.0, 24.0);
                        }
                    }
                    "eq_bass_db" => {
                        if let Ok(f) = v.trim().parse::<f32>() {
                            self.eq_bass_db = f.clamp(-12.0, 12.0);
                        }
                    }
                    "eq_presence_db" => {
                        if let Ok(f) = v.trim().parse::<f32>() {
                            self.eq_presence_db = f.clamp(-12.0, 12.0);
                        }
                    }
                    "eq_treble_db" => {
                        if let Ok(f) = v.trim().parse::<f32>() {
                            self.eq_treble_db = f.clamp(-12.0, 12.0);
                        }
                    }
                    "channel_role" => {
                        if let Ok(n) = v.trim().parse::<u8>() {
                            self.channel_role = ChannelRole::from_u8(n);
                        }
                    }
                    "binaural_crossfeed" => self.binaural_crossfeed = v.trim() == "true",
                    "night_mode_drc" => self.night_mode_drc = v.trim() == "true",
                    "latency_debug" => self.latency_debug = v.trim() == "true",
                    "gaming_hard_lock" => self.gaming_hard_lock = v.trim() == "true",
                    "phone_rx_mode" => {
                        if v.trim() == "1" || v.trim().eq_ignore_ascii_case("speakers") {
                            self.phone_rx_mode = PhoneRxRoutingMode::PlayOnPcSpeakers;
                        } else {
                            self.phone_rx_mode = PhoneRxRoutingMode::VirtualMicForCalls;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn toggle_host(&mut self) {
        if let Some(mut host) = self.host_engine.take() {
            host.stop();
            self.beacon = None;
            self.adb_watcher = None;
            self.os_guard = None;
            self.host_status_msg = "Host stopped.".into();
        } else {
            self.interfaces = list_valid_host_interfaces();
            let cfg = HostConfig {
                bind_port: DEFAULT_PORT,
                subframes_per_packet: if self.usb_low_latency_pacing { 1 } else { 2 },
                use_test_tone: self.use_test_tone,
                enable_smart_dtx: self.enable_smart_dtx,
            };

            // 1. Create Virtual Speaker & suppress 60s Wi-Fi scan BEFORE opening audio capture!
            let guard = VirtualSpeakerAndRadioGuard::acquire(self.enable_virtual_speaker);

            match HostEngine::start(cfg, vec![]) {
                Ok(engine) => {
                    if self.enable_pin_encryption {
                        engine.set_encryption_pin(Some(&self.pairing_pin));
                    }
                    engine
                        .telemetry
                        .quality_profile
                        .store(self.quality_profile as u8, Ordering::Relaxed);
                    engine
                        .telemetry
                        .target_bitrate_kbps
                        .store(self.target_bitrate_kbps, Ordering::Relaxed);
                    engine
                        .telemetry
                        .enable_group_sync
                        .store(self.enable_group_sync, Ordering::Relaxed);
                    engine
                        .telemetry
                        .manual_group_sync_trim_ms
                        .store(self.group_sync_trim_ms, Ordering::Relaxed);
                    self.os_guard = Some(guard);
                    self.beacon = Some(DiscoveryBeacon::start(
                        DEFAULT_PORT,
                        "PC-AudioShare-Ultra".into(),
                    ));
                    self.adb_watcher = Some(AdbUsbWatcher::start(DEFAULT_PORT));
                    self.host_engine = Some(engine);
                    self.host_status_msg =
                        "Broadcasting 48kHz Studio Lossless + [N-1]/[N-2] FEC (802.11e AC_VI)"
                            .into();
                }
                Err(e) => {
                    self.host_status_msg = format!("Error: {}", e);
                }
            }
        }
    }

    fn connect_client(&mut self, addr: SocketAddr) {
        if let Some(mut c) = self.client_engine.take() {
            c.stop();
        }
        let cfg = ClientConfig {
            server_addr: addr,
            target_buffer_subframes: if self.usb_buffer_mode || self.gaming_hard_lock {
                2.0
            } else {
                3.5
            },
            preamp_gain_db: self.preamp_gain_db,
        };
        let pin_opt = if self.enable_pin_encryption {
            Some(self.pairing_pin.as_str())
        } else {
            None
        };
        match ClientEngine::connect_with_pin(cfg, pin_opt) {
            Ok(engine) => {
                self.sync_client_dsp_atomics(&engine);
                self.client_engine = Some(engine);
                self.client_status_msg = format!("Connected to {} (Zero-Jitter Lock Active)", addr);
            }
            Err(e) => {
                self.client_status_msg = format!("Connection error: {}", e);
            }
        }
    }

    fn sync_client_dsp_atomics(&self, c: &ClientEngine) {
        c.telemetry
            .preamp_gain_db_tenths
            .store((self.preamp_gain_db * 10.0) as i32, Ordering::Relaxed);
        c.telemetry
            .eq_bass_db_tenths
            .store((self.eq_bass_db * 10.0) as i32, Ordering::Relaxed);
        c.telemetry
            .eq_presence_db_tenths
            .store((self.eq_presence_db * 10.0) as i32, Ordering::Relaxed);
        c.telemetry
            .eq_treble_db_tenths
            .store((self.eq_treble_db * 10.0) as i32, Ordering::Relaxed);
        c.telemetry
            .channel_role
            .store(self.channel_role as u8, Ordering::Relaxed);
        c.telemetry
            .enable_binaural_crossfeed
            .store(self.binaural_crossfeed, Ordering::Relaxed);
        c.telemetry
            .enable_night_mode
            .store(self.night_mode_drc, Ordering::Relaxed);
        c.telemetry
            .gaming_mode_hard_lock
            .store(self.gaming_hard_lock, Ordering::Relaxed);
    }

    /// Render a crisp, scannable ISO-18004 QR Code directly onto the egui canvas
    fn draw_qr_code(ui: &mut egui::Ui, uri: &str) {
        let Ok(code) = QrCode::new(uri.as_bytes()) else {
            return;
        };
        let width = code.width();
        let module_px = 5.0f32;
        let quiet_zone = 4;
        let total_modules = (width + quiet_zone * 2) as f32;
        let size_px = total_modules * module_px;

        let (rect, _) = ui.allocate_exact_size(egui::vec2(size_px, size_px), egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 6.0, egui::Color32::WHITE);

        for y in 0..width {
            for x in 0..width {
                if code[(x, y)] == QrColor::Dark {
                    let min = rect.min
                        + egui::vec2(
                            ((x + quiet_zone) as f32) * module_px,
                            ((y + quiet_zone) as f32) * module_px,
                        );
                    let cell = egui::Rect::from_min_size(min, egui::vec2(module_px, module_px));
                    painter.rect_filled(cell, 0.0, egui::Color32::BLACK);
                }
            }
        }
    }
}

impl eframe::App for AudioShareApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Close-to-Background Protection: If streaming is active and user clicks window 'X',
        // minimize to taskbar (0.0% CPU) instead of killing the live audio stream!
        if ctx.input(|i| i.viewport().close_requested())
            && !self.force_quit
            && self.minimize_on_close
            && (self.host_engine.is_some() || self.client_engine.is_some())
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }

        // Drop GUI CPU usage to ~0% while minimized: audio/network threads keep
        // running, and winit asks for a repaint as soon as the window returns.
        let is_minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        if !is_minimized {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        // Auto-switch to the virtual speaker only while phones are connected,
        // then restore the physical speakers when the last client leaves.
        if let Some(ref host) = self.host_engine {
            let client_count = host.telemetry.active_clients_count.load(Ordering::Relaxed);
            self.host_master_volume = host.telemetry.host_volume.load(Ordering::Relaxed);
            // Reflect profile changes requested from a connected phone.
            self.quality_profile =
                QualityProfile::from_u8(host.telemetry.quality_profile.load(Ordering::Relaxed));
            if let Some(ref mut guard) = self.os_guard {
                if client_count > 0 {
                    guard.activate();
                } else {
                    guard.deactivate();
                }
            }
        }

        egui::TopBottomPanel::top("top_nav").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("🎧 AudioShare Ultra");
                ui.separator();
                ui.selectable_value(
                    &mut self.tab,
                    AppTab::HostSender,
                    "📡 Host Server (Send Audio)",
                );
                ui.selectable_value(
                    &mut self.tab,
                    AppTab::ClientReceiver,
                    "🔊 Client Receiver & Studio DSP",
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("🚪 Quit").clicked() {
                        self.force_quit = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    let debug_lbl = if self.latency_debug {
                        "📊 Latency Debug: ON"
                    } else {
                        "📊 Latency Debug"
                    };
                    if ui.button(debug_lbl).clicked() {
                        self.latency_debug = !self.latency_debug;
                    }
                    let theme_lbl = if self.dark_mode {
                        "☀️ Light"
                    } else {
                        "🌙 Dark"
                    };
                    if ui.button(theme_lbl).clicked() {
                        self.dark_mode = !self.dark_mode;
                        ctx.set_visuals(if self.dark_mode {
                            egui::Visuals::dark()
                        } else {
                            egui::Visuals::light()
                        });
                    }
                    if ui.button("📱 Scan QR to Connect Phone").clicked() {
                        self.interfaces = list_valid_host_interfaces();
                        self.show_qr_modal = !self.show_qr_modal;
                    }
                });
            });
            ui.add_space(4.0);
        });

        // QR Code Pairing Modal
        if self.show_qr_modal {
            egui::Window::new("📱 1-Scan Phone QR Pairing")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    let primary_ip = self
                        .interfaces
                        .first()
                        .map(|i| i.ip.to_string())
                        .unwrap_or_else(|| "192.168.1.100".into());
                    let pin_opt = if self.enable_pin_encryption {
                        Some(self.pairing_pin.as_str())
                    } else {
                        None
                    };
                    let uri = build_qr_connect_uri(&primary_ip, DEFAULT_PORT, pin_opt);

                    ui.vertical_centered(|ui| {
                        ui.label("Scan with your Android Camera or AudioShare Ultra app:");
                        ui.add_space(6.0);
                        Self::draw_qr_code(ui, &uri);
                        ui.add_space(6.0);
                        ui.monospace(&uri);
                        ui.add_space(6.0);
                        if ui.button("📋 Copy Connection URI").clicked() {
                            ui.ctx().copy_text(uri.clone());
                        }
                        ui.add_space(6.0);
                        if ui.button("Close QR Window").clicked() {
                            self.show_qr_modal = false;
                        }
                    });
                });
        }

        if self.latency_debug {
            egui::Window::new("📊 Live Latency Debug")
                .default_width(560.0)
                .default_height(460.0)
                .resizable(true)
                .show(ctx, |ui| self.render_latency_debug(ui));
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
                AppTab::HostSender => self.render_host_tab(ui),
                AppTab::ClientReceiver => self.render_client_tab(ui),
            });
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_persistent_config();
        if let Some(mut host) = self.host_engine.take() {
            host.stop();
        }
        if let Some(mut client) = self.client_engine.take() {
            client.stop();
        }
        if let Some(mut mic) = self.phone_mic_receiver.take() {
            mic.stop();
        }
        self.beacon = None;
        self.adb_watcher = None;
        // Explicitly drop VirtualSpeakerAndRadioGuard to restore physical PC speakers on window close!
        self.os_guard = None;
    }
}

impl AudioShareApp {
    fn render_host_tab(&mut self, ui: &mut egui::Ui) {
        let is_running = self.host_engine.is_some();

        ui.group(|ui| {
            ui.horizontal(|ui| {
                let btn_text = if is_running {
                    "⏹ Stop Host Server"
                } else {
                    "▶ Start Host Server"
                };
                let btn = egui::Button::new(egui::RichText::new(btn_text).size(16.0).strong());
                if ui.add_sized([200.0, 38.0], btn).clicked() {
                    self.toggle_host();
                }
                ui.label(
                    egui::RichText::new(&self.host_status_msg).color(if is_running {
                        egui::Color32::LIGHT_GREEN
                    } else {
                        egui::Color32::GRAY
                    }),
                );
            });

            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(
                    &mut self.usb_low_latency_pacing,
                    "⚡ 2.5ms USB Pacing (Uncheck for 5.0ms Bundled Wi-Fi Pacing)",
                );
                ui.checkbox(&mut self.enable_smart_dtx, "🔋 Smart DTX Silence Saver");
            });
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(
                    &mut self.enable_virtual_speaker,
                    "🔇 Auto Virtual Speaker + Win32 60s Scan Blocker",
                );
                ui.checkbox(
                    &mut self.auto_bt_video_lipsync,
                    "🎬 Auto Video Lip-Sync for Phone Bluetooth Earbuds",
                );
                ui.checkbox(&mut self.use_test_tone, "🎵 440Hz/554Hz Test Chord");
            });
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(
                    &mut self.autostart_stream_on_launch,
                    "🚀 Auto-Start Broadcast on App Launch",
                );
                if ui
                    .checkbox(&mut self.os_boot_autostart, "🖥️ Start App on OS Login")
                    .changed()
                {
                    self.host_status_msg = set_os_boot_autostart(self.os_boot_autostart);
                }
                ui.checkbox(
                    &mut self.minimize_on_close,
                    "📌 Minimize on Window Close (Keep Stream Alive)",
                );
            });
        });

        ui.add_space(8.0);

        ui.group(|ui| {
            ui.heading("Quality Mode & Compressed Bitrate");
            ui.horizontal_wrapped(|ui| {
                ui.label("🎚️ Mode:");
                for (profile, label) in [
                    (QualityProfile::SmartAuto, "🤖 Smart Auto"),
                    (QualityProfile::StudioLossless, "💎 Studio Lossless 16-Bit (0ms)"),
                    (
                        QualityProfile::StudioLossless24Bit,
                        "👑 Studio Master 24-Bit (0ms)",
                    ),
                    (
                        QualityProfile::LowLatencyCompressed,
                        "⚡ Low-Latency Compressed",
                    ),
                    (
                        QualityProfile::MaxStability24Ghz,
                        "🛡️ Max Stability (10ms 2.4GHz)",
                    ),
                ] {
                    if ui
                        .selectable_value(&mut self.quality_profile, profile, label)
                        .changed()
                    {
                        if let Some(ref host) = self.host_engine {
                            host.telemetry
                                .quality_profile
                                .store(self.quality_profile as u8, Ordering::Relaxed);
                        }
                    }
                }
            });

            if self.quality_profile == QualityProfile::LowLatencyCompressed
                || self.quality_profile == QualityProfile::SmartAuto
            {
                ui.horizontal_wrapped(|ui| {
                    ui.label("📶 Compressed bitrate:");
                    for kbps in [96u16, 160, 256, 320] {
                        if ui
                            .selectable_value(
                                &mut self.target_bitrate_kbps,
                                kbps,
                                format!("{} kbps", kbps),
                            )
                            .changed()
                        {
                            if let Some(ref host) = self.host_engine {
                                host.telemetry
                                    .target_bitrate_kbps
                                    .store(self.target_bitrate_kbps, Ordering::Relaxed);
                            }
                        }
                    }
                });
            }

            let note = match self.quality_profile {
                QualityProfile::SmartAuto => {
                    "Adapts: lossless at <4.5% loss, compressed at >4.5%, 10ms/160k at >12%."
                }
                QualityProfile::StudioLossless => {
                    "100% bit-perfect 16-bit LPC-Delta, 2.5/5.0ms framing, zero codec lookahead."
                }
                QualityProfile::StudioLossless24Bit => {
                    "True 24-bit: upper-16-bit LPC + exact LSB plane, 2.5ms single-subframe packets."
                }
                QualityProfile::LowLatencyCompressed => {
                    "VBR DPCM primary payload at the selected target; ADPCM FEC stays active."
                }
                QualityProfile::MaxStability24Ghz => {
                    "Fixed 10ms (4-subframe) framing at 160 kbps for long-range 2.4GHz."
                }
            };
            ui.label(egui::RichText::new(note).color(egui::Color32::GRAY));
        });

        ui.add_space(8.0);

        ui.group(|ui| {
            ui.heading("Active Network, USB & 1-Hop Direct Wireless Endpoints");
            if let Some(ref adb) = self.adb_watcher {
                let usb_ok = adb.usb_device_connected.load(Ordering::Relaxed);
                ui.label(if usb_ok {
                    "🔌 ADB USB Device Detected — Zero-Touch Wired Tunnel Active!"
                } else {
                    "🔌 ADB USB Watcher: Waiting for USB cable connection..."
                });
            }
            for iface in &self.interfaces {
                let tag = if iface.is_usb_tethering {
                    "⚡ [USB RNDIS Priority]"
                } else {
                    "📶 [Wi-Fi / Ethernet]"
                };
                ui.horizontal(|ui| {
                    let ep = format!("{}:{}", iface.ip, DEFAULT_PORT);
                    ui.monospace(format!("{} {} ({})", tag, ep, iface.name));
                    if ui.small_button("📋 Copy IP").clicked() {
                        ui.ctx().copy_text(ep);
                    }
                });
            }
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("SSID:");
                ui.add_sized(
                    [110.0, 20.0],
                    egui::TextEdit::singleline(&mut self.hotspot_ssid),
                );
                ui.label("Pass:");
                ui.add_sized(
                    [100.0, 20.0],
                    egui::TextEdit::singleline(&mut self.hotspot_pass),
                );
                let hs_btn = if self.direct_hotspot_active {
                    "Stop 1-Hop Direct Hotspot"
                } else {
                    "📡 Start 1-Hop 5GHz Link"
                };
                if ui.button(hs_btn).clicked() {
                    if self.direct_hotspot_active {
                        DirectHotspotController::stop_direct_hotspot();
                        self.direct_hotspot_active = false;
                        self.hotspot_status = "Router LAN Mode".into();
                    } else {
                        match DirectHotspotController::start_direct_5ghz_hotspot(
                            &self.hotspot_ssid,
                            &self.hotspot_pass,
                        ) {
                            Ok(msg) => {
                                self.direct_hotspot_active = true;
                                self.hotspot_status = msg;
                            }
                            Err(e) => self.hotspot_status = format!("Hotspot notice: {}", e),
                        }
                    }
                }
                if ui
                    .button("🛡️ 1-Click Open Firewall (65530-65532)")
                    .clicked()
                {
                    self.hotspot_status = open_os_firewall_ports();
                }
                ui.monospace(&self.hotspot_status);
            });
        });

        ui.add_space(8.0);

        ui.group(|ui| {
            ui.heading("Connected Clients & Dynamic Link Governor");
            if let Some(ref host) = self.host_engine {
                let pkts = host.telemetry.packets_sent.load(Ordering::Relaxed);
                let kb = host.telemetry.bytes_sent.load(Ordering::Relaxed) / 1024;
                let dtx = host.telemetry.dtx_silent_mode.load(Ordering::Relaxed);
                let mode_id = host.telemetry.active_codec_mode.load(Ordering::Relaxed);
                let bt_delay = host.telemetry.max_client_bt_delay_ms.load(Ordering::Relaxed);
                let rebinds = host.telemetry.hotplug_rebinds.load(Ordering::Relaxed);
                let bytes_now = host.telemetry.bytes_sent.load(Ordering::Relaxed);

                if self.last_rate_calc.elapsed() >= Duration::from_secs(1) {
                    let dt = self.last_rate_calc.elapsed().as_secs_f32().max(0.1);
                    self.live_kbps = (((bytes_now.saturating_sub(self.prev_bytes_sent)) as f32 * 8.0)
                        / (dt * 1000.0)) as u32;
                    self.live_pps =
                        ((pkts.saturating_sub(self.prev_pkts_sent)) as f32 / dt) as u32;
                    self.prev_bytes_sent = bytes_now;
                    self.prev_pkts_sent = pkts;
                    self.last_rate_calc = Instant::now();
                }

                // Automatically adjust PipeWire sink latency offset when phone Bluetooth delay changes!
                if self.auto_bt_video_lipsync
                    && should_apply_lipsync(bt_delay, self.last_applied_bt_delay)
                {
                    apply_automatic_video_lipsync_offset_ms(bt_delay);
                    self.last_applied_bt_delay = bt_delay;
                }

                let mode_str = if dtx {
                    "SMART-DTX SILENCE (0.6 kbps)".to_string()
                } else if mode_id == 2 {
                    format!(
                        "GOVERNOR: COMPRESSED VBR {} kbps (Weak Wi-Fi Protection)",
                        self.target_bitrate_kbps
                    )
                } else if self.quality_profile == QualityProfile::StudioLossless24Bit {
                    "GOVERNOR: STUDIO MASTER 24-BIT LOSSLESS + FEC".to_string()
                } else {
                    "GOVERNOR: STUDIO LOSSLESS LPC + [N-1]/[N-2] FEC".to_string()
                };

                ui.monospace(format!(
                    "Live Speed: {} kbps ({} pkt/s) | Total: {} KB | Rebinds: {} | {}",
                    self.live_kbps, self.live_pps, kb, rebinds, mode_str
                ));

                // Live 60 FPS Audio Capture VU Peak Bar
                let vu_pct = host.telemetry.host_vu_peak_pct.load(Ordering::Relaxed);
                ui.horizontal(|ui| {
                    ui.label("Live Audio Peak:");
                    let bar_color = if vu_pct > 92 {
                        egui::Color32::RED
                    } else if vu_pct > 70 {
                        egui::Color32::GOLD
                    } else {
                        egui::Color32::LIGHT_GREEN
                    };
                    ui.add(
                        egui::ProgressBar::new((vu_pct as f32) / 100.0)
                            .fill(bar_color)
                            .text(format!("{}% Peak", vu_pct)),
                    );
                    ui.separator();
                    ui.label("Master Vol:");
                    if ui
                        .add(egui::Slider::new(&mut self.host_master_volume, 0..=100).suffix("%"))
                        .changed()
                    {
                        host.telemetry
                            .host_volume
                            .store(self.host_master_volume, Ordering::Relaxed);
                    }
                });

                // Multi-Device Acoustic Group Sync Controls
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .checkbox(
                            &mut self.enable_group_sync,
                            "🔗 Multi-Device Acoustic Sync (Lock All Phones/Speakers in Phase)",
                        )
                        .changed()
                    {
                        host.telemetry
                            .enable_group_sync
                            .store(self.enable_group_sync, Ordering::Relaxed);
                    }
                    if self.enable_group_sync {
                        ui.label("Room Trim:");
                        if ui
                            .add(
                                egui::Slider::new(&mut self.group_sync_trim_ms, 0..=120)
                                    .suffix(" ms"),
                            )
                            .changed()
                        {
                            host.telemetry
                                .manual_group_sync_trim_ms
                                .store(self.group_sync_trim_ms, Ordering::Relaxed);
                        }
                    }
                });

                if bt_delay > 40 {
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        format!(
                            "🎧 Phone Bluetooth Earbuds Active (+{} ms A2DP delay) — Auto Video Lip-Sync Applied!",
                            bt_delay
                        ),
                    );
                }

                ui.separator();
                let clients = host.connected_clients();
                if clients.is_empty() {
                    ui.label("Waiting for clients (Click '📱 Scan QR to Connect Phone' in top-right or use mDNS)...");
                } else {
                    for client in clients {
                        ui.horizontal(|ui| {
                            let badge = if client.is_usb { "[USB]" } else { "[Wi-Fi]" };
                            ui.monospace(format!(
                                "{} {} (Vol: {}% | Loss: {:.1}% | BT Delay: {}ms)",
                                badge,
                                client.addr,
                                client.client_volume,
                                (client.fec_rate_permille as f32) / 10.0,
                                client.bluetooth_delay_ms
                            ));
                            let mute_label = if client.muted { "Unmute" } else { "Mute" };
                            if ui.button(mute_label).clicked() {
                                host.set_client_muted(client.addr, !client.muted);
                            }
                            if ui.button("Kick").clicked() {
                                host.kick_client(client.addr);
                            }
                        });
                    }
                }
            } else {
                ui.label("Start the Host Server to view connected clients and telemetry.");
            }
        });

        ui.add_space(8.0);

        ui.group(|ui| {
            ui.heading("🛡️ Security, Per-App Audio Routing & Bidirectional Phone Mic");

            ui.horizontal(|ui| {
                if ui
                    .checkbox(
                        &mut self.enable_pin_encryption,
                        "🔒 ChaCha20 + MAC32 Encryption — PIN:",
                    )
                    .changed()
                {
                    if let Some(ref host) = self.host_engine {
                        let pin = if self.enable_pin_encryption {
                            Some(self.pairing_pin.as_str())
                        } else {
                            None
                        };
                        host.set_encryption_pin(pin);
                    }
                }
                ui.add_sized(
                    [80.0, 22.0],
                    egui::TextEdit::singleline(&mut self.pairing_pin),
                );
                if ui.button("Apply PIN").clicked() {
                    if let Some(ref host) = self.host_engine {
                        let pin = if self.enable_pin_encryption {
                            Some(self.pairing_pin.as_str())
                        } else {
                            None
                        };
                        host.set_encryption_pin(pin);
                    }
                }
            });

            ui.separator();
            ui.horizontal_wrapped(|ui| {
                ui.label("🎯 Per-App Capture:");
                if ui
                    .selectable_label(self.selected_app_idx.is_none(), "🌐 All System Audio")
                    .clicked()
                {
                    self.selected_app_idx = None;
                    self.app_route_status = restore_all_apps_to_default_sink();
                }
                if ui.button("🔄 Refresh Apps").clicked() {
                    self.audio_apps = list_active_audio_apps();
                }
                for (idx, app) in self.audio_apps.iter().enumerate() {
                    let selected = self.selected_app_idx == Some(idx);
                    if ui
                        .selectable_label(selected, format!("{} ({})", app.name, app.pid))
                        .clicked()
                    {
                        self.selected_app_idx = Some(idx);
                        match route_single_app_to_audioshare(app) {
                            Ok(msg) => self.app_route_status = msg,
                            Err(err) => self.app_route_status = err,
                        }
                    }
                }
            });
            ui.monospace(&self.app_route_status);

            ui.separator();
            ui.horizontal_wrapped(|ui| {
                ui.label("📲 Phone -> PC Mode:");
                let mut mode_changed = false;
                mode_changed |= ui
                    .selectable_value(
                        &mut self.phone_rx_mode,
                        PhoneRxRoutingMode::VirtualMicForCalls,
                        "🎙️ Virtual Mic (Zoom / Discord / Meet)",
                    )
                    .changed();
                mode_changed |= ui
                    .selectable_value(
                        &mut self.phone_rx_mode,
                        PhoneRxRoutingMode::PlayOnPcSpeakers,
                        "🔊 Play Phone Audio on PC Speakers",
                    )
                    .changed();

                // Hot-swap routing mode immediately if receiver is already active!
                if mode_changed && self.phone_mic_receiver.is_some() {
                    if let Some(mut m) = self.phone_mic_receiver.take() {
                        m.stop();
                    }
                    self.phone_mic_receiver =
                        PhoneMicReceiver::start_with_mode(PHONE_MIC_PORT, self.phone_rx_mode).ok();
                }

                let mic_active = self.phone_mic_receiver.is_some();
                let btn_label = if mic_active {
                    "⏹ Stop Phone Receiver"
                } else {
                    "▶ Start Phone Receiver (Port 65532)"
                };
                if ui.button(btn_label).clicked() {
                    if let Some(mut m) = self.phone_mic_receiver.take() {
                        m.stop();
                    } else if let Ok(rx) =
                        PhoneMicReceiver::start_with_mode(PHONE_MIC_PORT, self.phone_rx_mode)
                    {
                        self.phone_mic_receiver = Some(rx);
                    }
                }

                if let Some(ref mic) = self.phone_mic_receiver {
                    let pkts = mic.mic_packets_received.load(Ordering::Relaxed);
                    let peak = mic.mic_peak_level_pct.load(Ordering::Relaxed);
                    ui.monospace(format!("RX Phone Pkts: {} | Level: {}%", pkts, peak));

                    if let Some(ref host) = self.host_engine {
                        if ui.button("📡 Ping True Acoustic Latency (Sonar)").clicked() {
                            host.telemetry
                                .trigger_sonar_ping
                                .store(true, Ordering::Relaxed);
                        }
                        let tx_us = host.telemetry.sonar_tx_timestamp_us.load(Ordering::Relaxed);
                        let rx_us = mic.last_chirp_rx_timestamp_us.load(Ordering::Relaxed);
                        if rx_us > tx_us && (rx_us - tx_us) < 500_000 {
                            self.measured_sonar_ms = Some(((rx_us - tx_us) as f32) / 2000.0);
                            self.measured_sonar_at = Some(Instant::now());
                        }
                    }
                }
                if let Some(ms) = self.measured_sonar_ms {
                    let age_s = self
                        .measured_sonar_at
                        .map(|t| t.elapsed().as_secs())
                        .unwrap_or(0);
                    ui.colored_label(
                        egui::Color32::LIGHT_GREEN,
                        format!("⚡ Measured One-Way Latency: {:.1} ms ({}s ago)", ms, age_s),
                    );
                }
            });
        });
    }

    fn render_latency_debug(&mut self, ui: &mut egui::Ui) {
        ui.label(
            egui::RichText::new(
                "Live values are published by the audio threads. The estimate is not an acoustic measurement.",
            )
            .small()
            .color(egui::Color32::GRAY),
        );
        ui.separator();

        let sonar_ms = self.measured_sonar_ms;
        let sonar_age_s = self.measured_sonar_at.map(|t| t.elapsed().as_secs_f32());
        let mut any_engine = false;

        if let Some(ref host) = self.host_engine {
            any_engine = true;
            ui.heading("Host: capture → wire");
            let pacing_ms =
                host.telemetry.tx_packet_interval_us.load(Ordering::Relaxed) as f32 / 1000.0;
            let capture_ms =
                host.telemetry.capture_buffer_us.load(Ordering::Relaxed) as f32 / 1000.0;
            let subframes = host
                .telemetry
                .tx_subframes_per_packet
                .load(Ordering::Relaxed);
            let codec = codec_mode_name(host.telemetry.active_codec_mode.load(Ordering::Relaxed));
            let clients = host.connected_clients();
            let bt_max = host
                .telemetry
                .max_client_bt_delay_ms
                .load(Ordering::Relaxed);
            let downstream_max = host
                .telemetry
                .max_client_downstream_delay_ms
                .load(Ordering::Relaxed);
            let room_trim = host
                .telemetry
                .manual_group_sync_trim_ms
                .load(Ordering::Relaxed);

            egui::Grid::new("host_latency_grid")
                .num_columns(2)
                .spacing([16.0, 4.0])
                .show(ui, |ui| {
                    ui.label("Packetization");
                    ui.monospace(format!(
                        "{:.1} ms ({} × {:.1} ms, {})",
                        pacing_ms, subframes, SUBFRAME_MS, codec
                    ));
                    ui.end_row();

                    ui.label("Capture backlog");
                    ui.monospace(format!("{:.1} ms", capture_ms));
                    ui.end_row();

                    ui.label("Host pipeline");
                    ui.monospace(format!("{:.1} ms", capture_ms + pacing_ms));
                    ui.end_row();

                    ui.label("Connected clients");
                    ui.monospace(format!("{}", clients.len()));
                    ui.end_row();

                    ui.label("Downstream max");
                    ui.monospace(format!("{} ms (BT max {} ms)", downstream_max, bt_max));
                    ui.end_row();

                    ui.label("Room trim");
                    ui.monospace(format!("+{} ms", room_trim));
                    ui.end_row();

                    ui.label("Acoustic sonar");
                    match (sonar_ms, sonar_age_s) {
                        (Some(ms), Some(age)) => {
                            ui.monospace(format!("{:.1} ms ({:.0}s ago)", ms, age));
                        }
                        (Some(ms), None) => {
                            ui.monospace(format!("{:.1} ms", ms));
                        }
                        _ => {
                            ui.label("not measured");
                        }
                    }
                    ui.end_row();
                });

            if clients.is_empty() {
                ui.label("No clients connected.");
            } else {
                for c in &clients {
                    let badge = if c.is_usb { "⚡ USB" } else { "📶 Wi-Fi" };
                    ui.monospace(format!(
                        "{} {} | Loss {:.1}% | BT {} ms | DAC {} ms | Vol {}%{}",
                        badge,
                        c.addr,
                        (c.fec_rate_permille as f32) / 10.0,
                        c.bluetooth_delay_ms,
                        c.hardware_delay_ms,
                        c.client_volume,
                        if c.muted { " | MUTED" } else { "" }
                    ));
                }
            }
        }

        if any_engine {
            ui.separator();
        }

        if let Some(ref client) = self.client_engine {
            any_engine = true;
            let snap = client.telemetry.latency_snapshot(DESKTOP_OUTPUT_BUFFER_MS);

            ui.heading("Client: ingress → speaker");
            ui.monospace(
                egui::RichText::new(format!(
                    "Estimated one-way: {:.1} ms",
                    snap.estimated_one_way_ms()
                ))
                .strong()
                .color(egui::Color32::LIGHT_GREEN),
            );

            egui::Grid::new("client_latency_grid")
                .num_columns(2)
                .spacing([16.0, 4.0])
                .show(ui, |ui| {
                    ui.label("Sender packetization");
                    ui.monospace(format!("{:.1} ms", snap.packet_interval_ms));
                    ui.end_row();

                    ui.label("Network jitter");
                    ui.monospace(format!("{:.1} ms (sizes the FIFO)", snap.network_jitter_ms));
                    ui.end_row();

                    ui.label("Adaptive cushion");
                    ui.monospace(format!("{:.1} ms", snap.adaptive_buffer_ms));
                    ui.end_row();

                    ui.label("Target FIFO");
                    ui.monospace(format!("{:.1} ms", snap.target_buffer_ms));
                    ui.end_row();

                    ui.label("Group sync addition");
                    ui.monospace(format!("+{:.1} ms", snap.group_sync_extra_ms));
                    ui.end_row();

                    ui.label("Buffered right now");
                    ui.monospace(format!("{:.1} ms", snap.buffered_audio_ms));
                    ui.end_row();

                    ui.label("Downstream stage");
                    ui.monospace(format!("{:.1} ms", snap.downstream_stage_ms()));
                    ui.end_row();

                    ui.label("DAC queue");
                    if snap.hardware_delay_ms > 0 {
                        ui.monospace(format!("{} ms (measured)", snap.hardware_delay_ms));
                    } else {
                        ui.label("not measured on this platform");
                    }
                    ui.end_row();

                    ui.label("Bluetooth estimate");
                    if snap.bluetooth_delay_ms > 0 {
                        ui.monospace(format!(
                            "{} ms (fixed A2DP estimate)",
                            snap.bluetooth_delay_ms
                        ));
                    } else {
                        ui.label("not routed");
                    }
                    ui.end_row();

                    ui.label("Desktop output buffer");
                    ui.monospace(format!("{:.1} ms (configured)", snap.output_buffer_ms));
                    ui.end_row();

                    ui.label("Prebuffer");
                    ui.monospace(if snap.prebuffer_ready {
                        "READY"
                    } else {
                        "FILLING"
                    });
                    ui.end_row();

                    ui.label("Codec");
                    ui.monospace(snap.codec_name());
                    ui.end_row();
                });

            let fill = if snap.target_buffer_ms > 0.0 {
                (snap.buffered_audio_ms / snap.target_buffer_ms).clamp(0.0, 1.0)
            } else {
                0.0
            };
            ui.add(egui::ProgressBar::new(fill).text(format!(
                "FIFO fill: {:.1} / {:.1} ms",
                snap.buffered_audio_ms, snap.target_buffer_ms
            )));

            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(
                    "Estimate = packetization + target FIFO + max(DAC, BT, desktop output). \
                     Group sync is already inside the target FIFO. Excludes one-way network \
                     transit and the acoustic air path; use the sonar for end-to-end.",
                )
                .small()
                .color(egui::Color32::GRAY),
            );
        }

        if !any_engine {
            ui.label("Start the Host Server or connect to a Client to see live latency.");
        }
    }

    fn render_client_tab(&mut self, ui: &mut egui::Ui) {
        ui.group(|ui| {
            ui.heading("1-Tap Auto-Discovered Hosts (LAN & USB)");
            let discovered = self.scanner.discovered_hosts();
            if discovered.is_empty() {
                ui.label("Scanning LAN & USB for AudioShare Ultra beacons...");
            } else {
                for host in discovered {
                    ui.horizontal(|ui| {
                        let badge = if host.is_usb { "⚡ USB" } else { "📶 Wi-Fi" };
                        ui.monospace(format!("{} {} ({})", badge, host.hostname, host.addr));
                        if ui.button("Connect Now").clicked() {
                            self.usb_buffer_mode = host.is_usb;
                            self.connect_client(host.addr);
                        }
                    });
                }
            }

            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Manual Host IP:Port:");
                ui.text_edit_singleline(&mut self.manual_server_ip);
                if ui
                    .checkbox(&mut self.gaming_hard_lock, "🎮 Gaming 5ms Hard-Lock")
                    .changed()
                {
                    if let Some(ref c) = self.client_engine {
                        self.sync_client_dsp_atomics(c);
                    }
                }
                if ui.button("Connect").clicked() {
                    if let Ok(addr) = self.manual_server_ip.parse::<SocketAddr>() {
                        // The scanner sets this for discovered hosts. Manual
                        // entry, QR URIs and saved addresses bypass that path,
                        // so detect USB tethering from the destination address.
                        self.usb_buffer_mode = is_usb_subnet_ip(addr.ip());
                        self.connect_client(addr);
                    } else if let Ok(addr) =
                        format!("{}:{}", self.manual_server_ip, DEFAULT_PORT).parse::<SocketAddr>()
                    {
                        self.usb_buffer_mode = is_usb_subnet_ip(addr.ip());
                        self.connect_client(addr);
                    }
                }
                if self.client_engine.is_some() && ui.button("Disconnect").clicked() {
                    if let Some(mut c) = self.client_engine.take() {
                        c.stop();
                    }
                    self.client_status_msg = "Disconnected.".into();
                }
            });
            ui.label(&self.client_status_msg);
        });

        ui.add_space(8.0);

        // Studio Pre-Amp, 3-Band Biquad EQ, Spatial Role & Binaural Crossfeed (0.00ms Delay!)
        ui.group(|ui| {
            ui.heading("🎛️ Zero-Latency Studio DSP (3-Band EQ, Spatial Role & Binaural Crossfeed)");

            let mut dsp_changed = false;
            // 1-Click Studio EQ Presets
            ui.horizontal_wrapped(|ui| {
                ui.label("✨ 1-Click EQ Presets:");
                for (name, b, p, t) in [
                    ("Flat (Studio)", 0.0, 0.0, 0.0),
                    ("🔥 Bass Boost", 6.5, 0.0, 1.5),
                    ("🎙️ Vocal / Podcast", -2.0, 5.5, 2.0),
                    ("🎮 FPS Footsteps", -3.0, 6.0, 4.5),
                    ("🎧 Harman Target", 4.5, 1.5, 3.0),
                ] {
                    if ui.button(name).clicked() {
                        self.eq_bass_db = b;
                        self.eq_presence_db = p;
                        self.eq_treble_db = t;
                        dsp_changed = true;
                    }
                }
            });
            ui.separator();

            ui.horizontal(|ui| {
                ui.label("Pre-Amp Gain   :");
                dsp_changed |= ui
                    .add(egui::Slider::new(&mut self.preamp_gain_db, -12.0..=24.0).suffix(" dB"))
                    .changed();
                ui.label("Bass (90 Hz)   :");
                dsp_changed |= ui
                    .add(egui::Slider::new(&mut self.eq_bass_db, -12.0..=12.0).suffix(" dB"))
                    .changed();
            });

            ui.horizontal(|ui| {
                ui.label("Vocal (2.5 kHz):");
                dsp_changed |= ui
                    .add(egui::Slider::new(&mut self.eq_presence_db, -12.0..=12.0).suffix(" dB"))
                    .changed();
                ui.label("Air (10 kHz)   :");
                dsp_changed |= ui
                    .add(egui::Slider::new(&mut self.eq_treble_db, -12.0..=12.0).suffix(" dB"))
                    .changed();
                if ui.button("Flat Reset").clicked() {
                    self.preamp_gain_db = 0.0;
                    self.eq_bass_db = 0.0;
                    self.eq_presence_db = 0.0;
                    self.eq_treble_db = 0.0;
                    dsp_changed = true;
                }
            });

            ui.separator();
            ui.horizontal_wrapped(|ui| {
                ui.label("🔊 Spatial Channel Role:");
                for (role, label) in [
                    (ChannelRole::Stereo, "Stereo"),
                    (ChannelRole::LeftMono, "Left Speaker"),
                    (ChannelRole::RightMono, "Right Speaker"),
                    (ChannelRole::CenterDialogue, "Center Dialogue"),
                    (ChannelRole::SurroundAmbient, "Rear Surround"),
                ] {
                    if ui
                        .selectable_value(&mut self.channel_role, role, label)
                        .changed()
                    {
                        dsp_changed = true;
                    }
                }
            });

            ui.horizontal(|ui| {
                dsp_changed |= ui
                    .checkbox(
                        &mut self.binaural_crossfeed,
                        "🎧 Audiophile Bauer Binaural Headphone Crossfeed (0.00ms delay, 270µs ITD)",
                    )
                    .changed();
                dsp_changed |= ui
                    .checkbox(
                        &mut self.night_mode_drc,
                        "🌙 Night Mode (Boost Whispers/Footsteps +11.5dB, Tame Explosions)",
                    )
                    .changed();
            });

            if dsp_changed {
                if let Some(ref c) = self.client_engine {
                    self.sync_client_dsp_atomics(c);
                }
            }
        });

        ui.add_space(8.0);

        ui.group(|ui| {
            ui.heading("Real-Time WSOLA Sequencer & Clock Drift Telemetry");
            if let Some(ref client) = self.client_engine {
                let pkts = client.telemetry.packets_received.load(Ordering::Relaxed);
                let prim = client
                    .telemetry
                    .primary_frames_played
                    .load(Ordering::Relaxed);
                let fec = client.telemetry.fec_healed_frames.load(Ordering::Relaxed);
                let plc = client
                    .telemetry
                    .plc_extrapolated_frames
                    .load(Ordering::Relaxed);
                let auth_err = client.telemetry.auth_failures.load(Ordering::Relaxed);
                let drift = client.telemetry.clock_drift_ppm.load(Ordering::Relaxed);
                let dtx = client.telemetry.server_dtx_silent.load(Ordering::Relaxed);

                self.drift_history.pop_front();
                self.drift_history.push_back(drift as f32);

                let host_vol = client.telemetry.host_volume.load(Ordering::Relaxed);
                let group_ms = client.telemetry.max_group_delay_ms.load(Ordering::Relaxed);
                let idle_secs = client
                    .telemetry
                    .seconds_since_last_packet
                    .load(Ordering::Relaxed);

                if idle_secs >= 3 {
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        format!(
                            "⚠️ No packets from Host for {}s — Waiting for automatic reconnect...",
                            idle_secs
                        ),
                    );
                }
                ui.monospace(format!(
                    "Host Master Vol: {:<9}% | Multi-Phone Group Sync Delay: +{} ms",
                    host_vol, group_ms
                ));

                ui.monospace(format!(
                    "RX Packets     : {:<10} | Lossless Subframes : {}",
                    pkts, prim
                ));
                ui.monospace(format!(
                    "FEC Healed     : {:<10} | WSOLA PLC Frames   : {}",
                    fec, plc
                ));
                ui.monospace(format!(
                    "Clock Lock PPM : {:+4} ppm   | Auth Rejections    : {}",
                    drift, auth_err
                ));
                ui.monospace(format!(
                    "Bluetooth KeepAliveState   : {}",
                    if dtx {
                        "ACTIVE (-90 dBFS Dither)"
                    } else {
                        "STREAMING LIVE AUDIO"
                    }
                ));

                // Render 60 FPS Real-Time Clock-Drift Sparkline Graph (-500 ppm .. +500 ppm)
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 44.0),
                    egui::Sense::hover(),
                );
                let painter = ui.painter();
                painter.rect_filled(rect, 4.0, egui::Color32::from_gray(20));
                let mid_y = rect.center().y;
                painter.line_segment(
                    [
                        egui::pos2(rect.left(), mid_y),
                        egui::pos2(rect.right(), mid_y),
                    ],
                    egui::Stroke::new(1.0f32, egui::Color32::DARK_GRAY),
                );
                let step_x = rect.width() / (self.drift_history.len().max(2) as f32 - 1.0);
                let points: Vec<egui::Pos2> = self
                    .drift_history
                    .iter()
                    .enumerate()
                    .map(|(i, &ppm)| {
                        let x = rect.left() + (i as f32) * step_x;
                        let norm_y = (ppm / 500.0).clamp(-1.0, 1.0);
                        egui::pos2(x, mid_y - norm_y * (rect.height() * 0.42))
                    })
                    .collect();
                painter.add(egui::Shape::line(
                    points,
                    egui::Stroke::new(1.5f32, egui::Color32::LIGHT_GREEN),
                ));
            } else {
                ui.label("Connect to a Host to view real-time jitter & clock recovery telemetry.");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lipsync_hysteresis() {
        assert!(!should_apply_lipsync(0, 0));
        assert!(!should_apply_lipsync(50, 50));
        assert!(
            should_apply_lipsync(5, 0),
            "the first non-zero report must apply"
        );
        assert!(
            should_apply_lipsync(50, 0),
            "the first report after startup must apply"
        );
        assert!(!should_apply_lipsync(55, 50));
        assert!(!should_apply_lipsync(59, 50));
        assert!(should_apply_lipsync(60, 50));
        assert!(
            should_apply_lipsync(0, 50),
            "returning to zero must always apply"
        );
        assert!(
            should_apply_lipsync(5, 60),
            "a large downward change must apply"
        );
    }

    #[test]
    fn test_desktop_config_string_parser() {
        let mut app = AudioShareApp::new();
        let config_sample = "\
dark_mode=false
minimize_on_close=true
autostart_stream_on_launch=true
os_boot_autostart=false
hotspot_ssid=AudioShare_Direct
hotspot_pass=pass1234
quality_profile=4
target_bitrate_kbps=320
usb_low_latency_pacing=true
enable_smart_dtx=false
enable_virtual_speaker=true
auto_bt_video_lipsync=true
host_master_volume=75
enable_pin_encryption=true
pairing_pin=9999
manual_server_ip=192.168.1.50:65530
preamp_gain_db=6.5
eq_bass_db=3.0
eq_presence_db=-2.0
eq_treble_db=4.5
channel_role=1
binaural_crossfeed=true
night_mode_drc=true
latency_debug=true
gaming_hard_lock=true
enable_group_sync=false
group_sync_trim_ms=45
phone_rx_mode=1
";
        app.parse_config_string(config_sample);
        assert!(!app.dark_mode);
        assert!(app.minimize_on_close);
        assert!(app.autostart_stream_on_launch);
        assert!(!app.os_boot_autostart);
        assert_eq!(app.hotspot_ssid, "AudioShare_Direct");
        assert_eq!(app.hotspot_pass, "pass1234");
        assert_eq!(app.quality_profile, QualityProfile::StudioLossless24Bit);
        assert_eq!(app.target_bitrate_kbps, 320);
        assert!(app.usb_low_latency_pacing);
        assert!(!app.enable_smart_dtx);
        assert!(app.enable_virtual_speaker);
        assert!(!app.enable_group_sync);
        assert_eq!(app.group_sync_trim_ms, 45);
        assert_eq!(app.host_master_volume, 75);
        assert!(app.enable_pin_encryption);
        assert_eq!(app.pairing_pin, "9999");
        assert_eq!(app.manual_server_ip, "192.168.1.50:65530");
        assert!((app.preamp_gain_db - 6.5).abs() < 1e-4);
        assert!((app.eq_bass_db - 3.0).abs() < 1e-4);
        assert!((app.eq_presence_db - (-2.0)).abs() < 1e-4);
        assert!((app.eq_treble_db - 4.5).abs() < 1e-4);
        assert_eq!(app.channel_role, ChannelRole::LeftMono);
        assert!(app.binaural_crossfeed);
        assert!(app.night_mode_drc);
        assert!(app.latency_debug);
        assert!(app.gaming_hard_lock);
        assert_eq!(app.phone_rx_mode, PhoneRxRoutingMode::PlayOnPcSpeakers);
    }
}
