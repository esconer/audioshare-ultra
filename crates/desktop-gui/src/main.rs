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
            .with_inner_size([920.0, 740.0])
            .with_min_inner_size([760.0, 560.0])
            .with_icon(build_studio_window_icon())
            .with_title("AudioShare Ultra — Studio Audio Routing Mesh"),
        ..Default::default()
    };

    eframe::run_native(
        "AudioShare Ultra",
        options,
        Box::new(|cc| {
            let app = AudioShareApp::new();
            AudioShareApp::apply_studio_theme(&cc.egui_ctx, app.dark_mode);
            Ok(Box::new(app))
        }),
    )
}

// ---------------------------------------------------------------------------
// Window / taskbar icon: "Bidirectional Studio Bridge" (Option C)
// ---------------------------------------------------------------------------

/// Brand palette shared by the master artwork, Android adaptive icon, and the
/// zero-dependency desktop window icon.
const ICON_BG_TOP: (u8, u8, u8) = (0x11, 0x18, 0x27);
const ICON_BG_BOTTOM: (u8, u8, u8) = (0x09, 0x0D, 0x16);
const ICON_BORDER: (u8, u8, u8) = (0x24, 0x30, 0x47);
const ICON_CYAN: (u8, u8, u8) = (0x38, 0xBD, 0xF8);
const ICON_BLUE: (u8, u8, u8) = (0x25, 0x63, 0xEB);
const ICON_EMERALD: (u8, u8, u8) = (0x34, 0xD3, 0x99);
const ICON_GREEN: (u8, u8, u8) = (0x05, 0x96, 0x69);
const ICON_WHITE: (u8, u8, u8) = (0xFF, 0xFF, 0xFF);

/// Supersampling factor for the 64x64 window icon.
const ICON_SUPERSAMPLE: usize = 4;

/// Rasterizes the primary "Bidirectional Studio Bridge" mark (PC <-> Phone) as
/// RGBA data for the window and taskbar icon. The 512-unit master artwork is
/// sampled at 4x and box-filtered down, so the arcs and stereo bars stay smooth
/// at 64 px without adding an image or SVG dependency.
fn build_studio_window_icon() -> egui::IconData {
    const SIZE: usize = 64;
    const MASTER: f32 = 512.0;
    const STEP: f32 = MASTER / (SIZE * ICON_SUPERSAMPLE) as f32;

    let mut rgba = vec![0u8; SIZE * SIZE * 4];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let (mut sum_r, mut sum_g, mut sum_b, mut covered) = (0u32, 0u32, 0u32, 0u32);
            for sy in 0..ICON_SUPERSAMPLE {
                for sx in 0..ICON_SUPERSAMPLE {
                    let mx = (x * ICON_SUPERSAMPLE + sx) as f32 * STEP + STEP * 0.5;
                    let my = (y * ICON_SUPERSAMPLE + sy) as f32 * STEP + STEP * 0.5;
                    if let Some((r, g, b)) = studio_bridge_pixel(mx, my) {
                        sum_r += u32::from(r);
                        sum_g += u32::from(g);
                        sum_b += u32::from(b);
                        covered += 1;
                    }
                }
            }

            if let (Some(r), Some(g), Some(b)) = (
                sum_r.checked_div(covered),
                sum_g.checked_div(covered),
                sum_b.checked_div(covered),
            ) {
                let idx = (y * SIZE + x) * 4;
                rgba[idx] = r as u8;
                rgba[idx + 1] = g as u8;
                rgba[idx + 2] = b as u8;
                rgba[idx + 3] =
                    ((covered * 255) / (ICON_SUPERSAMPLE * ICON_SUPERSAMPLE) as u32) as u8;
            }
        }
    }

    egui::IconData {
        rgba,
        width: SIZE as u32,
        height: SIZE as u32,
    }
}

/// Samples one point of the 512-unit master artwork. Returns `None` outside the
/// squircle so the icon keeps transparent corners on every desktop.
fn studio_bridge_pixel(x: f32, y: f32) -> Option<(u8, u8, u8)> {
    // Rounded-rect squircle: 16..496 with a 112-unit corner radius.
    let dx = x - 256.0;
    let dy = y - 256.0;
    let qx = dx.abs() - 128.0;
    let qy = dy.abs() - 128.0;
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    let edge = qx.max(qy).min(0.0) + outside - 112.0;
    if edge > 0.0 {
        return None;
    }

    // Obsidian diagonal gradient plus a 6-unit inner rim.
    let t = ((x + y) / 1024.0).clamp(0.0, 1.0);
    let mut color = lerp_rgb(ICON_BG_TOP, ICON_BG_BOTTOM, t);
    if edge > -6.0 {
        color = ICON_BORDER;
    }

    // Left transmission arc: center (220, 256), radius 140, 28-unit stroke,
    // open to the right. Round caps extend beyond both endpoints.
    let left_dx = x - 220.0;
    let left_dist = (left_dx * left_dx + (y - 256.0).powi(2)).sqrt();
    let left_ring = (left_dist - 140.0).abs() <= 14.0 && left_dx <= 0.0;
    let left_cap = (x - 220.0).powi(2) + (y - 116.0).powi(2) <= 14.0 * 14.0
        || (x - 220.0).powi(2) + (y - 396.0).powi(2) <= 14.0 * 14.0;
    if left_ring || left_cap {
        let g = gradient_t(x, y, 96.0, 128.0, 320.0, 384.0);
        color = lerp_rgb(ICON_CYAN, ICON_BLUE, g);
    }

    // Right receiver arc: center (292, 256), mirror of the left one.
    let right_dx = x - 292.0;
    let right_dist = (right_dx * right_dx + (y - 256.0).powi(2)).sqrt();
    let right_ring = (right_dist - 140.0).abs() <= 14.0 && right_dx >= 0.0;
    let right_cap = (x - 292.0).powi(2) + (y - 116.0).powi(2) <= 14.0 * 14.0
        || (x - 292.0).powi(2) + (y - 396.0).powi(2) <= 14.0 * 14.0;
    if right_ring || right_cap {
        let g = gradient_t(x, y, 192.0, 128.0, 416.0, 384.0);
        color = lerp_rgb(ICON_EMERALD, ICON_GREEN, g);
    }

    // Three stereo lock bars; the middle one is the white lossless reference.
    if inside_rounded_rect(x, y, 192.0, 208.0, 24.0, 96.0, 12.0) {
        color = ICON_CYAN;
    }
    if inside_rounded_rect(x, y, 244.0, 164.0, 24.0, 184.0, 12.0) {
        color = ICON_WHITE;
    }
    if inside_rounded_rect(x, y, 296.0, 208.0, 24.0, 96.0, 12.0) {
        color = ICON_EMERALD;
    }

    Some(color)
}

