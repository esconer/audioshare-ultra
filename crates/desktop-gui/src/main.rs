#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::Duration;

use audioshare_client_engine::{ClientConfig, ClientEngine};
use audioshare_host_engine::app_routing::{
    list_active_audio_apps, route_single_app_to_audioshare, AudioAppEntry,
};
use audioshare_host_engine::bt_optimization::apply_automatic_video_lipsync_offset_ms;
use audioshare_host_engine::discovery_and_os::{
    AdbUsbWatcher, DirectHotspotController, DiscoveryBeacon, DiscoveryScanner,
    VirtualSpeakerAndRadioGuard,
};
use audioshare_host_engine::virtual_mic::{PhoneMicReceiver, PHONE_MIC_PORT};
use audioshare_host_engine::{
    list_valid_host_interfaces, HostConfig, HostEngine, NetworkInterfaceInfo, DEFAULT_PORT,
};
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
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(AudioShareApp::new()))
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

    // Host State
    host_engine: Option<HostEngine>,
    beacon: Option<DiscoveryBeacon>,
    adb_watcher: Option<AdbUsbWatcher>,
    os_guard: Option<VirtualSpeakerAndRadioGuard>,
    usb_low_latency_pacing: bool,
    enable_smart_dtx: bool,
    enable_virtual_speaker: bool,
    auto_bt_video_lipsync: bool,
    last_applied_bt_delay: u16,
    use_test_tone: bool,
    host_status_msg: String,

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
    gaming_hard_lock: bool,
    usb_buffer_mode: bool,
    drift_history: VecDeque<f32>,
    client_status_msg: String,
}

impl AudioShareApp {
    fn new() -> Self {
        Self {
            tab: AppTab::HostSender,
            interfaces: list_valid_host_interfaces(),
            show_qr_modal: false,
            host_engine: None,
            beacon: None,
            adb_watcher: None,
            os_guard: None,
            usb_low_latency_pacing: false,
            enable_smart_dtx: true,
            enable_virtual_speaker: false,
            auto_bt_video_lipsync: true,
            last_applied_bt_delay: 0,
            use_test_tone: false,
            host_status_msg: "Stopped. Click 'Start Host Server' to broadcast.".into(),
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
            scanner: DiscoveryScanner::start(),
            client_engine: None,
            manual_server_ip: format!("127.0.0.1:{}", DEFAULT_PORT),
            preamp_gain_db: 0.0,
            eq_bass_db: 0.0,
            eq_presence_db: 0.0,
            eq_treble_db: 0.0,
            channel_role: ChannelRole::Stereo,
            binaural_crossfeed: false,
            gaming_hard_lock: false,
            usb_buffer_mode: false,
            drift_history: VecDeque::from(vec![0.0; 120]),
            client_status_msg: "Idle. Select a discovered host or enter IP.".into(),
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

            match HostEngine::start(cfg, vec![]) {
                Ok(engine) => {
                    if self.enable_pin_encryption {
                        engine.set_encryption_pin(Some(&self.pairing_pin));
                    }
                    self.os_guard =
                        Some(VirtualSpeakerAndRadioGuard::acquire(self.enable_virtual_speaker));
                    self.beacon = Some(DiscoveryBeacon::start(
                        DEFAULT_PORT,
                        "PC-AudioShare-Ultra".into(),
                    ));
                    self.adb_watcher = Some(AdbUsbWatcher::start(DEFAULT_PORT));
                    self.host_engine = Some(engine);
                    self.host_status_msg =
                        "Broadcasting 48kHz Studio Lossless + [N-1]/[N-2] FEC (802.11e AC_VO)".into();
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
                    let cell =
                        egui::Rect::from_min_size(min, egui::vec2(module_px, module_px));
                    painter.rect_filled(cell, 0.0, egui::Color32::BLACK);
                }
            }
        }
    }
}

impl eframe::App for AudioShareApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(100));

        egui::TopBottomPanel::top("top_nav").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("🎧 AudioShare Ultra");
                ui.separator();
                ui.selectable_value(&mut self.tab, AppTab::HostSender, "📡 Host Server (Send Audio)");
                ui.selectable_value(
                    &mut self.tab,
                    AppTab::ClientReceiver,
                    "🔊 Client Receiver & Studio DSP",
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
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
                        if ui.button("Close QR Window").clicked() {
                            self.show_qr_modal = false;
                        }
                    });
                });
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
                AppTab::HostSender => self.render_host_tab(ui),
                AppTab::ClientReceiver => self.render_client_tab(ui),
            });
        });
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
                ui.label(egui::RichText::new(&self.host_status_msg).color(if is_running {
                    egui::Color32::LIGHT_GREEN
                } else {
                    egui::Color32::GRAY
                }));
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
                ui.monospace(format!("{} {}:{} ({})", tag, iface.ip, DEFAULT_PORT, iface.name));
            }
            ui.separator();
            ui.horizontal(|ui| {
                let hs_btn = if self.direct_hotspot_active {
                    "Stop 1-Hop Direct Hotspot"
                } else {
                    "📡 Start 1-Hop 5GHz Direct Link (Bypass Router)"
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

                // Automatically adjust PipeWire sink latency offset when phone Bluetooth delay changes!
                if self.auto_bt_video_lipsync && bt_delay != self.last_applied_bt_delay {
                    apply_automatic_video_lipsync_offset_ms(bt_delay);
                    self.last_applied_bt_delay = bt_delay;
                }

                let mode_str = if dtx {
                    "SMART-DTX SILENCE (0.6 kbps)"
                } else if mode_id == 2 {
                    "GOVERNOR: COMPRESSED LOW-DELAY (Weak Wi-Fi Protection)"
                } else {
                    "GOVERNOR: STUDIO LOSSLESS LPC + [N-1]/[N-2] FEC"
                };

                ui.monospace(format!(
                    "TX Pkts: {} | KB: {} | Hot-Plug Recoveries: {} | Mode: {}",
                    pkts, kb, rebinds, mode_str
                ));

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
                                "{} {} (Loss: {:.1}% | BT Delay: {}ms)",
                                badge,
                                client.addr,
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
            ui.horizontal(|ui| {
                let mic_active = self.phone_mic_receiver.is_some();
                let btn_label = if mic_active {
                    "🎙️ Stop Phone-to-PC Audio/Mic Receiver"
                } else {
                    "🎙️ Enable Phone-to-PC Audio/Mic Receiver (Port 65532)"
                };
                if ui.button(btn_label).clicked() {
                    if let Some(mut m) = self.phone_mic_receiver.take() {
                        m.stop();
                    } else if let Ok(rx) = PhoneMicReceiver::start(PHONE_MIC_PORT) {
                        self.phone_mic_receiver = Some(rx);
                    }
                }

                if let Some(ref mic) = self.phone_mic_receiver {
                    let pkts = mic.mic_packets_received.load(Ordering::Relaxed);
                    let peak = mic.mic_peak_level_pct.load(Ordering::Relaxed);
                    ui.monospace(format!("RX Phone Pkts: {} | Level: {}%", pkts, peak));
                }
            });
        });
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
                        self.connect_client(addr);
                    } else if let Ok(addr) =
                        format!("{}:{}", self.manual_server_ip, DEFAULT_PORT).parse::<SocketAddr>()
                    {
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
                let prim = client.telemetry.primary_frames_played.load(Ordering::Relaxed);
                let fec = client.telemetry.fec_healed_frames.load(Ordering::Relaxed);
                let plc = client.telemetry.plc_extrapolated_frames.load(Ordering::Relaxed);
                let auth_err = client.telemetry.auth_failures.load(Ordering::Relaxed);
                let drift = client.telemetry.clock_drift_ppm.load(Ordering::Relaxed);
                let dtx = client.telemetry.server_dtx_silent.load(Ordering::Relaxed);

                self.drift_history.pop_front();
                self.drift_history.push_back(drift as f32);

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
            } else {
                ui.label("Connect to a Host to view real-time jitter & clock recovery telemetry.");
            }
        });
    }
}