fn gradient_t(px: f32, py: f32, x1: f32, y1: f32, x2: f32, y2: f32) -> f32 {
    let dx = x2 - x1;
    let dy = y2 - y1;
    (((px - x1) * dx + (py - y1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0)
}

fn lerp_rgb(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let mix =
        |from: u8, to: u8| (f32::from(from) + (f32::from(to) - f32::from(from)) * t).round() as u8;
    (mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

fn inside_rounded_rect(
    px: f32,
    py: f32,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    radius: f32,
) -> bool {
    let qx = (px - (x + width * 0.5)).abs() - (width * 0.5 - radius);
    let qy = (py - (y + height * 0.5)).abs() - (height * 0.5 - radius);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    qx.max(qy).min(0.0) + outside - radius <= 0.0
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum AppTab {
    HostSender,
    ClientReceiver,
}

struct AudioShareApp {
    tab: AppTab,
    interfaces: Vec<NetworkInterfaceInfo>,
    /// Index into `interfaces` used to build the pairing QR code. Defaults to
    /// the first Wi-Fi/LAN adapter instead of the USB-RNDIS link that
    /// `list_valid_host_interfaces()` sorts first.
    selected_qr_iface_idx: usize,
    show_qr_modal: bool,
    show_settings_modal: bool,
    dark_mode: bool,
    minimize_on_close: bool,
    autostart_stream_on_launch: bool,
    os_boot_autostart: bool,
    force_quit: bool,

    // Progressive Disclosure Drawer States ("Simple First, Complex When Needed")
    host_pro_codec_open: bool,
    host_routing_mic_open: bool,
    client_manual_ip_open: bool,
    client_eq_drawer_open: bool,
    client_telemetry_drawer_open: bool,

    // Host State
    host_engine: Option<HostEngine>,
    beacon: Option<DiscoveryBeacon>,
    adb_watcher: Option<AdbUsbWatcher>,
    os_guard: Option<VirtualSpeakerAndRadioGuard>,
    quality_profile: QualityProfile,
    prefer_opus: bool,
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

/// Default pairing adapter: the first Wi-Fi/Ethernet interface. The interface
/// list sorts USB tethering first so the Android host page can surface it, but
/// a QR code containing the RNDIS address is unreachable for a phone that is
/// not plugged in.
fn default_qr_iface_index(interfaces: &[NetworkInterfaceInfo]) -> usize {
    interfaces
        .iter()
        .position(|i| !i.is_usb_tethering)
        .unwrap_or(0)
}

/// Keep the operator's chosen adapter across a refresh, falling back to the
/// Wi-Fi/LAN default only when the old index no longer exists.
fn refreshed_qr_iface_index(interfaces: &[NetworkInterfaceInfo], selected: usize) -> usize {
    if selected < interfaces.len() {
        selected
    } else {
        default_qr_iface_index(interfaces)
    }
}

impl AudioShareApp {
    fn new() -> Self {
        let interfaces = list_valid_host_interfaces();
        let selected_qr_iface_idx = default_qr_iface_index(&interfaces);
        let mut app = Self {
            tab: AppTab::HostSender,
            interfaces,
            selected_qr_iface_idx,
            show_qr_modal: false,
            show_settings_modal: false,
            dark_mode: true,
            minimize_on_close: true,
            autostart_stream_on_launch: false,
            os_boot_autostart: false,
            force_quit: false,
            host_pro_codec_open: false,
            host_routing_mic_open: false,
            client_manual_ip_open: false,
            client_eq_drawer_open: false,
            client_telemetry_drawer_open: false,
            host_engine: None,
            beacon: None,
            adb_watcher: None,
            os_guard: None,
            quality_profile: QualityProfile::SmartAuto,
            prefer_opus: false,
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
            host_status_msg: "Ready to broadcast 48 kHz studio audio.".into(),
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
             prefer_opus={}\n\
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
            self.prefer_opus,
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
                    "prefer_opus" => self.prefer_opus = v.trim() == "true",
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
                        .prefer_opus
                        .store(self.prefer_opus, Ordering::Relaxed);
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
                        "Broadcasting 48 kHz Studio Audio + [N-1]/[N-2] FEC".into();
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
            dolby_effects: false,
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

    // =========================================================================
    // STUDIO DESIGN SYSTEM & CUSTOM VECTOR WIDGETS (Zero Broken Tofu Glyphs)
    // =========================================================================

    fn apply_studio_theme(ctx: &egui::Context, dark_mode: bool) {
        let mut style = (*ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.window_margin = egui::Margin::same(16.0);
        style.spacing.button_padding = egui::vec2(12.0, 7.0);
        style.visuals = if dark_mode {
            let mut v = egui::Visuals::dark();
            v.panel_fill = egui::Color32::from_rgb(10, 13, 19);
            v.window_fill = egui::Color32::from_rgb(16, 20, 29);
            v.faint_bg_color = egui::Color32::from_rgb(20, 25, 36);
            v.extreme_bg_color = egui::Color32::from_rgb(7, 9, 14);
            v.selection.bg_fill = egui::Color32::from_rgb(37, 99, 235);
            v.selection.stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(147, 197, 253));
            v.widgets.noninteractive.bg_stroke =
                egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(32, 40, 56));
            v.widgets.noninteractive.rounding = egui::Rounding::same(8.0);
            v.widgets.inactive.bg_fill = egui::Color32::from_rgb(24, 30, 43);
            v.widgets.inactive.weak_bg_fill = egui::Color32::from_rgb(22, 28, 40);
            v.widgets.inactive.bg_stroke =
                egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(42, 52, 70));
            v.widgets.inactive.rounding = egui::Rounding::same(7.0);
            v.widgets.hovered.bg_fill = egui::Color32::from_rgb(34, 43, 62);
            v.widgets.hovered.bg_stroke =
                egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(96, 165, 250));
            v.widgets.hovered.rounding = egui::Rounding::same(7.0);
            v.widgets.active.bg_fill = egui::Color32::from_rgb(37, 99, 235);
            v.widgets.active.rounding = egui::Rounding::same(7.0);
            v
        } else {
            let mut v = egui::Visuals::light();
            v.panel_fill = egui::Color32::from_rgb(241, 245, 249);
            v.window_fill = egui::Color32::from_rgb(255, 255, 255);
            v.selection.bg_fill = egui::Color32::from_rgb(37, 99, 235);
            v.widgets.noninteractive.rounding = egui::Rounding::same(8.0);
            v.widgets.inactive.rounding = egui::Rounding::same(7.0);
            v.widgets.hovered.rounding = egui::Rounding::same(7.0);
            v.widgets.active.rounding = egui::Rounding::same(7.0);
            v
        };
        ctx.set_style(style);
    }

    fn studio_card<R>(
        ui: &mut egui::Ui,
        dark_mode: bool,
        add_contents: impl FnOnce(&mut egui::Ui) -> R,
    ) -> egui::InnerResponse<R> {
        let fill = if dark_mode {
            egui::Color32::from_rgb(17, 22, 32)
        } else {
            egui::Color32::from_rgb(255, 255, 255)
        };
        let stroke = if dark_mode {
            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(33, 42, 59))
        } else {
            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(214, 222, 234))
        };
        egui::Frame::none()
            .fill(fill)
            .stroke(stroke)
            .rounding(egui::Rounding::same(12.0))
            .inner_margin(egui::Margin::same(16.0))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                add_contents(ui)
            })
    }

    fn status_pill(ui: &mut egui::Ui, text: &str, bg: egui::Color32, fg: egui::Color32) {
        egui::Frame::none()
            .fill(bg)
            .rounding(egui::Rounding::same(6.0))
            .inner_margin(egui::Margin::symmetric(9.0, 4.0))
            .show(ui, |ui| {
                ui.label(egui::RichText::new(text).color(fg).size(11.5).strong());
            });
    }

    fn metric_tile(ui: &mut egui::Ui, dark_mode: bool, label: &str, value: &str) {
        let fill = if dark_mode {
            egui::Color32::from_rgb(12, 16, 24)
        } else {
            egui::Color32::from_rgb(244, 247, 251)
        };
        let stroke = if dark_mode {
            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(28, 36, 52))
        } else {
            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(220, 226, 236))
        };
        egui::Frame::none()
            .fill(fill)
            .stroke(stroke)
            .rounding(egui::Rounding::same(8.0))
            .inner_margin(egui::Margin::symmetric(11.0, 8.0))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(
                        egui::RichText::new(label)
                            .size(11.0)
                            .color(egui::Color32::from_rgb(130, 144, 168)),
                    );
                    ui.monospace(egui::RichText::new(value).size(13.0).strong());
                });
            });
    }

    /// Custom-painted iOS / Sonar style animated pill switch
    fn pill_switch(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
        let desired_size = egui::vec2(38.0, 20.0);
        let (rect, mut response) = ui.allocate_exact_size(desired_size, egui::Sense::click());
        if response.clicked() {
            *on = !*on;
            response.mark_changed();
        }
        if ui.is_rect_visible(rect) {
            let how_on = ui.ctx().animate_bool(response.id, *on);
            let bg_off = egui::Color32::from_rgb(45, 55, 72);
            let bg_on = egui::Color32::from_rgb(37, 99, 235);
            let bg = egui::Color32::from_rgb(
                (bg_off.r() as f32 + (bg_on.r() as f32 - bg_off.r() as f32) * how_on) as u8,
                (bg_off.g() as f32 + (bg_on.g() as f32 - bg_off.g() as f32) * how_on) as u8,
                (bg_off.b() as f32 + (bg_on.b() as f32 - bg_off.b() as f32) * how_on) as u8,
            );
            let radius = 0.5 * rect.height();
            ui.painter().rect_filled(rect, radius, bg);
            let circle_x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), how_on);
            let center = egui::pos2(circle_x, rect.center().y);
            ui.painter()
                .circle_filled(center, radius - 2.5, egui::Color32::WHITE);
        }
        response
    }

    /// Clean two-line setting row with title + subtitle on the left and a pill switch on the right
    fn setting_switch_row(
        ui: &mut egui::Ui,
        dark_mode: bool,
        title: &str,
        subtitle: &str,
        value: &mut bool,
    ) -> bool {
        let mut changed = false;
        let row_bg = if dark_mode {
            egui::Color32::from_rgb(13, 17, 25)
        } else {
            egui::Color32::from_rgb(246, 248, 252)
        };
        let row_stroke = if dark_mode {
            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(28, 35, 50))
        } else {
            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(226, 232, 240))
        };
        egui::Frame::none()
            .fill(row_bg)
            .stroke(row_stroke)
            .rounding(egui::Rounding::same(8.0))
            .inner_margin(egui::Margin::symmetric(12.0, 8.0))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.5;
                        ui.label(egui::RichText::new(title).size(13.0).strong());
                        ui.label(
                            egui::RichText::new(subtitle)
                                .size(11.0)
                                .color(egui::Color32::from_rgb(130, 144, 168)),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if Self::pill_switch(ui, value).changed() {
                            changed = true;
                        }
                    });
                });
            });
        changed
    }

    /// Segmented capsule button with clear active/inactive borders and fills
    fn pill_choice(ui: &mut egui::Ui, dark_mode: bool, selected: bool, label: &str) -> bool {
        let (fill, stroke, text_col) = if selected {
            (
                egui::Color32::from_rgb(37, 99, 235),
                egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(96, 165, 250)),
                egui::Color32::WHITE,
            )
        } else if dark_mode {
            (
                egui::Color32::from_rgb(24, 30, 43),
                egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(44, 54, 74)),
                egui::Color32::from_rgb(203, 213, 225),
            )
        } else {
            (
                egui::Color32::from_rgb(238, 242, 248),
                egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(203, 213, 225)),
                egui::Color32::from_rgb(30, 41, 59),
            )
        };
        let btn = egui::Button::new(
            egui::RichText::new(label)
                .size(12.5)
                .strong()
                .color(text_col),
        )
        .fill(fill)
        .stroke(stroke)
        .rounding(egui::Rounding::same(7.0));
        ui.add(btn).clicked()
    }

    /// Tactile Mode Card for the 4 "Simple First" hero presets
    fn mode_preset_card(
        ui: &mut egui::Ui,
        dark_mode: bool,
        selected: bool,
        title: &str,
        subtitle: &str,
        badge: &str,
    ) -> bool {
        let (fill, stroke, title_col) = if selected {
            (
                if dark_mode {
                    egui::Color32::from_rgb(23, 37, 68)
                } else {
                    egui::Color32::from_rgb(219, 234, 254)
                },
                egui::Stroke::new(1.5_f32, egui::Color32::from_rgb(59, 130, 246)),
                if dark_mode {
                    egui::Color32::WHITE
                } else {
                    egui::Color32::from_rgb(30, 58, 138)
                },
            )
        } else if dark_mode {
            (
                egui::Color32::from_rgb(14, 18, 27),
                egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(34, 43, 60)),
                egui::Color32::from_rgb(226, 232, 240),
            )
        } else {
            (
                egui::Color32::from_rgb(248, 250, 252),
                egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(203, 213, 225)),
                egui::Color32::from_rgb(15, 23, 42),
            )
        };

        let resp = egui::Frame::none()
            .fill(fill)
            .stroke(stroke)
            .rounding(egui::Rounding::same(10.0))
            .inner_margin(egui::Margin::symmetric(12.0, 10.0))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    ui.horizontal(|ui| {
                        let (dot_rect, _) =
                            ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                        let dot_col = if selected {
                            egui::Color32::from_rgb(59, 130, 246)
                        } else {
                            egui::Color32::from_rgb(100, 116, 139)
                        };
                        ui.painter().circle_filled(dot_rect.center(), 4.0, dot_col);
                        ui.label(
                            egui::RichText::new(title)
                                .size(13.5)
                                .strong()
                                .color(title_col),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            Self::status_pill(
                                ui,
                                badge,
                                if selected {
                                    egui::Color32::from_rgb(37, 99, 235)
                                } else {
                                    egui::Color32::from_rgb(26, 33, 48)
                                },
                                if selected {
                                    egui::Color32::WHITE
                                } else {
                                    egui::Color32::from_rgb(148, 163, 184)
                                },
                            );
                        });
                    });
                    ui.label(
                        egui::RichText::new(subtitle)
                            .size(11.0)
                            .color(egui::Color32::from_rgb(135, 149, 173)),
                    );
                });
            })
            .response;

        ui.interact(resp.rect, resp.id.with("mode_click"), egui::Sense::click())
            .clicked()
    }

    /// Clickable Progressive Disclosure Drawer Header with live summary pills on the right
    fn drawer_header(
        ui: &mut egui::Ui,
        dark_mode: bool,
        open: &mut bool,
        title: &str,
        summary_pills: &[&str],
    ) {
        let bg = if *open {
            if dark_mode {
                egui::Color32::from_rgb(22, 29, 44)
            } else {
                egui::Color32::from_rgb(235, 241, 250)
            }
        } else if dark_mode {
            egui::Color32::from_rgb(16, 21, 31)
        } else {
            egui::Color32::from_rgb(248, 250, 252)
        };
        let stroke = if *open {
            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(59, 130, 246))
        } else if dark_mode {
            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(33, 42, 59))
        } else {
            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(214, 222, 234))
        };

        let resp = egui::Frame::none()
            .fill(bg)
            .stroke(stroke)
            .rounding(egui::Rounding::same(10.0))
            .inner_margin(egui::Margin::symmetric(14.0, 10.0))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    let arrow = if *open { "⏷" } else { "⏵" };
                    ui.label(
                        egui::RichText::new(format!("{}  {}", arrow, title))
                            .size(13.5)
                            .strong(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        for &pill in summary_pills.iter().rev() {
                            Self::status_pill(
                                ui,
                                pill,
                                egui::Color32::from_rgb(26, 34, 49),
                                egui::Color32::from_rgb(147, 197, 253),
                            );
                        }
                    });
                });
            })
            .response;

        if ui
            .interact(
                resp.rect,
                resp.id.with("drawer_toggle"),
                egui::Sense::click(),
            )
            .clicked()
        {
            *open = !*open;
        }
    }

    /// Custom 24-Segment Studio LED VU Peak Meter
    fn draw_led_vu_meter(ui: &mut egui::Ui, vu_pct: u8) {
        let segments = 24usize;
        let desired = egui::vec2(210.0, 16.0);
        let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 4.0, egui::Color32::from_rgb(10, 13, 20));

        let gap = 2.0f32;
        let seg_w = (rect.width() - gap * (segments as f32 + 1.0)) / (segments as f32);
        let active_segs = ((vu_pct as usize) * segments).div_ceil(100);

        for i in 0..segments {
            let x = rect.left() + gap + (i as f32) * (seg_w + gap);
            let seg_rect = egui::Rect::from_min_size(
                egui::pos2(x, rect.top() + 2.5),
                egui::vec2(seg_w, rect.height() - 5.0),
            );
            let color = if i < active_segs {
                if i >= 21 {
                    egui::Color32::from_rgb(239, 68, 68) // Red clip zone
                } else if i >= 16 {
                    egui::Color32::from_rgb(245, 158, 11) // Amber headroom zone
                } else {
                    egui::Color32::from_rgb(16, 185, 129) // Emerald nominal zone
                }
            } else {
                egui::Color32::from_rgb(26, 33, 46)
            };
            painter.rect_filled(seg_rect, 1.5, color);
        }
    }

    /// Studio Fader Row: Band title + frequency badge on left, clickable dB reset pill on right
    fn studio_fader_row(
        ui: &mut egui::Ui,
        dark_mode: bool,
        title: &str,
        freq_badge: &str,
        value: &mut f32,
        range: std::ops::RangeInclusive<f32>,
    ) -> bool {
        let mut changed = false;
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(title).size(12.5).strong());
                if !freq_badge.is_empty() {
                    Self::status_pill(
                        ui,
                        freq_badge,
                        if dark_mode {
                            egui::Color32::from_rgb(24, 31, 46)
                        } else {
                            egui::Color32::from_rgb(230, 236, 245)
                        },
                        egui::Color32::from_rgb(130, 144, 168),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let is_boosted = value.abs() >= 0.05;
                    let (bg, fg) = if is_boosted {
                        (
                            egui::Color32::from_rgb(30, 58, 138),
                            egui::Color32::from_rgb(147, 197, 253),
                        )
                    } else {
                        (
                            egui::Color32::from_rgb(24, 30, 43),
                            egui::Color32::from_rgb(148, 163, 184),
                        )
                    };
                    let db_btn = egui::Button::new(
                        egui::RichText::new(format!("{:+.1} dB", *value))
                            .monospace()
                            .size(11.5)
                            .strong()
                            .color(fg),
                    )
                    .fill(bg)
                    .rounding(egui::Rounding::same(6.0));
                    if ui
                        .add(db_btn)
                        .on_hover_text("Click to reset band to 0.0 dB")
                        .clicked()
                    {
                        *value = 0.0;
                        changed = true;
                    }
                });
            });

            ui.spacing_mut().slider_width = (ui.available_width() - 8.0).max(140.0);
            let slider = egui::Slider::new(value, range)
                .show_value(false)
                .trailing_fill(true);
            if ui.add(slider).changed() {
                changed = true;
            }
        });
        ui.add_space(2.0);
        changed
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
        painter.rect_filled(rect, 8.0, egui::Color32::WHITE);

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
        if ctx.input(|i| i.viewport().close_requested())
            && !self.force_quit
            && self.minimize_on_close
            && (self.host_engine.is_some() || self.client_engine.is_some())
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }

        let is_minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        if !is_minimized {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        if let Some(ref host) = self.host_engine {
            let client_count = host.telemetry.active_clients_count.load(Ordering::Relaxed);
            self.host_master_volume = host.telemetry.host_volume.load(Ordering::Relaxed);
            self.quality_profile =
                QualityProfile::from_u8(host.telemetry.quality_profile.load(Ordering::Relaxed));
            self.prefer_opus = host.telemetry.prefer_opus.load(Ordering::Relaxed);
            self.target_bitrate_kbps = host
                .telemetry
                .target_bitrate_kbps
                .load(Ordering::Relaxed)
                .clamp(96, 320);

            let pkts = host.telemetry.packets_sent.load(Ordering::Relaxed);
            let bytes_now = host.telemetry.bytes_sent.load(Ordering::Relaxed);
            let bt_delay = host
                .telemetry
                .max_client_bt_delay_ms
                .load(Ordering::Relaxed);

            if self.last_rate_calc.elapsed() >= Duration::from_secs(1) {
                let dt = self.last_rate_calc.elapsed().as_secs_f32().max(0.1);
                self.live_kbps = (((bytes_now.saturating_sub(self.prev_bytes_sent)) as f32 * 8.0)
                    / (dt * 1000.0)) as u32;
                self.live_pps = ((pkts.saturating_sub(self.prev_pkts_sent)) as f32 / dt) as u32;
                self.prev_bytes_sent = bytes_now;
                self.prev_pkts_sent = pkts;
                self.last_rate_calc = Instant::now();
            }

            if self.auto_bt_video_lipsync
                && should_apply_lipsync(bt_delay, self.last_applied_bt_delay)
            {
                apply_automatic_video_lipsync_offset_ms(bt_delay);
                self.last_applied_bt_delay = bt_delay;
            }

            if let Some(ref mut guard) = self.os_guard {
                if client_count > 0 {
                    guard.activate();
                } else {
                    guard.deactivate();
                }
            }
        }

        let dark = self.dark_mode;

        // =========================================================================
        // 1. TOP NAVIGATION BAR (Minimalist & Intent-Driven)
        // =========================================================================
        egui::TopBottomPanel::top("top_nav")
            .frame(
                egui::Frame::none()
                    .fill(if dark {
                        egui::Color32::from_rgb(14, 18, 27)
                    } else {
                        egui::Color32::from_rgb(255, 255, 255)
                    })
                    .inner_margin(egui::Margin::symmetric(18.0, 10.0))
                    .stroke(egui::Stroke::new(
                        1.0_f32,
                        if dark {
                            egui::Color32::from_rgb(28, 36, 52)
                        } else {
                            egui::Color32::from_rgb(220, 226, 236)
                        },
                    )),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let (dot_rect, _) =
                        ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                    let dot_col = if self.host_engine.is_some() {
                        egui::Color32::from_rgb(16, 185, 129)
                    } else if self.client_engine.is_some() {
                        egui::Color32::from_rgb(59, 130, 246)
                    } else {
                        egui::Color32::from_rgb(100, 116, 139)
                    };
                    ui.painter().circle_filled(dot_rect.center(), 4.5, dot_col);

                    ui.label(egui::RichText::new("AudioShare Ultra").size(17.0).strong());
                    ui.add_space(10.0);
                    ui.separator();
                    ui.add_space(6.0);

                    if Self::pill_choice(ui, dark, self.tab == AppTab::HostSender, "Send PC Audio")
                    {
                        self.tab = AppTab::HostSender;
                    }
                    if Self::pill_choice(
                        ui,
                        dark,
                        self.tab == AppTab::ClientReceiver,
                        "Receive & EQ",
                    ) {
                        self.tab = AppTab::ClientReceiver;
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Quit").clicked() {
                            self.force_quit = true;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        if Self::pill_choice(ui, dark, self.show_settings_modal, "Settings") {
                            self.interfaces = list_valid_host_interfaces();
                            self.show_settings_modal = !self.show_settings_modal;
                        }
                        if Self::pill_choice(ui, dark, self.show_qr_modal, "Pair Phone QR") {
                            self.interfaces = list_valid_host_interfaces();
                            self.selected_qr_iface_idx = refreshed_qr_iface_index(
                                &self.interfaces,
                                self.selected_qr_iface_idx,
                            );
                            self.show_qr_modal = !self.show_qr_modal;
                        }
                    });
                });
            });

        // =========================================================================
        // 2. BOTTOM STATUS & TELEMETRY BAR (Quiet 1-Line Summary + HUD Trigger)
        // =========================================================================
        egui::TopBottomPanel::bottom("bottom_status")
            .frame(
                egui::Frame::none()
                    .fill(if dark {
                        egui::Color32::from_rgb(13, 17, 25)
                    } else {
                        egui::Color32::from_rgb(248, 250, 252)
                    })
                    .inner_margin(egui::Margin::symmetric(18.0, 7.0))
                    .stroke(egui::Stroke::new(
                        1.0_f32,
                        if dark {
                            egui::Color32::from_rgb(28, 36, 52)
                        } else {
                            egui::Color32::from_rgb(220, 226, 236)
                        },
                    )),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if let Some(ref host) = self.host_engine {
                        let dtx = host.telemetry.dtx_silent_mode.load(Ordering::Relaxed);
                        let mode_id = host.telemetry.active_codec_mode.load(Ordering::Relaxed);
                        let clients = host.telemetry.active_clients_count.load(Ordering::Relaxed);
                        let kb = host.telemetry.bytes_sent.load(Ordering::Relaxed) / 1024;
                        let codec_label = if dtx {
                            "Smart DTX Sleep (0.6 kbps)".to_string()
                        } else if mode_id == 4 {
                            format!("Opus RLD ({} kbps)", self.target_bitrate_kbps)
                        } else if mode_id == 2 {
                            format!("Rust VBR ({} kbps)", self.target_bitrate_kbps)
                        } else if self.quality_profile == QualityProfile::StudioLossless24Bit {
                            "24-Bit Studio Master".to_string()
                        } else {
                            "16-Bit Lossless LPC".to_string()
                        };
                        ui.label(
                            egui::RichText::new(format!(
                                "• Broadcasting: {}   •   {} kbps ({} pps)   •   Sent: {} KB   •   Receivers: {}",
                                codec_label, self.live_kbps, self.live_pps, kb, clients
                            ))
                            .size(11.5)
                            .color(egui::Color32::from_rgb(134, 239, 172)),
                        );
                    } else if let Some(ref client) = self.client_engine {
                        let prim = client.telemetry.primary_frames_played.load(Ordering::Relaxed);
                        let fec = client.telemetry.fec_healed_frames.load(Ordering::Relaxed);
                        let plc = client.telemetry.plc_extrapolated_frames.load(Ordering::Relaxed);
                        let drift = client.telemetry.clock_drift_ppm.load(Ordering::Relaxed);
                        let snap = client.telemetry.latency_snapshot(DESKTOP_OUTPUT_BUFFER_MS);
                        ui.label(
                            egui::RichText::new(format!(
                                "• Receiving: {}   •   Est. Latency: {:.1} ms   •   Lossless: {}   •   FEC/PLC: {}/{}   •   Clock Lock: {:+} ppm",
                                snap.codec_name(),
                                snap.estimated_one_way_ms(),
                                prim,
                                fec,
                                plc,
                                drift
                            ))
                            .size(11.5)
                            .color(egui::Color32::from_rgb(147, 197, 253)),
                        );
                    } else {
                        let primary_ip = self
                            .interfaces
                            .get(default_qr_iface_index(&self.interfaces))
                            .map(|i| format!("{}:{}", i.ip, DEFAULT_PORT))
                            .unwrap_or_else(|| format!("0.0.0.0:{}", DEFAULT_PORT));
                        ui.label(
                            egui::RichText::new(format!(
                                "○ Standby   •   Host Endpoint: {}   •   Plug in USB or click Start Broadcast",
                                primary_ip
                            ))
                            .size(11.5)
                            .color(egui::Color32::from_rgb(130, 144, 168)),
                        );
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if Self::pill_choice(
                            ui,
                            dark,
                            self.latency_debug,
                            if self.latency_debug {
                                "Latency & Telemetry HUD: ON"
                            } else {
                                "Latency & Telemetry HUD"
                            },
                        ) {
                            self.latency_debug = !self.latency_debug;
                        }
                    });
                });
            });

        // =========================================================================
        // 3. MODAL WINDOWS: QR Pairing, System/Network Settings, and Latency HUD
        // =========================================================================
        if self.show_qr_modal {
            self.render_qr_modal(ctx);
        }

        if self.show_settings_modal {
            self.render_settings_modal(ctx);
        }

        if self.latency_debug {
            egui::Window::new("Live Stage-by-Stage Latency Breakdown")
                .default_width(600.0)
                .default_height(460.0)
                .resizable(true)
                .show(ctx, |ui| self.render_latency_debug(ui));
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    ui.add_space(4.0);
                    match self.tab {
                        AppTab::HostSender => self.render_host_tab(ui),
                        AppTab::ClientReceiver => self.render_client_tab(ui),
                    }
                    ui.add_space(12.0);
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
        self.os_guard = None;
    }
}

impl AudioShareApp {
    fn render_qr_modal(&mut self, ctx: &egui::Context) {
        let dark = self.dark_mode;
        egui::Window::new("Pair Android Phone via QR Code")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                Self::studio_card(ui, dark, |ui| {
                    let pin_opt = if self.enable_pin_encryption {
                        Some(self.pairing_pin.as_str())
                    } else {
                        None
                    };

                    ui.vertical_centered(|ui| {
                        if self.interfaces.len() > 1 {
                            ui.label(
                                egui::RichText::new("Select Network Adapter")
                                    .size(12.0)
                                    .strong(),
                            );
                            ui.horizontal_wrapped(|ui| {
                                for (idx, iface) in self.interfaces.iter().enumerate() {
                                    let tag = if iface.is_usb_tethering {
                                        "[USB]"
                                    } else {
                                        "[LAN]"
                                    };
                                    if Self::pill_choice(
                                        ui,
                                        dark,
                                        self.selected_qr_iface_idx == idx,
                                        &format!("{} {} ({})", tag, iface.name, iface.ip),
                                    ) {
                                        self.selected_qr_iface_idx = idx;
                                    }
                                }
                            });
                            ui.add_space(6.0);
                        }
                        let primary_ip = self
                            .interfaces
                            .get(self.selected_qr_iface_idx)
                            .or_else(|| self.interfaces.first())
                            .map(|i| i.ip.to_string())
                            .unwrap_or_else(|| "192.168.1.100".into());
                        let uri = build_qr_connect_uri(&primary_ip, DEFAULT_PORT, pin_opt);
                        ui.label("Scan with your Android Camera or the AudioShare Ultra app:");
                        ui.add_space(8.0);
                        Self::draw_qr_code(ui, &uri);
                        ui.add_space(8.0);
                        Self::status_pill(
                            ui,
                            &uri,
                            egui::Color32::from_rgb(15, 23, 42),
                            egui::Color32::from_rgb(147, 197, 253),
                        );
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("Copy Connection URI").clicked() {
                                ui.ctx().copy_text(uri.clone());
                            }
                            if ui.button("Close").clicked() {
                                self.show_qr_modal = false;
                            }
                        });
                    });
                });
            });
    }

    fn render_settings_modal(&mut self, ctx: &egui::Context) {
        let dark = self.dark_mode;
        egui::Window::new("System, Network, Hotspot & Security Settings")
            .collapsible(false)
            .resizable(true)
            .vscroll(true)
            .default_width(540.0)
            .max_height((ctx.screen_rect().height() - 48.0).max(240.0))
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                Self::studio_card(ui, dark, |ui| {
                    ui.label(
                        egui::RichText::new("Active Network & USB Endpoints")
                            .size(13.5)
                            .strong(),
                    );
                    if self.interfaces.is_empty() {
                        ui.label("Listening on 0.0.0.0:65530");
                    } else {
                        for iface in &self.interfaces {
                            let badge = if iface.is_usb_tethering {
                                "USB RNDIS"
                            } else {
                                "Wi-Fi / LAN"
                            };
                            ui.horizontal(|ui| {
                                Self::status_pill(
                                    ui,
                                    badge,
                                    egui::Color32::from_rgb(30, 41, 59),
                                    egui::Color32::from_rgb(147, 197, 253),
                                );
                                let ep = format!("{}:{}", iface.ip, DEFAULT_PORT);
                                ui.monospace(format!("{} ({})", ep, iface.name));
                                if ui.small_button("Copy IP").clicked() {
                                    ui.ctx().copy_text(ep);
                                }
                            });
                        }
                    }

                    ui.separator();
                    ui.label(
                        egui::RichText::new("1-Hop 5 GHz Direct Hotspot & Firewall")
                            .size(13.5)
                            .strong(),
                    );
                    ui.horizontal_wrapped(|ui| {
                        ui.label("SSID:");
                        ui.add_sized(
                            [120.0, 22.0],
                            egui::TextEdit::singleline(&mut self.hotspot_ssid),
                        );
                        ui.label("Password:");
                        ui.add_sized(
                            [110.0, 22.0],
                            egui::TextEdit::singleline(&mut self.hotspot_pass),
                        );
                    });
                    ui.horizontal_wrapped(|ui| {
                        let hs_btn = if self.direct_hotspot_active {
                            "Stop 1-Hop Direct Hotspot"
                        } else {
                            "Start 1-Hop 5GHz Link"
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
                                    Err(e) => {
                                        self.hotspot_status = format!("Hotspot notice: {}", e)
                                    }
                                }
                            }
                        }
                        if ui.button("1-Click Open Firewall (65530-65532)").clicked() {
                            self.hotspot_status = open_os_firewall_ports();
                        }
                    });
                    ui.label(
                        egui::RichText::new(&self.hotspot_status)
                            .size(11.0)
                            .color(egui::Color32::from_rgb(130, 144, 168)),
                    );

                    ui.separator();
                    ui.label(
                        egui::RichText::new("Stream Security (ChaCha20 + MAC32)")
                            .size(13.5)
                            .strong(),
                    );
                    if Self::setting_switch_row(
                        ui,
                        dark,
                        "Require Pairing PIN Encryption",
                        "Encrypts all audio and control packets with RFC 8439 ChaCha20",
                        &mut self.enable_pin_encryption,
                    ) {
                        if let Some(ref host) = self.host_engine {
                            let pin = if self.enable_pin_encryption {
                                Some(self.pairing_pin.as_str())
                            } else {
                                None
                            };
                            host.set_encryption_pin(pin);
                        }
                    }
                    ui.horizontal(|ui| {
                        ui.label("Pairing PIN:");
                        ui.add_sized(
                            [90.0, 22.0],
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
                    ui.label(
                        egui::RichText::new("Application & OS Preferences")
                            .size(13.5)
                            .strong(),
                    );
                    Self::setting_switch_row(
                        ui,
                        dark,
                        "Auto-Start Broadcast on App Launch",
                        "Immediately start Host Server when AudioShare Ultra opens",
                        &mut self.autostart_stream_on_launch,
                    );
                    if Self::setting_switch_row(
                        ui,
                        dark,
                        "Start App on OS Login",
                        "Register AudioShare Ultra in desktop session startup",
                        &mut self.os_boot_autostart,
                    ) {
                        self.host_status_msg = set_os_boot_autostart(self.os_boot_autostart);
                    }
                    Self::setting_switch_row(
                        ui,
                        dark,
                        "Minimize on Window Close",
                        "Keep active audio stream running in background when closing window",
                        &mut self.minimize_on_close,
                    );
                    if Self::setting_switch_row(
                        ui,
                        dark,
                        "Dark Studio Theme",
                        "Use high-contrast obsidian slate palette",
                        &mut self.dark_mode,
                    ) {
                        Self::apply_studio_theme(ctx, self.dark_mode);
                    }

                    ui.add_space(6.0);
                    ui.vertical_centered(|ui| {
                        if ui.button("Close Settings").clicked() {
                            self.show_settings_modal = false;
                        }
                    });
                });
            });
    }

    fn render_host_tab(&mut self, ui: &mut egui::Ui) {
        let is_running = self.host_engine.is_some();
        let dark = self.dark_mode;

        // =========================================================================
        // 1. SIMPLE HERO CONSOLE: Start/Stop CTA, Peak LED Meter & Master Volume
        // =========================================================================
        Self::studio_card(ui, dark, |ui| {
            ui.horizontal(|ui| {
                let (btn_text, btn_fill) = if is_running {
                    ("■  Stop Broadcast", egui::Color32::from_rgb(185, 28, 28))
                } else {
                    ("▶  Start Broadcast", egui::Color32::from_rgb(22, 163, 74))
                };
                let btn = egui::Button::new(
                    egui::RichText::new(btn_text)
                        .size(15.0)
                        .strong()
                        .color(egui::Color32::WHITE),
                )
                .fill(btn_fill)
                .rounding(egui::Rounding::same(8.0));

                if ui.add_sized([195.0, 42.0], btn).clicked() {
                    self.toggle_host();
                }

                ui.add_space(8.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    ui.label(
                        egui::RichText::new(&self.host_status_msg)
                            .size(13.5)
                            .strong()
                            .color(if is_running {
                                egui::Color32::from_rgb(134, 239, 172)
                            } else {
                                egui::Color32::from_rgb(148, 163, 184)
                            }),
                    );
                    let usb_connected = self
                        .adb_watcher
                        .as_ref()
                        .map(|a| a.usb_device_connected.load(Ordering::Relaxed))
                        .unwrap_or(false);
                    ui.label(
                        egui::RichText::new(if usb_connected {
                            "USB ADB Tunnel Active (<11 ms Wired Path Ready)"
                        } else {
                            "Wireless LAN Ready • Plug in USB cable anytime for instant wired failover"
                        })
                        .size(11.5)
                        .color(egui::Color32::from_rgb(130, 144, 168)),
                    );
                });
            });

            ui.add_space(6.0);
            ui.separator();

            let vu_pct = self
                .host_engine
                .as_ref()
                .map(|h| h.telemetry.host_vu_peak_pct.load(Ordering::Relaxed))
                .unwrap_or(0);
            let bt_delay = self
                .host_engine
                .as_ref()
                .map(|h| h.telemetry.max_client_bt_delay_ms.load(Ordering::Relaxed))
                .unwrap_or(0);

            ui.columns(2, |cols| {
                cols[0].horizontal(|ui| {
                    ui.label(egui::RichText::new("Audio Peak").size(12.5).strong());
                    Self::draw_led_vu_meter(ui, vu_pct);
                    ui.monospace(format!("{:>3}%", vu_pct));
                });
                cols[1].horizontal(|ui| {
                    ui.label(egui::RichText::new("Master Volume").size(12.5).strong());
                    ui.spacing_mut().slider_width = (ui.available_width() - 70.0).max(100.0);
                    if ui
                        .add(egui::Slider::new(&mut self.host_master_volume, 0..=100).suffix("%"))
                        .changed()
                    {
                        if let Some(ref host) = self.host_engine {
                            host.telemetry
                                .host_volume
                                .store(self.host_master_volume, Ordering::Relaxed);
                        }
                    }
                });
            });

            if bt_delay > 40 {
                ui.add_space(2.0);
                Self::status_pill(
                    ui,
                    &format!(
                        "Phone Bluetooth Earbuds Active (+{} ms A2DP) — Auto Video Lip-Sync Applied",
                        bt_delay
                    ),
                    egui::Color32::from_rgb(66, 32, 6),
                    egui::Color32::from_rgb(253, 224, 71),
                );
            }
        });

        ui.add_space(8.0);

        // =========================================================================
        // 2. 4 SIMPLE MODE CARDS (Zero Jargon, 1-Click Priority Selection)
        // =========================================================================
        ui.columns(4, |cols| {
            let is_auto = self.quality_profile == QualityProfile::SmartAuto;
            let is_lossless = matches!(
                self.quality_profile,
                QualityProfile::StudioLossless | QualityProfile::StudioLossless24Bit
            );
            let is_low_lat = self.quality_profile == QualityProfile::LowLatencyCompressed;
            let is_max_range = self.quality_profile == QualityProfile::MaxStability24Ghz;

            if Self::mode_preset_card(
                &mut cols[0],
                dark,
                is_auto,
                "Smart Auto",
                "Adapts to Wi-Fi quality",
                "Default",
            ) {
                self.quality_profile = QualityProfile::SmartAuto;
                if let Some(ref host) = self.host_engine {
                    host.telemetry
                        .quality_profile
                        .store(self.quality_profile as u8, Ordering::Relaxed);
                }
            }

            let lossless_badge = if self.quality_profile == QualityProfile::StudioLossless24Bit {
                "24-Bit"
            } else {
                "16-Bit"
            };
            if Self::mode_preset_card(
                &mut cols[1],
                dark,
                is_lossless,
                "Lossless HD",
                "100% bit-exact studio audio",
                lossless_badge,
            ) {
                if !is_lossless {
                    self.quality_profile = QualityProfile::StudioLossless;
                }
                if let Some(ref host) = self.host_engine {
                    host.telemetry
                        .quality_profile
                        .store(self.quality_profile as u8, Ordering::Relaxed);
                }
            }

            if Self::mode_preset_card(
                &mut cols[2],
                dark,
                is_low_lat,
                "Low-Latency VBR",
                "Compact fast wireless stream",
                &format!("{}k", self.target_bitrate_kbps),
            ) {
                self.quality_profile = QualityProfile::LowLatencyCompressed;
                if let Some(ref host) = self.host_engine {
                    host.telemetry
                        .quality_profile
                        .store(self.quality_profile as u8, Ordering::Relaxed);
                }
            }

            if Self::mode_preset_card(
                &mut cols[3],
                dark,
                is_max_range,
                "Max Range 2.4G",
                "10 ms framing through walls",
                "10 ms",
            ) {
                self.quality_profile = QualityProfile::MaxStability24Ghz;
                if let Some(ref host) = self.host_engine {
                    host.telemetry
                        .quality_profile
                        .store(self.quality_profile as u8, Ordering::Relaxed);
                }
            }
        });

        ui.add_space(8.0);

        // =========================================================================
        // 3. CONNECTED DEVICES CARD (Always Visible, Clean Empty State)
        // =========================================================================
        Self::studio_card(ui, dark, |ui| {
            let clients = self
                .host_engine
                .as_ref()
                .map(|h| h.connected_clients())
                .unwrap_or_default();

            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("Connected Receivers ({})", clients.len()))
                        .size(13.5)
                        .strong(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Pair New Phone (QR)").clicked() {
                        self.interfaces = list_valid_host_interfaces();
                        self.selected_qr_iface_idx =
                            refreshed_qr_iface_index(&self.interfaces, self.selected_qr_iface_idx);
                        self.show_qr_modal = true;
                    }
                });
            });

            if clients.is_empty() {
                ui.label(
                    egui::RichText::new(
                        "No receivers connected — tap this PC in the Android app or click 'Pair New Phone (QR)'.",
                    )
                    .size(12.0)
                    .color(egui::Color32::from_rgb(130, 144, 168)),
                );
            } else if let Some(ref host) = self.host_engine {
                ui.separator();
                for client in clients {
                    ui.horizontal(|ui| {
                        let badge = if client.is_usb { "USB" } else { "Wi-Fi" };
                        Self::status_pill(
                            ui,
                            badge,
                            egui::Color32::from_rgb(30, 58, 138),
                            egui::Color32::WHITE,
                        );
                        ui.monospace(format!(
                            "{}   •   Vol: {}%   •   Loss: {:.1}%   •   BT: {} ms",
                            client.addr,
                            client.client_volume,
                            (client.fec_rate_permille as f32) / 10.0,
                            client.bluetooth_delay_ms
                        ));
                        let mute_label = if client.muted { "Unmute" } else { "Mute" };
                        if ui.small_button(mute_label).clicked() {
                            host.set_client_muted(client.addr, !client.muted);
                        }
                        if ui.small_button("Kick").clicked() {
                            host.kick_client(client.addr);
                        }
                    });
                }
            }
        });

        ui.add_space(8.0);

        // =========================================================================
        // 4. PRO DRAWER 1: Codec, Bitrate, Frame Pacing & Multi-Device Sync
        // =========================================================================
        let profile_badge = match self.quality_profile {
            QualityProfile::SmartAuto => "Smart Auto",
            QualityProfile::StudioLossless => "16-Bit Lossless",
            QualityProfile::StudioLossless24Bit => "24-Bit Master",
            QualityProfile::LowLatencyCompressed => "Compressed VBR",
            QualityProfile::MaxStability24Ghz => "10ms 2.4G",
        };
        let engine_badge = if self.prefer_opus {
            "Opus RLD"
        } else {
            "Rust VBR"
        };
        let pacing_badge = if self.usb_low_latency_pacing {
            "2.5ms USB"
        } else {
            "5.0ms Wi-Fi"
        };
        Self::drawer_header(
            ui,
            dark,
            &mut self.host_pro_codec_open,
            "Pro Codec, Bitrate & Sync Tuning",
            &[profile_badge, engine_badge, pacing_badge],
        );

        if self.host_pro_codec_open {
            ui.add_space(4.0);
            Self::studio_card(ui, dark, |ui| {
                ui.label(
                    egui::RichText::new("Exact Quality Profile & Bit Depth")
                        .size(12.0)
                        .strong(),
                );
                ui.horizontal_wrapped(|ui| {
                    for (profile, label) in [
                        (QualityProfile::SmartAuto, "Smart Auto"),
                        (
                            QualityProfile::StudioLossless,
                            "Studio Lossless 16-Bit (0ms)",
                        ),
                        (
                            QualityProfile::StudioLossless24Bit,
                            "Studio Master 24-Bit (0ms)",
                        ),
                        (
                            QualityProfile::LowLatencyCompressed,
                            "Low-Latency Compressed",
                        ),
                        (
                            QualityProfile::MaxStability24Ghz,
                            "Max Stability (10ms 2.4GHz)",
                        ),
                    ] {
                        if Self::pill_choice(ui, dark, self.quality_profile == profile, label) {
                            self.quality_profile = profile;
                            if let Some(ref host) = self.host_engine {
                                host.telemetry
                                    .quality_profile
                                    .store(self.quality_profile as u8, Ordering::Relaxed);
                            }
                        }
                    }
                });

                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("Compressed Codec Module")
                        .size(12.0)
                        .strong(),
                );
                ui.horizontal_wrapped(|ui| {
                    if Self::pill_choice(
                        ui,
                        dark,
                        !self.prefer_opus,
                        "Pure-Rust VBR (Default, 0ms)",
                    ) {
                        self.prefer_opus = false;
                        if let Some(ref host) = self.host_engine {
                            host.telemetry.prefer_opus.store(false, Ordering::Relaxed);
                        }
                    }
                    if Self::pill_choice(
                        ui,
                        dark,
                        self.prefer_opus,
                        "Opus RLD (Pure Rust, 2.5ms lookahead)",
                    ) {
                        self.prefer_opus = true;
                        if let Some(ref host) = self.host_engine {
                            host.telemetry.prefer_opus.store(true, Ordering::Relaxed);
                        }
                    }
                });

                if matches!(
                    self.quality_profile,
                    QualityProfile::LowLatencyCompressed
                        | QualityProfile::SmartAuto
                        | QualityProfile::MaxStability24Ghz
                ) {
                    let stability = self.quality_profile == QualityProfile::MaxStability24Ghz;
                    let choices: &[u16] = if stability {
                        &[96, 160]
                    } else {
                        &[96, 160, 256, 320]
                    };
                    let effective = if stability {
                        self.target_bitrate_kbps.clamp(96, 160)
                    } else {
                        self.target_bitrate_kbps
                    };

                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("Target Compressed Bitrate")
                            .size(12.0)
                            .strong(),
                    );
                    ui.horizontal_wrapped(|ui| {
                        for &kbps in choices {
                            if Self::pill_choice(
                                ui,
                                dark,
                                effective == kbps,
                                &format!("{} kbps", kbps),
                            ) {
                                self.target_bitrate_kbps = kbps;
                                if let Some(ref host) = self.host_engine {
                                    host.telemetry
                                        .target_bitrate_kbps
                                        .store(self.target_bitrate_kbps, Ordering::Relaxed);
                                }
                            }
                        }
                    });
                    if stability {
                        ui.label(
                            egui::RichText::new(
                                "2.4G stability caps the target at 160 kbps and steps down to 96 kbps above 200‰ loss.",
                            )
                            .size(11.0)
                            .color(egui::Color32::from_rgb(130, 144, 168)),
                        );
                    }
                }

                ui.add_space(4.0);
                ui.separator();

                ui.columns(2, |cols| {
                    cols[0].vertical(|ui| {
                        Self::setting_switch_row(
                            ui,
                            dark,
                            "2.5 ms USB Frame Pacing",
                            "Single-subframe packets (OFF = 5.0 ms bundled Wi-Fi)",
                            &mut self.usb_low_latency_pacing,
                        );
                        Self::setting_switch_row(
                            ui,
                            dark,
                            "Smart DTX Silence Saver",
                            "Halts audio packets during silence (0.6 kbps heartbeat)",
                            &mut self.enable_smart_dtx,
                        );
                        Self::setting_switch_row(
                            ui,
                            dark,
                            "Auto Virtual Speaker & Scan Lock",
                            "Switches PC output on connect & suppresses 60s Wi-Fi scans",
                            &mut self.enable_virtual_speaker,
                        );
                    });
                    cols[1].vertical(|ui| {
                        Self::setting_switch_row(
                            ui,
                            dark,
                            "Auto Bluetooth Video Lip-Sync",
                            "Applies PipeWire offset matching phone Bluetooth delay",
                            &mut self.auto_bt_video_lipsync,
                        );
                        if Self::setting_switch_row(
                            ui,
                            dark,
                            "Multi-Device Acoustic Group Sync",
                            "Locks all connected phones and speakers in phase",
                            &mut self.enable_group_sync,
                        ) {
                            if let Some(ref host) = self.host_engine {
                                host.telemetry
                                    .enable_group_sync
                                    .store(self.enable_group_sync, Ordering::Relaxed);
                            }
                        }
                        Self::setting_switch_row(
                            ui,
                            dark,
                            "440 Hz / 554 Hz Test Chord",
                            "Synthesizes reference tone instead of system capture",
                            &mut self.use_test_tone,
                        );
                    });
                });

                if self.enable_group_sync {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("Room Alignment Trim:")
                                .size(12.0)
                                .strong(),
                        );
                        if ui
                            .add(
                                egui::Slider::new(&mut self.group_sync_trim_ms, 0..=120)
                                    .suffix(" ms"),
                            )
                            .changed()
                        {
                            if let Some(ref host) = self.host_engine {
                                host.telemetry
                                    .manual_group_sync_trim_ms
                                    .store(self.group_sync_trim_ms, Ordering::Relaxed);
                            }
                        }
                    });
                }
            });
        }

        ui.add_space(8.0);

        // =========================================================================
        // 5. PRO DRAWER 2: Per-App Audio Capture & Bidirectional Phone Receiver
        // =========================================================================
        let source_summary = if self.selected_app_idx.is_none() {
            "All System Audio"
        } else {
            "Single App Routed"
        };
        let mic_summary = if self.phone_mic_receiver.is_some() {
            "Phone RX: LIVE"
        } else {
            "Phone RX: Off"
        };
        Self::drawer_header(
            ui,
            dark,
            &mut self.host_routing_mic_open,
            "App Audio Picker & Phone Microphone (Port 65532)",
            &[source_summary, mic_summary],
        );

        if self.host_routing_mic_open {
            ui.add_space(4.0);
            Self::studio_card(ui, dark, |ui| {
                ui.columns(2, |cols| {
                    // Left: Per-App Capture
                    cols[0].vertical(|ui| {
                        ui.label(
                            egui::RichText::new("Per-Application Audio Capture")
                                .size(13.0)
                                .strong(),
                        );
                        ui.horizontal_wrapped(|ui| {
                            if Self::pill_choice(
                                ui,
                                dark,
                                self.selected_app_idx.is_none(),
                                "All System Audio",
                            ) {
                                self.selected_app_idx = None;
                                self.app_route_status = restore_all_apps_to_default_sink();
                            }
                            if ui.button("Refresh Apps").clicked() {
                                self.audio_apps = list_active_audio_apps();
                            }
                        });
                        if !self.audio_apps.is_empty() {
                            ui.horizontal_wrapped(|ui| {
                                for (idx, app) in self.audio_apps.iter().enumerate() {
                                    let selected = self.selected_app_idx == Some(idx);
                                    if Self::pill_choice(
                                        ui,
                                        dark,
                                        selected,
                                        &format!("{} ({})", app.name, app.pid),
                                    ) {
                                        self.selected_app_idx = Some(idx);
                                        match route_single_app_to_audioshare(app) {
                                            Ok(msg) => self.app_route_status = msg,
                                            Err(err) => self.app_route_status = err,
                                        }
                                    }
                                }
                            });
                        }
                        ui.label(
                            egui::RichText::new(&self.app_route_status)
                                .size(11.0)
                                .color(egui::Color32::from_rgb(130, 144, 168)),
                        );
                    });

                    // Right: Phone -> PC Receiver
                    cols[1].vertical(|ui| {
                        ui.label(
                            egui::RichText::new("Phone-to-PC Receiver (Port 65532)")
                                .size(13.0)
                                .strong(),
                        );
                        let mut mode_changed = false;
                        ui.horizontal_wrapped(|ui| {
                            if Self::pill_choice(
                                ui,
                                dark,
                                self.phone_rx_mode == PhoneRxRoutingMode::VirtualMicForCalls,
                                "Virtual Mic (Zoom / Discord)",
                            ) && self.phone_rx_mode != PhoneRxRoutingMode::VirtualMicForCalls
                            {
                                self.phone_rx_mode = PhoneRxRoutingMode::VirtualMicForCalls;
                                mode_changed = true;
                            }
                            if Self::pill_choice(
                                ui,
                                dark,
                                self.phone_rx_mode == PhoneRxRoutingMode::PlayOnPcSpeakers,
                                "Play on PC Speakers",
                            ) && self.phone_rx_mode != PhoneRxRoutingMode::PlayOnPcSpeakers
                            {
                                self.phone_rx_mode = PhoneRxRoutingMode::PlayOnPcSpeakers;
                                mode_changed = true;
                            }
                        });

                        if mode_changed && self.phone_mic_receiver.is_some() {
                            if let Some(mut m) = self.phone_mic_receiver.take() {
                                m.stop();
                            }
                            self.phone_mic_receiver = PhoneMicReceiver::start_with_mode(
                                PHONE_MIC_PORT,
                                self.phone_rx_mode,
                            )
                            .ok();
                        }

                        ui.horizontal_wrapped(|ui| {
                            let mic_active = self.phone_mic_receiver.is_some();
                            let (btn_label, btn_col) = if mic_active {
                                (
                                    "■  Stop Phone Receiver",
                                    egui::Color32::from_rgb(185, 28, 28),
                                )
                            } else {
                                (
                                    "▶  Start Phone Receiver",
                                    egui::Color32::from_rgb(37, 99, 235),
                                )
                            };
                            let btn = egui::Button::new(
                                egui::RichText::new(btn_label)
                                    .strong()
                                    .color(egui::Color32::WHITE),
                            )
                            .fill(btn_col);
                            if ui.add(btn).clicked() {
                                if let Some(mut m) = self.phone_mic_receiver.take() {
                                    m.stop();
                                } else if let Ok(rx) = PhoneMicReceiver::start_with_mode(
                                    PHONE_MIC_PORT,
                                    self.phone_rx_mode,
                                ) {
                                    self.phone_mic_receiver = Some(rx);
                                }
                            }

                            if let Some(ref mic) = self.phone_mic_receiver {
                                let pkts = mic.mic_packets_received.load(Ordering::Relaxed);
                                let peak = mic.mic_peak_level_pct.load(Ordering::Relaxed);
                                Self::status_pill(
                                    ui,
                                    &format!("RX: {} pkts • Peak: {}%", pkts, peak),
                                    egui::Color32::from_rgb(20, 83, 45),
                                    egui::Color32::from_rgb(134, 239, 172),
                                );

                                if let Some(ref host) = self.host_engine {
                                    if ui.button("Ping Acoustic Latency (Sonar)").clicked() {
                                        host.telemetry
                                            .trigger_sonar_ping
                                            .store(true, Ordering::Relaxed);
                                    }
                                    let tx_us = host
                                        .telemetry
                                        .sonar_tx_timestamp_us
                                        .load(Ordering::Relaxed);
                                    let rx_us =
                                        mic.last_chirp_rx_timestamp_us.load(Ordering::Relaxed);
                                    if rx_us > tx_us && (rx_us - tx_us) < 500_000 {
                                        self.measured_sonar_ms =
                                            Some(((rx_us - tx_us) as f32) / 2000.0);
                                        self.measured_sonar_at = Some(Instant::now());
                                    }
                                }
                            }
                            if let Some(ms) = self.measured_sonar_ms {
                                let age_s = self
                                    .measured_sonar_at
                                    .map(|t| t.elapsed().as_secs())
                                    .unwrap_or(0);
                                Self::status_pill(
                                    ui,
                                    &format!("Measured One-Way: {:.1} ms ({}s ago)", ms, age_s),
                                    egui::Color32::from_rgb(20, 83, 45),
                                    egui::Color32::from_rgb(134, 239, 172),
                                );
                            }
                        });
                    });
                });
            });
        }
    }

    fn render_client_tab(&mut self, ui: &mut egui::Ui) {
        let dark = self.dark_mode;

        // =========================================================================
        // 1. SIMPLE CONNECTION CONSOLE (Auto-Discovered Hosts + 1-Click Connect)
        // =========================================================================
        Self::studio_card(ui, dark, |ui| {
            let discovered = self.scanner.discovered_hosts();
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Available AudioShare Hosts")
                        .size(14.5)
                        .strong(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.client_engine.is_some() {
                        let disc_btn = egui::Button::new(
                            egui::RichText::new("■  Disconnect")
                                .strong()
                                .color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::from_rgb(185, 28, 28));
                        if ui.add(disc_btn).clicked() {
                            if let Some(mut c) = self.client_engine.take() {
                                c.stop();
                            }
                            self.client_status_msg = "Disconnected.".into();
                        }
                    }
                    let manual_lbl = if self.client_manual_ip_open {
                        "Hide Manual IP"
                    } else {
                        "Manual IP / USB"
                    };
                    if Self::pill_choice(ui, dark, self.client_manual_ip_open, manual_lbl) {
                        self.client_manual_ip_open = !self.client_manual_ip_open;
                    }
                });
            });

            ui.label(
                egui::RichText::new(&self.client_status_msg)
                    .size(12.0)
                    .color(if self.client_engine.is_some() {
                        egui::Color32::from_rgb(134, 239, 172)
                    } else {
                        egui::Color32::from_rgb(130, 144, 168)
                    }),
            );

            if !discovered.is_empty() {
                ui.separator();
                for host in &discovered {
                    ui.horizontal(|ui| {
                        let badge = if host.is_usb { "USB" } else { "Wi-Fi" };
                        Self::status_pill(
                            ui,
                            badge,
                            egui::Color32::from_rgb(30, 58, 138),
                            egui::Color32::WHITE,
                        );
                        ui.monospace(format!("{} ({})", host.hostname, host.addr));
                        if ui.button("▶  Connect Now").clicked() {
                            self.usb_buffer_mode = host.is_usb;
                            self.connect_client(host.addr);
                        }
                    });
                }
            } else if !self.client_manual_ip_open {
                ui.label(
                    egui::RichText::new(
                        "Scanning LAN & USB for beacons... Click 'Manual IP / USB' to enter an address directly.",
                    )
                    .size(11.5)
                    .color(egui::Color32::from_rgb(130, 144, 168)),
                );
            }

            if self.client_manual_ip_open || discovered.is_empty() {
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Host IP:Port:").size(12.5).strong());
                    ui.add_sized(
                        [210.0, 26.0],
                        egui::TextEdit::singleline(&mut self.manual_server_ip),
                    );
                    let connect_btn = egui::Button::new(
                        egui::RichText::new("▶  Connect")
                            .strong()
                            .color(egui::Color32::WHITE),
                    )
                    .fill(egui::Color32::from_rgb(22, 163, 74));
                    if ui.add(connect_btn).clicked() {
                        if let Ok(addr) = self.manual_server_ip.parse::<SocketAddr>() {
                            self.usb_buffer_mode = is_usb_subnet_ip(addr.ip());
                            self.connect_client(addr);
                        } else if let Ok(addr) =
                            format!("{}:{}", self.manual_server_ip, DEFAULT_PORT)
                                .parse::<SocketAddr>()
                        {
                            self.usb_buffer_mode = is_usb_subnet_ip(addr.ip());
                            self.connect_client(addr);
                        }
                    }
                    if ui.button("USB Localhost").clicked() {
                        self.manual_server_ip = format!("127.0.0.1:{}", DEFAULT_PORT);
                    }
                });
            }
        });

        ui.add_space(8.0);

        // =========================================================================
        // 2. SIMPLE SOUND PROFILE & 3 ENHANCEMENT CARDS (Always Visible)
        // =========================================================================
        let mut dsp_changed = false;
        Self::studio_card(ui, dark, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("Sound Profile:").size(13.0).strong());
                for (name, b, p, t) in [
                    ("Flat (Studio)", 0.0, 0.0, 0.0),
                    ("Bass Boost", 6.5, 0.0, 1.5),
                    ("Vocal / Podcast", -2.0, 5.5, 2.0),
                    ("FPS Footsteps", -3.0, 6.0, 4.5),
                    ("Harman Target", 4.5, 1.5, 3.0),
                ] {
                    let is_preset = (self.eq_bass_db - b).abs() < 0.05
                        && (self.eq_presence_db - p).abs() < 0.05
                        && (self.eq_treble_db - t).abs() < 0.05;
                    if Self::pill_choice(ui, dark, is_preset, name) {
                        self.eq_bass_db = b;
                        self.eq_presence_db = p;
                        self.eq_treble_db = t;
                        dsp_changed = true;
                    }
                }
                if ui.small_button("Reset All").clicked() {
                    self.preamp_gain_db = 0.0;
                    self.eq_bass_db = 0.0;
                    self.eq_presence_db = 0.0;
                    self.eq_treble_db = 0.0;
                    dsp_changed = true;
                }
            });

            ui.separator();
            ui.columns(3, |cols| {
                dsp_changed |= Self::setting_switch_row(
                    &mut cols[0],
                    dark,
                    "3D Headphone Stage",
                    "Binaural studio monitor crossfeed",
                    &mut self.binaural_crossfeed,
                );
                dsp_changed |= Self::setting_switch_row(
                    &mut cols[1],
                    dark,
                    "Night Mode DRC",
                    "Boost whispers & tame explosions",
                    &mut self.night_mode_drc,
                );
                dsp_changed |= Self::setting_switch_row(
                    &mut cols[2],
                    dark,
                    "5 ms Gaming Lock",
                    "Locks jitter target to lowest floor",
                    &mut self.gaming_hard_lock,
                );
            });
        });

        ui.add_space(8.0);

        // =========================================================================
        // 3. PRO DRAWER 1: Custom 3-Band Parametric EQ, Pre-Amp & Speaker Role
        // =========================================================================
        let preamp_pill = format!("Pre-Amp {:+.1} dB", self.preamp_gain_db);
        let role_pill = match self.channel_role {
            ChannelRole::Stereo => "Role: Stereo",
            ChannelRole::LeftMono => "Role: Left",
            ChannelRole::RightMono => "Role: Right",
            ChannelRole::CenterDialogue => "Role: Vocal",
            ChannelRole::SurroundAmbient => "Role: Surround",
        };
        Self::drawer_header(
            ui,
            dark,
            &mut self.client_eq_drawer_open,
            "Custom 3-Band Parametric EQ, Pre-Amp & Speaker Role",
            &[&preamp_pill, role_pill],
        );

        if self.client_eq_drawer_open {
            ui.add_space(4.0);
            Self::studio_card(ui, dark, |ui| {
                ui.columns(2, |cols| {
                    dsp_changed |= Self::studio_fader_row(
                        &mut cols[0],
                        dark,
                        "Pre-Amp Gain",
                        "True-Peak Limited",
                        &mut self.preamp_gain_db,
                        -12.0..=24.0,
                    );
                    dsp_changed |= Self::studio_fader_row(
                        &mut cols[0],
                        dark,
                        "Sub-Bass",
                        "90 Hz",
                        &mut self.eq_bass_db,
                        -12.0..=12.0,
                    );
                    dsp_changed |= Self::studio_fader_row(
                        &mut cols[1],
                        dark,
                        "Vocal Presence",
                        "2.5 kHz",
                        &mut self.eq_presence_db,
                        -12.0..=12.0,
                    );
                    dsp_changed |= Self::studio_fader_row(
                        &mut cols[1],
                        dark,
                        "Treble Air",
                        "10 kHz",
                        &mut self.eq_treble_db,
                        -12.0..=12.0,
                    );
                });

                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        egui::RichText::new("Spatial Speaker Role:")
                            .size(12.0)
                            .strong(),
                    );
                    for (role, label) in [
                        (ChannelRole::Stereo, "Stereo"),
                        (ChannelRole::LeftMono, "Left Speaker"),
                        (ChannelRole::RightMono, "Right Speaker"),
                        (ChannelRole::CenterDialogue, "Center Dialogue"),
                        (ChannelRole::SurroundAmbient, "Rear Surround"),
                    ] {
                        if Self::pill_choice(ui, dark, self.channel_role == role, label) {
                            self.channel_role = role;
                            dsp_changed = true;
                        }
                    }
                });
            });
        }

        if dsp_changed {
            if let Some(ref c) = self.client_engine {
                self.sync_client_dsp_atomics(c);
            }
        }

        ui.add_space(8.0);

        // =========================================================================
        // 4. PRO DRAWER 2: Real-Time WSOLA Sequencer & Clock-Drift Oscilloscope
        // =========================================================================
        let (pkts, prim, fec, plc, auth_err, drift, dtx, host_vol, group_ms, idle_secs) =
            if let Some(ref client) = self.client_engine {
                let drift_now = client.telemetry.clock_drift_ppm.load(Ordering::Relaxed);
                self.drift_history.pop_front();
                self.drift_history.push_back(drift_now as f32);
                (
                    client.telemetry.packets_received.load(Ordering::Relaxed),
                    client
                        .telemetry
                        .primary_frames_played
                        .load(Ordering::Relaxed),
                    client.telemetry.fec_healed_frames.load(Ordering::Relaxed),
                    client
                        .telemetry
                        .plc_extrapolated_frames
                        .load(Ordering::Relaxed),
                    client.telemetry.auth_failures.load(Ordering::Relaxed),
                    drift_now,
                    client.telemetry.server_dtx_silent.load(Ordering::Relaxed),
                    client.telemetry.host_volume.load(Ordering::Relaxed),
                    client.telemetry.max_group_delay_ms.load(Ordering::Relaxed),
                    client
                        .telemetry
                        .seconds_since_last_packet
                        .load(Ordering::Relaxed),
                )
            } else {
                (0, 0, 0, 0, 0, 0, false, 100, 0, 0)
            };

        let drift_pill = format!("Drift: {:+} ppm", drift);
        let fec_pill = format!("FEC/PLC: {}/{}", fec, plc);
        Self::drawer_header(
            ui,
            dark,
            &mut self.client_telemetry_drawer_open,
            "Real-Time WSOLA Sequencer & Clock-Drift Oscilloscope",
            &[&fec_pill, &drift_pill],
        );

        if self.client_telemetry_drawer_open {
            ui.add_space(4.0);
            Self::studio_card(ui, dark, |ui| {
                if self.client_engine.is_some() && idle_secs >= 3 {
                    Self::status_pill(
                        ui,
                        &format!(
                            "No packets from Host for {}s — Waiting for automatic reconnect...",
                            idle_secs
                        ),
                        egui::Color32::from_rgb(66, 32, 6),
                        egui::Color32::from_rgb(253, 224, 71),
                    );
                }

                ui.columns(4, |cols| {
                    Self::metric_tile(
                        &mut cols[0],
                        dark,
                        "RX Packets / Lossless",
                        &format!("{} / {}", pkts, prim),
                    );
                    Self::metric_tile(
                        &mut cols[1],
                        dark,
                        "FEC Healed / WSOLA PLC",
                        &format!("{} / {}", fec, plc),
                    );
                    Self::metric_tile(
                        &mut cols[2],
                        dark,
                        "Clock Lock / Auth Err",
                        &format!("{:+4} ppm / {}", drift, auth_err),
                    );
                    let state_str = if self.client_engine.is_none() {
                        "STANDBY"
                    } else if dtx {
                        "DTX SLEEP"
                    } else {
                        "LIVE"
                    };
                    Self::metric_tile(
                        &mut cols[3],
                        dark,
                        "Host Vol / Group Sync",
                        &format!("{}% / +{} ms ({})", host_vol, group_ms, state_str),
                    );
                });

                ui.add_space(4.0);
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 52.0),
                    egui::Sense::hover(),
                );
                let painter = ui.painter();
                painter.rect_filled(rect, 8.0, egui::Color32::from_rgb(10, 13, 20));
                painter.rect_stroke(
                    rect,
                    8.0,
                    egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(30, 38, 54)),
                );
                let mid_y = rect.center().y;
                painter.line_segment(
                    [
                        egui::pos2(rect.left(), mid_y),
                        egui::pos2(rect.right(), mid_y),
                    ],
                    egui::Stroke::new(1.0f32, egui::Color32::from_rgb(38, 48, 66)),
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
                    egui::Stroke::new(1.75f32, egui::Color32::from_rgb(56, 189, 248)),
                ));
            });
        }
    }

    fn render_latency_debug(&mut self, ui: &mut egui::Ui) {
        let dark = self.dark_mode;
        ui.label(
            egui::RichText::new(
                "Live values are published by the audio threads. The estimate is not an acoustic measurement.",
            )
            .size(11.5)
            .color(egui::Color32::from_rgb(130, 144, 168)),
        );
        ui.add_space(4.0);

        let sonar_ms = self.measured_sonar_ms;
        let sonar_age_s = self.measured_sonar_at.map(|t| t.elapsed().as_secs_f32());
        let mut any_engine = false;

        if let Some(ref host) = self.host_engine {
            any_engine = true;
            Self::studio_card(ui, dark, |ui| {
                ui.label(
                    egui::RichText::new("Host Pipeline: Capture -> Wire")
                        .size(14.5)
                        .strong(),
                );
                ui.separator();

                let pacing_ms =
                    host.telemetry.tx_packet_interval_us.load(Ordering::Relaxed) as f32 / 1000.0;
                let capture_ms =
                    host.telemetry.capture_buffer_us.load(Ordering::Relaxed) as f32 / 1000.0;
                let subframes = host
                    .telemetry
                    .tx_subframes_per_packet
                    .load(Ordering::Relaxed);
                let codec =
                    codec_mode_name(host.telemetry.active_codec_mode.load(Ordering::Relaxed));
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
                let rebinds = host.telemetry.hotplug_rebinds.load(Ordering::Relaxed);

                ui.columns(3, |cols| {
                    Self::metric_tile(
                        &mut cols[0],
                        dark,
                        "Packetization",
                        &format!(
                            "{:.1} ms ({} x {:.1} ms)",
                            pacing_ms, subframes, SUBFRAME_MS
                        ),
                    );
                    Self::metric_tile(
                        &mut cols[1],
                        dark,
                        "Capture Backlog",
                        &format!("{:.1} ms", capture_ms),
                    );
                    Self::metric_tile(
                        &mut cols[2],
                        dark,
                        "Host Pipeline Total",
                        &format!("{:.1} ms ({})", capture_ms + pacing_ms, codec),
                    );
                });

                ui.columns(4, |cols| {
                    Self::metric_tile(
                        &mut cols[0],
                        dark,
                        "Downstream Max",
                        &format!("{} ms (BT max {} ms)", downstream_max, bt_max),
                    );
                    Self::metric_tile(
                        &mut cols[1],
                        dark,
                        "Room Trim",
                        &format!("+{} ms", room_trim),
                    );
                    let sonar_label = match (sonar_ms, sonar_age_s) {
                        (Some(ms), Some(age)) => format!("{:.1} ms ({:.0}s ago)", ms, age),
                        (Some(ms), None) => format!("{:.1} ms", ms),
                        _ => "not measured".to_string(),
                    };
                    Self::metric_tile(&mut cols[2], dark, "Acoustic Sonar", &sonar_label);
                    Self::metric_tile(
                        &mut cols[3],
                        dark,
                        "Hotplug Rebinds",
                        &format!("{}", rebinds),
                    );
                });

                if clients.is_empty() {
                    ui.label("No clients connected.");
                } else {
                    ui.separator();
                    for c in &clients {
                        let badge = if c.is_usb { "[USB]" } else { "[Wi-Fi]" };
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
            });
        }

        if any_engine {
            ui.add_space(6.0);
        }

        if let Some(ref client) = self.client_engine {
            any_engine = true;
            let snap = client.telemetry.latency_snapshot(DESKTOP_OUTPUT_BUFFER_MS);

            Self::studio_card(ui, dark, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("Client Pipeline: Ingress -> Speaker")
                            .size(14.5)
                            .strong(),
                    );
                    Self::status_pill(
                        ui,
                        &format!("Estimated One-Way: {:.1} ms", snap.estimated_one_way_ms()),
                        egui::Color32::from_rgb(20, 83, 45),
                        egui::Color32::from_rgb(134, 239, 172),
                    );
                    Self::status_pill(
                        ui,
                        if snap.prebuffer_ready {
                            "READY"
                        } else {
                            "FILLING"
                        },
                        egui::Color32::from_rgb(30, 41, 59),
                        egui::Color32::WHITE,
                    );
                });
                ui.separator();

                ui.columns(3, |cols| {
                    Self::metric_tile(
                        &mut cols[0],
                        dark,
                        "Sender Packetization",
                        &format!("{:.1} ms ({})", snap.packet_interval_ms, snap.codec_name()),
                    );
                    Self::metric_tile(
                        &mut cols[1],
                        dark,
                        "Network Jitter",
                        &format!("{:.1} ms", snap.network_jitter_ms),
                    );
                    Self::metric_tile(
                        &mut cols[2],
                        dark,
                        "Adaptive Cushion",
                        &format!("{:.1} ms", snap.adaptive_buffer_ms),
                    );
                });

                ui.columns(3, |cols| {
                    Self::metric_tile(
                        &mut cols[0],
                        dark,
                        "Target FIFO",
                        &format!(
                            "{:.1} ms (Group +{:.1} ms)",
                            snap.target_buffer_ms, snap.group_sync_extra_ms
                        ),
                    );
                    Self::metric_tile(
                        &mut cols[1],
                        dark,
                        "Buffered Now",
                        &format!("{:.1} ms", snap.buffered_audio_ms),
                    );
                    Self::metric_tile(
                        &mut cols[2],
                        dark,
                        "Downstream + Lookahead",
                        &format!(
                            "{:.1} ms + {:.1} ms",
                            snap.downstream_stage_ms(),
                            snap.codec_lookahead_ms
                        ),
                    );
                });

                ui.columns(4, |cols| {
                    Self::metric_tile(
                        &mut cols[0],
                        dark,
                        "DAC Queue",
                        &if snap.hardware_delay_ms > 0 {
                            format!("{} ms (measured)", snap.hardware_delay_ms)
                        } else {
                            "not measured".to_string()
                        },
                    );
                    Self::metric_tile(
                        &mut cols[1],
                        dark,
                        "Bluetooth Estimate",
                        &if snap.bluetooth_delay_ms > 0 {
                            format!("{} ms (A2DP)", snap.bluetooth_delay_ms)
                        } else {
                            "not routed".to_string()
                        },
                    );
                    Self::metric_tile(
                        &mut cols[2],
                        dark,
                        "Desktop Output Buffer",
                        &format!("{:.1} ms", snap.output_buffer_ms),
                    );
                    Self::metric_tile(
                        &mut cols[3],
                        dark,
                        "Prebuffer / Codec",
                        &format!(
                            "{} • {}",
                            if snap.prebuffer_ready {
                                "READY"
                            } else {
                                "FILLING"
                            },
                            snap.codec_name()
                        ),
                    );
                });

                let fill = if snap.target_buffer_ms > 0.0 {
                    (snap.buffered_audio_ms / snap.target_buffer_ms).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                ui.add(
                    egui::ProgressBar::new(fill)
                        .fill(egui::Color32::from_rgb(37, 99, 235))
                        .text(format!(
                            "FIFO Fill: {:.1} / {:.1} ms ({:.0}%)",
                            snap.buffered_audio_ms,
                            snap.target_buffer_ms,
                            fill * 100.0
                        )),
                );

                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Estimate = packetization + target FIFO + max(DAC, BT, desktop output) + codec lookahead. \
                         Group sync is already inside the target FIFO. Excludes one-way network transit and the acoustic air path; use the sonar for end-to-end.",
                    )
                    .size(11.0)
                    .color(egui::Color32::from_rgb(130, 144, 168)),
                );
            });
        }

        if !any_engine {
            Self::studio_card(ui, dark, |ui| {
                ui.label("Start the Host Server or connect to a Client to see live latency.");
            });
        }
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
    fn test_qr_iface_defaults_to_wifi_and_keeps_manual_choice() {
        let usb = NetworkInterfaceInfo {
            name: "enp0s20f0u1".into(),
            ip: "192.168.42.100".parse().unwrap(),
            is_usb_tethering: true,
        };
        let eth = NetworkInterfaceInfo {
            name: "enp3s0".into(),
            ip: "192.168.1.10".parse().unwrap(),
            is_usb_tethering: false,
        };
        let wifi = NetworkInterfaceInfo {
            name: "wlp2s0".into(),
            ip: "192.168.1.20".parse().unwrap(),
            is_usb_tethering: false,
        };
        // list_valid_host_interfaces() sorts USB first; the QR code must not
        // default to that unreachable RNDIS address.
        let interfaces = vec![usb.clone(), eth, wifi];
        assert_eq!(default_qr_iface_index(&interfaces), 1);
        // An operator selection survives a refresh, but a vanished index falls
        // back to the Wi-Fi/LAN default.
        assert_eq!(refreshed_qr_iface_index(&interfaces, 2), 2);
        assert_eq!(refreshed_qr_iface_index(&interfaces, 99), 1);
        assert_eq!(default_qr_iface_index(&[usb]), 0);
        assert_eq!(default_qr_iface_index(&[]), 0);
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
prefer_opus=true
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
        assert!(app.prefer_opus);
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

    fn icon_pixel(icon: &egui::IconData, x: usize, y: usize) -> [u8; 4] {
        let idx = (y * icon.width as usize + x) * 4;
        [
            icon.rgba[idx],
            icon.rgba[idx + 1],
            icon.rgba[idx + 2],
            icon.rgba[idx + 3],
        ]
    }

    #[test]
    fn test_window_icon_dimensions_and_corners() {
        let icon = build_studio_window_icon();
        assert_eq!(icon.width, 64);
        assert_eq!(icon.height, 64);
        assert_eq!(icon.rgba.len(), 64 * 64 * 4);
        for (x, y) in [(0, 0), (63, 0), (0, 63), (63, 63)] {
            assert_eq!(
                icon_pixel(&icon, x, y)[3],
                0,
                "corner ({x},{y}) must be transparent"
            );
        }
    }

    #[test]
    fn test_window_icon_center_bar_is_white() {
        let icon = build_studio_window_icon();
        let p = icon_pixel(&icon, 32, 32);
        assert_eq!(p[3], 255);
        assert!(
            p[0] > 240 && p[1] > 240 && p[2] > 240,
            "center lossless bar must be white, got {p:?}"
        );
    }

    #[test]
    fn test_window_icon_arcs_use_cyan_and_emerald_brand_colors() {
        let icon = build_studio_window_icon();
        let left = icon_pixel(&icon, 10, 32);
        assert_eq!(left[3], 255);
        assert!(
            left[2] > 220 && left[1] > 130,
            "left transmission arc must stay cyan/blue, got {left:?}"
        );
        let right = icon_pixel(&icon, 54, 32);
        assert_eq!(right[3], 255);
        assert!(
            right[1] > 130 && right[1] > right[2],
            "right receiver arc must stay emerald, got {right:?}"
        );
    }

    #[test]
    fn test_window_icon_stereo_bars_use_brand_colors() {
        let icon = build_studio_window_icon();
        let left = icon_pixel(&icon, 26, 32);
        assert!(
            left[2] > 220 && left[1] > 150,
            "left bar must be cyan, got {left:?}"
        );
        let right = icon_pixel(&icon, 38, 32);
        assert!(
            right[1] > 180 && right[2] < 190,
            "right bar must be emerald, got {right:?}"
        );
    }

    #[test]
    fn test_window_icon_opaque_pixels_are_never_black() {
        let icon = build_studio_window_icon();
        for (i, chunk) in icon.rgba.chunks_exact(4).enumerate() {
            if chunk[3] > 0 {
                assert!(
                    chunk[0] > 0 || chunk[1] > 0 || chunk[2] > 0,
                    "opaque black pixel at index {i}"
                );
            }
        }
    }
}
