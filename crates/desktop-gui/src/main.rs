#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ui_widgets;

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use audioshare_client_engine::{ClientConfig, ClientEngine};
use audioshare_host_engine::app_routing::{
    list_active_audio_apps, restore_all_apps_to_default_sink, route_single_app_to_audioshare,
    AudioAppEntry,
};
use audioshare_host_engine::bt_optimization::{
    apply_automatic_video_lipsync_offset_ms, reset_video_lipsync_offset_blocking,
};
use audioshare_host_engine::discovery_and_os::{
    open_os_firewall_ports, set_os_boot_autostart, AdbUsbWatcher, DirectHotspotController,
    DiscoveryBeacon, DiscoveryScanner, VirtualSpeakerAndRadioGuard,
};
// Added by the Windows firewall lane (L4); gated so Linux check stays valid here.
#[cfg(target_os = "windows")]
use audioshare_host_engine::discovery_and_os::is_windows_firewall_configured;
use audioshare_host_engine::virtual_mic::{PhoneMicReceiver, PhoneRxRoutingMode, PHONE_MIC_PORT};
use audioshare_host_engine::{
    is_usb_subnet_ip, list_valid_host_interfaces, HostConfig, HostEngine, NetworkInterfaceInfo,
    DEFAULT_PORT,
};
use audioshare_protocol::crypto::SessionKey;
use audioshare_protocol::latency::{codec_mode_name, DESKTOP_OUTPUT_BUFFER_MS, SUBFRAME_MS};
use audioshare_protocol::lpc_lossless::QualityProfile;
use audioshare_protocol::sync_and_eq::{build_qr_connect_uri, ChannelRole};
use eframe::egui;
use ui_widgets as uw;

fn main() -> eframe::Result<()> {
    // Write the ring-buffered diagnostics on a fatal panic before panic=abort.
    let crash_path = AudioShareApp::config_file_path().with_file_name("audioshare-ultra-crash.log");
    audioshare_protocol::diagnostics::install_crash_hook(crash_path);
    #[cfg_attr(not(target_os = "windows"), allow(unused_mut))]
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([920.0, 740.0])
            .with_min_inner_size([760.0, 560.0])
            .with_icon(build_studio_window_icon())
            .with_title("AudioShare Ultra — Studio Audio Routing Mesh"),
        ..Default::default()
    };

    // On Windows, wgpu's default Backends::PRIMARY includes Vulkan, which loads
    // third-party Vulkan ICDs (e.g. Intel igvk64.dll) that can segfault (0xc0000005)
    // during adapter enumeration before wgpu can fall back to DX12. Prefer native
    // DX12 (+ GL fallback) unless the user explicitly sets WGPU_BACKEND.
    #[cfg(target_os = "windows")]
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(ref mut setup) = options.wgpu_options.wgpu_setup {
        setup.instance_descriptor.backends = eframe::wgpu::Backends::from_env()
            .unwrap_or(eframe::wgpu::Backends::DX12 | eframe::wgpu::Backends::GL);
    }

    eframe::run_native(
        "AudioShare Ultra",
        options,
        Box::new(|cc| {
            let app = AudioShareApp::new();
            uw::apply_studio_theme(&cc.egui_ctx, app.dark_mode);
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

#[derive(PartialEq, Eq, Clone, Copy)]
enum SettingsTab {
    NetworkAndSecurity,
    GeneralAndDiagnostics,
}

/// Results sent back from detached workers that run blocking OS operations
/// (firewall / hotspot). `logic()` drains the channel every frame so the egui
/// thread never stalls on `pkexec`, `netsh`, `nmcli`, or `netsh wlan`.
enum BgTaskResult {
    Firewall { msg: String, firewall_ok: bool },
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    FirewallProbe { firewall_ok: bool },
    HotspotStart {
        result: Result<String, String>,
    },
    HotspotStop,
}

struct AudioShareApp {
    tab: AppTab,
    interfaces: Vec<NetworkInterfaceInfo>,
    /// Index into `interfaces` used to build the pairing QR code. Defaults to
    /// the first Wi-Fi/LAN adapter instead of the USB-RNDIS link that
    /// `list_valid_host_interfaces()` sorts first.
    selected_qr_iface_idx: usize,
    show_qr_modal: bool,
    /// QR PNG re-encoding/upload cache, keyed by the connection URI. Without
    /// this the modal re-encodes a QR at every repaint (~16 Hz while streaming).
    qr_cache: uw::QrTextureCache,
    show_settings_modal: bool,
    settings_tab: SettingsTab,
    pin_dirty: bool,
    vu_peak_hold: f32,
    /// Transient banner text plus the instant it was raised. Cleared after 2.5 s.
    toast: Option<(String, Instant)>,
    /// Detached worker results (firewall / hotspot) are marshalled back here
    /// and applied from `logic()` so the UI thread never blocks.
    bg_task_tx: mpsc::Sender<BgTaskResult>,
    bg_task_rx: mpsc::Receiver<BgTaskResult>,
    firewall_task_in_flight: bool,
    hotspot_task_in_flight: bool,
    #[cfg(target_os = "windows")]
    windows_firewall_ok: bool,
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
    /// Shared with the live beacon so a PIN toggle immediately changes the
    /// advertised `enc` bit without restarting the host.
    beacon_encrypted: Arc<AtomicBool>,
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
    phone_rx_status: String,

    // Client & Studio DSP State
    /// `None` in the side-effect-free test instance (no UDP bind). Started in
    /// `new()` before the first frame is drawn.
    scanner: Option<DiscoveryScanner>,
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
    last_sonar_pair: (u64, u64),
    latency_debug: bool,
    gaming_hard_lock: bool,
    usb_buffer_mode: bool,
    drift_history: VecDeque<f32>,
    /// Fixed-rate (100 ms) oscilloscope sampling clock, driven from `logic()`
    /// rather than from render code (which ran at an arbitrary frame rate).
    last_drift_sample: Instant,
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

/// Offset the GUI must push to PipeWire this frame, or `None` when nothing
/// changed. Turning the feature off must actively clear a previously applied
/// sink offset: otherwise the last Bluetooth delay stays latched into the
/// default sink and every other audio source keeps playing behind the video.
fn lipsync_offset_to_apply(
    enabled: bool,
    bt_delay_ms: u16,
    last_applied_ms: u16,
) -> Option<u16> {
    if !enabled {
        return if last_applied_ms == 0 { None } else { Some(0) };
    }
    should_apply_lipsync(bt_delay_ms, last_applied_ms).then_some(bt_delay_ms)
}

/// Format a manual lip-sync hint for Windows hosts when an active Bluetooth
/// peer adds playback delay (Windows lacks PipeWire per-sink negative video
/// offset injection).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn format_windows_bt_lipsync_hint(offset_ms: u32) -> Option<String> {
    if offset_ms == 0 {
        return None;
    }
    Some(format!(
        "VLC/mpv audio delay: -{offset_ms} ms • Enable Earbud Gaming / Low-Latency Mode or LE Audio (LC3) to reduce browser lag to ~40-60 ms."
    ))
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

/// Refresh `interfaces` while preserving the operator's previously chosen QR
/// adapter by IP address (so plugging in a USB-tethered phone after launch,
/// which inserts at index 0, never silently switches a Wi-Fi QR code to an
/// unreachable USB RNDIS IP).
fn refreshed_qr_iface_index_by_ip(
    interfaces: &[NetworkInterfaceInfo],
    prev_ip: Option<std::net::IpAddr>,
    prev_idx: usize,
) -> usize {
    if let Some(ip) = prev_ip {
        if let Some(pos) = interfaces.iter().position(|i| i.ip == ip) {
            return pos;
        }
    }
    let _ = prev_idx;
    default_qr_iface_index(interfaces)
}

/// Keep the operator's chosen adapter across a refresh, falling back to the
/// Wi-Fi/LAN default only when the old index no longer exists.
#[allow(dead_code)]
fn refreshed_qr_iface_index(interfaces: &[NetworkInterfaceInfo], selected: usize) -> usize {
    if selected < interfaces.len() {
        selected
    } else {
        default_qr_iface_index(interfaces)
    }
}

/// Resolve whether the desktop client should send a pairing PIN to the target
/// host. Discovered open hosts (`use_pin = false`) must connect unencrypted
/// even when this machine's own host PIN encryption is enabled.
fn effective_client_pin(use_pin: bool, encryption_enabled: bool, pin: &str) -> Option<&str> {
    let trimmed = pin.trim();
    if use_pin && encryption_enabled && !trimmed.is_empty() {
        Some(trimmed)
    } else {
        None
    }
}

impl AudioShareApp {
    /// Side-effect-free construction shared by `new()` and the config unit
    /// tests. Deliberately does **not** enumerate NICs, bind the discovery UDP
    /// socket, read the on-disk config, or auto-start the host engine — tests
    /// that call this get a deterministic clean slate.
    fn blank() -> Self {
        let (bg_task_tx, bg_task_rx) = mpsc::channel();
        Self {
            tab: AppTab::HostSender,
            interfaces: Vec::new(),
            selected_qr_iface_idx: 0,
            show_qr_modal: false,
            qr_cache: uw::QrTextureCache::default(),
            show_settings_modal: false,
            settings_tab: SettingsTab::NetworkAndSecurity,
            pin_dirty: false,
            vu_peak_hold: 0.0,
            toast: None,
            bg_task_tx,
            bg_task_rx,
            firewall_task_in_flight: false,
            hotspot_task_in_flight: false,
            #[cfg(target_os = "windows")]
            windows_firewall_ok: true,
            dark_mode: true,
            minimize_on_close: true,
            autostart_stream_on_launch: false,
            os_boot_autostart: false,
            force_quit: false,
            host_pro_codec_open: false,
            host_routing_mic_open: false,
            client_manual_ip_open: true,
            client_eq_drawer_open: false,
            client_telemetry_drawer_open: false,
            host_engine: None,
            beacon: None,
            adb_watcher: None,
            os_guard: None,
            quality_profile: QualityProfile::SmartAuto,
            prefer_opus: true,
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
            beacon_encrypted: Arc::new(AtomicBool::new(false)),
            pairing_pin: "4829".into(),
            audio_apps: Vec::new(),
            selected_app_idx: None,
            app_route_status: "Capturing Full System Audio".into(),
            phone_mic_receiver: None,
            phone_rx_mode: PhoneRxRoutingMode::VirtualMicForCalls,
            phone_rx_status: "Stopped — click Start Phone Receiver, then enable Share Phone Microphone on your phone.".into(),
            scanner: None,
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
            last_sonar_pair: (0, 0),
            latency_debug: false,
            gaming_hard_lock: false,
            usb_buffer_mode: false,
            drift_history: VecDeque::from(vec![0.0; 120]),
            last_drift_sample: Instant::now(),
            client_status_msg: "Idle. Select a discovered host or enter IP.".into(),
        }
    }

    fn new() -> Self {
        let mut app = Self::blank();
        app.interfaces = list_valid_host_interfaces();
        app.selected_qr_iface_idx = default_qr_iface_index(&app.interfaces);
        app.audio_apps = list_active_audio_apps();
        app.scanner = Some(DiscoveryScanner::start());
        #[cfg(target_os = "windows")]
        {
            // Probe PowerShell/netsh off the UI thread so cold launch stays <50 ms.
            let tx = app.bg_task_tx.clone();
            std::thread::spawn(move || {
                let firewall_ok = is_windows_firewall_configured();
                let _ = tx.send(BgTaskResult::FirewallProbe { firewall_ok });
            });
        }
        if Self::config_file_path().exists() {
            app.load_persistent_config();
        } else {
            app.pairing_pin = Self::generate_initial_pairing_pin();
        }
        let launched_via_autostart = std::env::args().any(|a| a == "--autostart");
        if app.autostart_stream_on_launch || launched_via_autostart {
            app.toggle_host();
        }
        app
    }

    /// Generate a 6-digit OS-CSPRNG-seeded PIN for first-run installs so no
    /// two machines share a static default PIN on public/campus Wi-Fi.
    ///
    /// `RandomState::new()` seeds its SipHash-1-3 keys from the OS random
    /// source (`hashmap_random_keys`); the process ID is the hashed payload.
    fn generate_initial_pairing_pin() -> String {
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};
        let mut h = RandomState::new().build_hasher();
        h.write_u64(std::process::id() as u64);
        format!("{:06}", 100_000 + (h.finish() % 900_000))
    }

    #[cfg(test)]
    fn test_instance() -> Self {
        Self::blank()
    }

    fn config_file_path() -> std::path::PathBuf {
        #[cfg(target_os = "windows")]
        if let Ok(appdata) = std::env::var("APPDATA") {
            let p = std::path::Path::new(&appdata);
            if p.is_dir() {
                return p.join("audioshare-ultra.conf");
            }
        }
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
        // Strip CR/LF from free-text fields so a pasted newline cannot inject
        // additional config keys when the file is read back.
        let clean = |s: &str| s.replace(['\r', '\n'], "");
        let ssid = clean(&self.hotspot_ssid);
        let pass = clean(&self.hotspot_pass);
        let pin = clean(&self.pairing_pin);
        let manual_ip = clean(&self.manual_server_ip);
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
            ssid,
            pass,
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
            pin,
            manual_ip,
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
        // Write to a temp file then rename so a crash cannot truncate the live
        // config, and keep the secrets owner-only on Unix.
        let path = Self::config_file_path();
        let tmp = path.with_extension("conf.tmp");
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
            {
                if f.write_all(content.as_bytes()).is_ok() {
                    let _ = std::fs::rename(&tmp, &path);
                }
            }
        }
        #[cfg(not(unix))]
        if std::fs::write(&tmp, content).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
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
                    // Pure-Rust VBR is no longer user-selectable: migrate any
                    // legacy opt-out in the config to Opus RLD on load.
                    "prefer_opus" => self.prefer_opus = true,
                    "target_bitrate_kbps" => {
                        if let Ok(n) = v.trim().parse::<u16>() {
                            self.target_bitrate_kbps = n.clamp(96, 510);
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
                            if f.is_finite() {
                                self.preamp_gain_db = f.clamp(-12.0, 24.0);
                            }
                        }
                    }
                    "eq_bass_db" => {
                        if let Ok(f) = v.trim().parse::<f32>() {
                            if f.is_finite() {
                                self.eq_bass_db = f.clamp(-12.0, 12.0);
                            }
                        }
                    }
                    "eq_presence_db" => {
                        if let Ok(f) = v.trim().parse::<f32>() {
                            if f.is_finite() {
                                self.eq_presence_db = f.clamp(-12.0, 12.0);
                            }
                        }
                    }
                    "eq_treble_db" => {
                        if let Ok(f) = v.trim().parse::<f32>() {
                            if f.is_finite() {
                                self.eq_treble_db = f.clamp(-12.0, 12.0);
                            }
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

    fn refresh_interfaces_preserving_qr(&mut self) {
        let prev_ip = self
            .interfaces
            .get(self.selected_qr_iface_idx)
            .map(|i| i.ip);
        self.interfaces = list_valid_host_interfaces();
        self.selected_qr_iface_idx = refreshed_qr_iface_index_by_ip(
            &self.interfaces,
            prev_ip,
            self.selected_qr_iface_idx,
        );
    }

    fn toggle_host(&mut self) {
        if let Some(mut host) = self.host_engine.take() {
            host.stop();
            if self.last_applied_bt_delay != 0 {
                apply_automatic_video_lipsync_offset_ms(0);
                self.last_applied_bt_delay = 0;
            }
            if self.selected_app_idx.is_some() {
                let _ = restore_all_apps_to_default_sink();
                self.selected_app_idx = None;
                self.app_route_status = "Capturing Full System Audio".into();
            }
            self.measured_sonar_ms = None;
            self.measured_sonar_at = None;
            self.prev_bytes_sent = 0;
            self.prev_pkts_sent = 0;
            self.live_kbps = 0;
            self.live_pps = 0;
            self.vu_peak_hold = 0.0;
            self.beacon = None;
            self.adb_watcher = None;
            self.os_guard = None;
            self.host_status_msg = "Host stopped.".into();
        } else {
            if self.pin_dirty {
                self.apply_pin_now();
            }
            self.refresh_interfaces_preserving_qr();
            let cfg = HostConfig {
                bind_port: DEFAULT_PORT,
                subframes_per_packet: if self.usb_low_latency_pacing { 1 } else { 2 },
                use_test_tone: self.use_test_tone,
                enable_smart_dtx: self.enable_smart_dtx,
            };

            let guard = VirtualSpeakerAndRadioGuard::acquire(self.enable_virtual_speaker);
            let effective_pin = if self.enable_pin_encryption && !self.pairing_pin.trim().is_empty()
            {
                Some(self.pairing_pin.trim())
            } else {
                None
            };

            match HostEngine::start_with_pin(cfg, vec![], effective_pin) {
                Ok(engine) => {
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
                        .effective_bitrate_kbps
                        .store(self.target_bitrate_kbps, Ordering::Relaxed);
                    engine
                        .telemetry
                        .enable_group_sync
                        .store(self.enable_group_sync, Ordering::Relaxed);
                    engine
                        .telemetry
                        .manual_group_sync_trim_ms
                        .store(self.group_sync_trim_ms, Ordering::Relaxed);
                    engine
                        .telemetry
                        .host_volume
                        .store(self.host_master_volume, Ordering::Relaxed);
                    self.os_guard = Some(guard);
                    // The beacon reads this atomic on every advertisement, so
                    // apply_pin_now can flip `enc` live without a restart.
                    self.beacon_encrypted.store(
                        effective_pin.is_some(),
                        Ordering::Relaxed,
                    );
                    self.beacon = Some(DiscoveryBeacon::start_with_encryption_flag(
                        DEFAULT_PORT,
                        "PC-AudioShare-Ultra".into(),
                        Arc::clone(&self.beacon_encrypted),
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

    fn connect_client(&mut self, addr: SocketAddr, use_pin: bool) {
        if self.pin_dirty {
            self.apply_pin_now();
        }
        if let Some(mut c) = self.client_engine.take() {
            c.stop();
        }
        self.drift_history = VecDeque::from(vec![0.0; 120]);
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
        let pin_opt = effective_client_pin(use_pin, self.enable_pin_encryption, &self.pairing_pin);
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
            .store((self.preamp_gain_db * 10.0).round() as i32, Ordering::Relaxed);
        c.telemetry
            .eq_bass_db_tenths
            .store((self.eq_bass_db * 10.0).round() as i32, Ordering::Relaxed);
        c.telemetry
            .eq_presence_db_tenths
            .store((self.eq_presence_db * 10.0).round() as i32, Ordering::Relaxed);
        c.telemetry
            .eq_treble_db_tenths
            .store((self.eq_treble_db * 10.0).round() as i32, Ordering::Relaxed);
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

    // ---------------------------------------------------------------------
    // Transient feedback + PIN plumbing (widget primitives live in
    // `ui_widgets`).
    // ---------------------------------------------------------------------

    fn show_toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    // ---------------------------------------------------------------------
    // Background worker dispatch. Each helper clones the mpsc sender into a
    // detached thread so pkexec/netsh/nmcli never block the UI. Results are
    // applied in `logic()` (see `drain_bg_tasks`).
    // ---------------------------------------------------------------------

    fn dispatch_open_firewall(&mut self, ctx: &egui::Context) {
        if self.firewall_task_in_flight {
            return;
        }
        self.firewall_task_in_flight = true;
        let tx = self.bg_task_tx.clone();
        let ctx_clone = ctx.clone();
        std::thread::spawn(move || {
            let msg = open_os_firewall_ports();
            #[cfg(target_os = "windows")]
            let firewall_ok = is_windows_firewall_configured();
            #[cfg(not(target_os = "windows"))]
            let firewall_ok = true;
            let _ = tx.send(BgTaskResult::Firewall { msg, firewall_ok });
            ctx_clone.request_repaint();
        });
        self.show_toast("Opening firewall ports (65530-65532/UDP)…");
    }

    fn dispatch_hotspot_start(&mut self, ctx: &egui::Context) {
        if self.hotspot_task_in_flight {
            return;
        }
        self.hotspot_task_in_flight = true;
        let tx = self.bg_task_tx.clone();
        let ctx_clone = ctx.clone();
        let clean = |s: &str| s.replace(['\r', '\n'], "");
        let ssid = clean(&self.hotspot_ssid);
        let pass = clean(&self.hotspot_pass);
        std::thread::spawn(move || {
            let result = DirectHotspotController::start_direct_5ghz_hotspot(&ssid, &pass);
            let _ = tx.send(BgTaskResult::HotspotStart { result });
            ctx_clone.request_repaint();
        });
        self.hotspot_status = "Starting 1-Hop Direct Hotspot…".into();
    }

    fn dispatch_hotspot_stop(&mut self, ctx: &egui::Context) {
        if self.hotspot_task_in_flight {
            return;
        }
        self.hotspot_task_in_flight = true;
        let tx = self.bg_task_tx.clone();
        let ctx_clone = ctx.clone();
        std::thread::spawn(move || {
            DirectHotspotController::stop_direct_hotspot();
            let _ = tx.send(BgTaskResult::HotspotStop);
            ctx_clone.request_repaint();
        });
        self.hotspot_status = "Stopping 1-Hop Direct Hotspot…".into();
    }

    /// Drain all worker results delivered since the previous frame and turn
    /// them into the same status text / toasts the old synchronous calls set.
    fn drain_bg_tasks(&mut self, ctx: &egui::Context) {
        while let Ok(result) = self.bg_task_rx.try_recv() {
            match result {
                BgTaskResult::Firewall { msg, firewall_ok } => {
                    self.firewall_task_in_flight = false;
                    #[cfg(target_os = "windows")]
                    {
                        self.windows_firewall_ok = firewall_ok;
                    }
                    #[cfg(not(target_os = "windows"))]
                    let _ = firewall_ok;
                    self.show_toast(msg);
                }
                BgTaskResult::FirewallProbe { firewall_ok } => {
                    #[cfg(target_os = "windows")]
                    {
                        self.windows_firewall_ok = firewall_ok;
                    }
                    #[cfg(not(target_os = "windows"))]
                    let _ = firewall_ok;
                }
                BgTaskResult::HotspotStart { result } => {
                    self.hotspot_task_in_flight = false;
                    match result {
                        Ok(msg) => {
                            self.direct_hotspot_active = true;
                            self.refresh_interfaces_preserving_qr();
                            self.hotspot_status = msg.clone();
                            self.show_toast(msg);
                        }
                        Err(e) => {
                            self.hotspot_status = format!("Hotspot notice: {}", e);
                            self.show_toast(format!("Hotspot notice: {}", e));
                        }
                    }
                }
                BgTaskResult::HotspotStop => {
                    self.hotspot_task_in_flight = false;
                    self.direct_hotspot_active = false;
                    self.refresh_interfaces_preserving_qr();
                    self.hotspot_status = "Router LAN Mode".into();
                    self.show_toast("1-Hop Direct Hotspot stopped");
                }
            }
            // A result may have arrived while the idle Host tab had no
            // repaint scheduled; make sure the banner gets drawn.
            ctx.request_repaint();
        }
    }

    /// Push the current PIN setting into every live engine at once.
    fn apply_pin_now(&mut self) {
        self.pairing_pin = self.pairing_pin.trim().to_string();
        if self.pairing_pin.is_empty() {
            // Keep the UI switch truthful: an empty PIN disables encryption.
            self.enable_pin_encryption = false;
        }
        let pin = if self.enable_pin_encryption && !self.pairing_pin.is_empty() {
            Some(self.pairing_pin.as_str())
        } else {
            None
        };

        // Derive the expensive PBKDF2 key exactly ONCE on this UI thread and
        // share the resulting SessionKey (Copy) with all three engines. The old
        // path called set_encryption_pin() three times, re-running 65,536
        // PBKDF2 rounds per engine (~3x) on every PIN apply.
        let key = pin.map(|p| {
            let mut derived = SessionKey::from_pin(p);
            // Fresh nonce salt so a live re-apply never reuses a keystream.
            derived.rotate_session_salt();
            derived
        });
        if let Some(ref host) = self.host_engine {
            host.set_session_key(key);
        }
        if let Some(ref client) = self.client_engine {
            client.set_session_key(key);
        }
        if let Some(ref mic) = self.phone_mic_receiver {
            mic.set_session_key(key);
        }
        // Advertise the new state on the very next beacon without restarting
        // the host/beacon thread.
        self.beacon_encrypted.store(pin.is_some(), Ordering::Relaxed);
        self.pin_dirty = false;
        self.save_persistent_config();
        self.show_toast(if pin.is_some() {
            "PIN encryption active"
        } else {
            "PIN encryption disabled"
        });
    }
}

impl eframe::App for AudioShareApp {
    /// Runs every frame AND while the window is hidden in the tray (no GPU pass).
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Apply results from detached firewall / hotspot workers. This must run
        // in logic() (not in a render closure) so a background UAC prompt never
        // blocks the UI and its toast shows up on the very next frame.
        self.drain_bg_tasks(ctx);

        // Expire the transient toast banner after 2.5 seconds.
        if let Some((_, created)) = &self.toast {
            if created.elapsed() >= Duration::from_millis(2500) {
                self.toast = None;
            }
        }

        let is_minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        // Only intercept close-to-minimize while the window is visible; if the
        // user explicitly right-clicks the minimized taskbar button -> "Close
        // window", let the app exit cleanly instead of swallowing the close.
        if ctx.input(|i| i.viewport().close_requested())
            && !self.force_quit
            && !is_minimized
            && self.minimize_on_close
            && (self.host_engine.is_some()
                || self.client_engine.is_some()
                || self.phone_mic_receiver.is_some())
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }

        // Surface asynchronous Windows per-app process-loopback failures or
        // isolated-process exits immediately instead of claiming isolation.
        if let Some(err) = audioshare_host_engine::app_routing::take_windows_app_isolation_error() {
            self.selected_app_idx = None;
            self.app_route_status = err.clone();
            self.show_toast(err);
        }

        // Event-driven repaint: 0 Hz on the idle Host tab (no GPU/CPU burn),
        // ~16 Hz while streaming for smooth VU/oscilloscope, 1 Hz while the
        // Receive tab scans LAN beacons, 2 Hz while hidden/minimized, and
        // 4 Hz while a toast is visible so it expires promptly.
        // Only scan LAN/USB beacons while the Receive tab is visible.
        if let Some(ref scanner) = self.scanner {
            scanner.set_paused(self.tab != AppTab::ClientReceiver || is_minimized);
        }
        let streaming_active = self.host_engine.is_some()
            || self.client_engine.is_some()
            || self.phone_mic_receiver.is_some();
        if streaming_active {
            let interval_ms = if is_minimized { 500 } else { 60 };
            ctx.request_repaint_after(Duration::from_millis(interval_ms));
        } else if self.tab == AppTab::ClientReceiver && !is_minimized {
            ctx.request_repaint_after(Duration::from_millis(1000));
        } else if self.toast.is_some()
            || self.firewall_task_in_flight
            || self.hotspot_task_in_flight
        {
            ctx.request_repaint_after(Duration::from_millis(250));
        }

        // Sample the clock-drift scope on a fixed 100 ms cadence in logic()
        // instead of at the arbitrary (frame-rate dependent) render rate.
        if let Some(ref client) = self.client_engine {
            if self.last_drift_sample.elapsed() >= Duration::from_millis(100) {
                let drift_now = client.telemetry.clock_drift_ppm.load(Ordering::Relaxed);
                self.drift_history.pop_front();
                self.drift_history.push_back(drift_now as f32);
                self.last_drift_sample = Instant::now();
            }
        }

        if let Some(ref host) = self.host_engine {
            if let Some(ref mic) = self.phone_mic_receiver {
                let tx_us = host
                    .telemetry
                    .sonar_tx_timestamp_us
                    .load(Ordering::Relaxed);
                let rx_us = mic.last_chirp_rx_timestamp_us.load(Ordering::Relaxed);
                if (tx_us, rx_us) != self.last_sonar_pair
                    && rx_us > tx_us
                    && (rx_us - tx_us) < 500_000
                {
                    self.last_sonar_pair = (tx_us, rx_us);
                    self.measured_sonar_ms = Some(((rx_us - tx_us) as f32) / 2000.0);
                    self.measured_sonar_at = Some(Instant::now());
                }
            }

            let client_count = host.telemetry.active_clients_count.load(Ordering::Relaxed);
            self.host_master_volume = host.telemetry.host_volume.load(Ordering::Relaxed);
            self.quality_profile =
                QualityProfile::from_u8(host.telemetry.quality_profile.load(Ordering::Relaxed));
            self.target_bitrate_kbps = host
                .telemetry
                .target_bitrate_kbps
                .load(Ordering::Relaxed)
                .clamp(96, 510);

            let pkts = host.telemetry.packets_sent.load(Ordering::Relaxed);
            let bytes_now = host.telemetry.bytes_sent.load(Ordering::Relaxed);
            // Include Wi-Fi FIFO and packetization, not just the raw A2DP
            // estimate, so video lands in sync with the audible stream.
            let total_lipsync_ms = host.telemetry.video_lipsync_offset_ms();

            if self.last_rate_calc.elapsed() >= Duration::from_secs(1) {
                let dt = self.last_rate_calc.elapsed().as_secs_f32().max(0.1);
                self.live_kbps = (((bytes_now.saturating_sub(self.prev_bytes_sent)) as f32 * 8.0)
                    / (dt * 1000.0)) as u32;
                self.live_pps = ((pkts.saturating_sub(self.prev_pkts_sent)) as f32 / dt) as u32;
                self.prev_bytes_sent = bytes_now;
                self.prev_pkts_sent = pkts;
                self.last_rate_calc = Instant::now();
            }

            let lipsync = lipsync_offset_to_apply(
                self.auto_bt_video_lipsync,
                total_lipsync_ms,
                self.last_applied_bt_delay,
            );
            if let Some(offset) = lipsync {
                apply_automatic_video_lipsync_offset_ms(offset);
                self.last_applied_bt_delay = offset;
            }

            if let Some(ref mut guard) = self.os_guard {
                // On Linux, Virtual Speaker and Per-App Capture share
                // `AudioShare_Ultra`: do not overwrite `@DEFAULT_SINK@` /
                // `@DEFAULT_SOURCE@` while a single app is isolated.
                // On Windows, Virtual Speaker and Phone Virtual Mic share the
                // single VB-Cable endpoint: pause Virtual Speaker while Phone
                // Virtual Mic is active so call audio cannot feed back into
                // VB-Cable in a howling loop.
                #[cfg(target_os = "windows")]
                let virtual_speaker_blocked = self.phone_mic_receiver.is_some()
                    && self.phone_rx_mode == PhoneRxRoutingMode::VirtualMicForCalls;
                #[cfg(target_os = "linux")]
                let virtual_speaker_blocked = self.selected_app_idx.is_some();
                #[cfg(not(any(target_os = "windows", target_os = "linux")))]
                let virtual_speaker_blocked = false;

                if client_count > 0 && !virtual_speaker_blocked {
                    guard.activate();
                } else {
                    guard.deactivate();
                }
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Keyboard shortcuts: Esc closes modals, Ctrl+, toggles Settings,
        // F12 toggles the latency HUD.
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if self.show_settings_modal {
                if self.pin_dirty {
                    self.apply_pin_now();
                } else {
                    self.save_persistent_config();
                }
                self.show_settings_modal = false;
            } else if self.show_qr_modal {
                self.show_qr_modal = false;
            } else if self.latency_debug {
                self.latency_debug = false;
            }
        }
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Comma)) {
            self.refresh_interfaces_preserving_qr();
            if self.show_settings_modal {
                if self.pin_dirty {
                    self.apply_pin_now();
                } else {
                    self.save_persistent_config();
                }
            }
            self.show_settings_modal = !self.show_settings_modal;
            if self.show_settings_modal {
                self.show_qr_modal = false;
            }
        }
        if ctx.input(|i| i.key_pressed(egui::Key::F12)) {
            self.latency_debug = !self.latency_debug;
        }

        let dark = self.dark_mode;
        let p = uw::Palette::for_mode(dark);

        // =========================================================================
        // 1. TOP NAVIGATION BAR (Minimalist & Intent-Driven)
        // =========================================================================
        egui::Panel::top("top_nav")
            .frame(
                egui::Frame::NONE
                    .fill(if dark {
                        egui::Color32::from_rgb(14, 18, 27)
                    } else {
                        egui::Color32::from_rgb(255, 255, 255)
                    })
                    .inner_margin(egui::Margin::symmetric(18, 10))
                    .stroke(egui::Stroke::new(
                        1.0_f32,
                        if dark {
                            egui::Color32::from_rgb(28, 36, 52)
                        } else {
                            egui::Color32::from_rgb(220, 226, 236)
                        },
                    )),
            )
            .show(ui, |ui| {
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

                    if uw::pill_choice(ui, dark, self.tab == AppTab::HostSender, "Send PC Audio")
                    {
                        self.tab = AppTab::HostSender;
                    }
                    if uw::pill_choice(
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
                        if uw::pill_choice(ui, dark, self.show_settings_modal, "Settings") {
                            self.refresh_interfaces_preserving_qr();
                            if self.show_settings_modal {
                                if self.pin_dirty {
                                    self.apply_pin_now();
                                } else {
                                    self.save_persistent_config();
                                }
                            }
                            self.show_settings_modal = !self.show_settings_modal;
                            if self.show_settings_modal {
                                self.show_qr_modal = false;
                            }
                        }
                        if uw::pill_choice(ui, dark, self.show_qr_modal, "Pair Phone QR") {
                            if self.pin_dirty {
                                self.apply_pin_now();
                            }
                            self.refresh_interfaces_preserving_qr();
                            self.show_qr_modal = !self.show_qr_modal;
                            if self.show_qr_modal {
                                self.show_settings_modal = false;
                            }
                        }
                        // Clickable PIN ON indicator — jumps straight to Security.
                        if self.enable_pin_encryption && !self.pairing_pin.trim().is_empty() {
                            let p = uw::Palette::for_mode(dark);
                            let resp = egui::Frame::NONE
                                .fill(p.ok_bg)
                                .corner_radius(egui::CornerRadius::same(6))
                                .inner_margin(egui::Margin::symmetric(8, 4))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        let (lock_rect, _) = ui.allocate_exact_size(
                                            egui::vec2(10.0, 12.0),
                                            egui::Sense::hover(),
                                        );
                                        uw::paint_lock_icon(ui.painter(), lock_rect.center(), p.ok_fg);
                                        ui.label(
                                            egui::RichText::new("PIN ON")
                                                .size(11.5)
                                                .strong()
                                                .color(p.ok_fg),
                                        );
                                    });
                                })
                                .response;
                            if ui
                                .interact(resp.rect, resp.id.with("pin_badge"), egui::Sense::click())
                                .clicked()
                            {
                                self.settings_tab = SettingsTab::NetworkAndSecurity;
                                self.show_settings_modal = true;
                                self.show_qr_modal = false;
                            }
                        }
                    });
                });
            });

        // =========================================================================
        // 2. BOTTOM STATUS & TELEMETRY BAR (Quiet 1-Line Summary + HUD Trigger)
        // =========================================================================
        egui::Panel::bottom("bottom_status")
            .frame(
                egui::Frame::NONE
                    .fill(if dark {
                        egui::Color32::from_rgb(13, 17, 25)
                    } else {
                        egui::Color32::from_rgb(248, 250, 252)
                    })
                    .inner_margin(egui::Margin::symmetric(18, 7))
                    .stroke(egui::Stroke::new(
                        1.0_f32,
                        if dark {
                            egui::Color32::from_rgb(28, 36, 52)
                        } else {
                            egui::Color32::from_rgb(220, 226, 236)
                        },
                    )),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if let Some(ref host) = self.host_engine {
                        let dtx = host.telemetry.dtx_silent_mode.load(Ordering::Relaxed);
                        let mode_id = host.telemetry.active_codec_mode.load(Ordering::Relaxed);
                        let clients = host.telemetry.active_clients_count.load(Ordering::Relaxed);
                        let kb = host.telemetry.bytes_sent.load(Ordering::Relaxed) / 1024;
                        let display_kbps = host
                            .telemetry
                            .effective_bitrate_kbps
                            .load(Ordering::Relaxed);
                        let codec_label = if dtx {
                            "Smart DTX Sleep (0.6 kbps)".to_string()
                        } else if mode_id == 4 {
                            format!("Opus RLD ({} kbps)", display_kbps)
                        } else if mode_id == 2 {
                            format!("Rust VBR ({} kbps)", display_kbps)
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
                            .color(p.ok_fg),
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
                            .color(if dark {
                                egui::Color32::from_rgb(147, 197, 253)
                            } else {
                                egui::Color32::from_rgb(29, 78, 216)
                            }),
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
                            .color(p.muted_text),
                        );
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if uw::pill_choice(
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
                        if let Some((ref msg, _)) = self.toast {
                            let p = uw::Palette::for_mode(dark);
                            uw::status_pill(ui, msg, p.ok_bg, p.ok_fg);
                        }
                    });
                });
            });

        // =========================================================================
        // 3. MODAL WINDOWS: QR Pairing, System/Network Settings, and Latency HUD
        // =========================================================================
        if self.show_qr_modal {
            self.render_qr_modal(&ctx);
        }

        if self.show_settings_modal {
            self.render_settings_modal(&ctx);
        }

        if self.latency_debug {
            let mut open = self.latency_debug;
            egui::Window::new("Live Stage-by-Stage Latency Breakdown")
                .open(&mut open)
                .default_width(600.0)
                .default_height(460.0)
                .vscroll(true)
                .resizable(true)
                .show(&ctx, |ui| self.render_latency_debug(ui));
            self.latency_debug = open;
        }

        egui::CentralPanel::default().show(ui, |ui| {
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

    /// eframe 0.37 defaults to the wgpu backend (no `glow` feature), so
    /// `on_exit` no longer receives a glow context.
    fn on_exit(&mut self) {
        if self.pin_dirty {
            self.pairing_pin = self.pairing_pin.trim().to_string();
        }
        self.save_persistent_config();
        if self.last_applied_bt_delay != 0 {
            // Synchronous reset: a detached thread would be killed by process
            // exit and leave the system speaker with a stale +Nms offset.
            reset_video_lipsync_offset_blocking();
            self.last_applied_bt_delay = 0;
        }
        if self.direct_hotspot_active || self.hotspot_task_in_flight {
            DirectHotspotController::stop_direct_hotspot();
            self.direct_hotspot_active = false;
            self.hotspot_task_in_flight = false;
        }
        if let Some(mut host) = self.host_engine.take() {
            host.stop();
        }
        if self.selected_app_idx.is_some() {
            let _ = restore_all_apps_to_default_sink();
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
        if self.pin_dirty {
            self.apply_pin_now();
        }
        let dark = self.dark_mode;
        let p = uw::Palette::for_mode(dark);
        let mut open = self.show_qr_modal;
        egui::Window::new("Pair Android Phone via QR Code")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                uw::studio_card(ui, dark, |ui| {
                    let pin = self.pairing_pin.trim();
                    let pin_opt = if self.enable_pin_encryption && !pin.is_empty() {
                        Some(pin)
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
                                    if uw::pill_choice(
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
                        if let Some(primary_ip) = self
                            .interfaces
                            .get(self.selected_qr_iface_idx)
                            .or_else(|| self.interfaces.first())
                            .map(|i| i.ip.to_string())
                        {
                            let uri = build_qr_connect_uri(&primary_ip, DEFAULT_PORT, pin_opt);
                            ui.label("Scan with your Android Camera or the AudioShare Ultra app:");
                            ui.add_space(8.0);
                            uw::draw_qr_code(ui, &uri, &mut self.qr_cache);
                            ui.add_space(8.0);
                            uw::status_pill(
                                ui,
                                &uri,
                                egui::Color32::from_rgb(15, 23, 42),
                                egui::Color32::from_rgb(147, 197, 253),
                            );
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                if ui.button("Copy Connection URI").clicked() {
                                    ui.ctx().copy_text(uri.clone());
                                    // Disjoint field write: `pin_opt` still borrows
                                    // `self.pairing_pin` until the URI is built.
                                    self.toast =
                                        Some(("Copied Connection URI".into(), Instant::now()));
                                }
                            });
                        } else {
                            ui.add_space(8.0);
                            ui.label(
                                egui::RichText::new(
                                    "No active LAN or USB adapter found — connect a network adapter and reopen pairing.",
                                )
                                .size(12.0)
                                .color(p.warn_fg),
                            );
                        }
                    });
                });
            });
        self.show_qr_modal = open;
    }

    fn render_settings_modal(&mut self, ctx: &egui::Context) {
        let dark = self.dark_mode;
        let p = uw::Palette::for_mode(dark);
        let mut open = self.show_settings_modal;
        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .vscroll(true)
            .default_width(560.0)
            .max_height((ctx.content_rect().height() - 64.0).max(260.0))
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                // Two-tab modal replaces the old single scrolling settings wall.
                ui.horizontal(|ui| {
                    if uw::pill_choice(
                        ui,
                        dark,
                        self.settings_tab == SettingsTab::NetworkAndSecurity,
                        "Network & Security",
                    ) {
                        self.settings_tab = SettingsTab::NetworkAndSecurity;
                    }
                    if uw::pill_choice(
                        ui,
                        dark,
                        self.settings_tab == SettingsTab::GeneralAndDiagnostics,
                        "General & Diagnostics",
                    ) {
                        self.settings_tab = SettingsTab::GeneralAndDiagnostics;
                    }
                });
                ui.add_space(6.0);

                uw::studio_card(ui, dark, |ui| match self.settings_tab {
                    SettingsTab::NetworkAndSecurity => {
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
                                    uw::status_pill(
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
                            let hs_btn = if self.hotspot_task_in_flight {
                                "Working on 1-Hop Hotspot…"
                            } else if self.direct_hotspot_active {
                                "Stop 1-Hop Direct Hotspot"
                            } else {
                                "Start 1-Hop 5GHz Link"
                            };
                            if ui
                                .add_enabled(!self.hotspot_task_in_flight, egui::Button::new(hs_btn))
                                .clicked()
                            {
                                // nmcli / netsh can take seconds; run off the UI
                                // thread and flip state when the result arrives.
                                if self.direct_hotspot_active {
                                    self.dispatch_hotspot_stop(ctx);
                                } else {
                                    self.dispatch_hotspot_start(ctx);
                                }
                            }
                            let fw_btn = if self.firewall_task_in_flight {
                                "Opening Firewall…"
                            } else {
                                "1-Click Open Firewall (65530-65532)"
                            };
                            if ui
                                .add_enabled(
                                    !self.firewall_task_in_flight,
                                    egui::Button::new(fw_btn),
                                )
                                .clicked()
                            {
                                self.dispatch_open_firewall(ctx);
                            }
                        });
                        ui.label(
                            egui::RichText::new(&self.hotspot_status)
                                .size(11.0)
                                .color(p.muted_text),
                        );

                        ui.separator();
                        ui.label(
                            egui::RichText::new("Stream Security (ChaCha20-Poly1305 AEAD)")
                                .size(13.5)
                                .strong(),
                        );
                        if uw::setting_switch_row(
                            ui,
                            dark,
                            "Require Pairing PIN Encryption",
                            "Encrypts all audio and control packets with RFC 8439 ChaCha20-Poly1305 + anti-replay",
                            &mut self.enable_pin_encryption,
                        ) {
                            self.apply_pin_now();
                        }
                        ui.horizontal(|ui| {
                            ui.label("Pairing PIN:");
                            let resp = ui.add_sized(
                                [110.0, 24.0],
                                egui::TextEdit::singleline(&mut self.pairing_pin),
                            );
                            if resp.changed() {
                                self.pin_dirty = true;
                            }
                            let enter_pressed =
                                resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                            let lbl = if self.pin_dirty {
                                "Save & Apply PIN *"
                            } else {
                                "Apply PIN"
                            };
                            if ui.button(lbl).clicked() || enter_pressed {
                                self.apply_pin_now();
                            }
                            if ui.button("New PIN").clicked() {
                                self.pairing_pin = Self::generate_initial_pairing_pin();
                                // Clicking New PIN implies the user intends to use it;
                                // enable and apply immediately so PC & Android never disagree.
                                self.enable_pin_encryption = true;
                                self.apply_pin_now();
                            }
                        });
                    }
                    SettingsTab::GeneralAndDiagnostics => {
                        ui.label(
                            egui::RichText::new("Application & OS Preferences")
                                .size(13.5)
                                .strong(),
                        );
                        uw::setting_switch_row(
                            ui,
                            dark,
                            "Auto-Start Broadcast on App Launch",
                            "Immediately start Host Server when AudioShare Ultra opens",
                            &mut self.autostart_stream_on_launch,
                        );
                        if uw::setting_switch_row(
                            ui,
                            dark,
                            "Start App on OS Login",
                            "Register AudioShare Ultra in desktop session startup",
                            &mut self.os_boot_autostart,
                        ) {
                            let msg = set_os_boot_autostart(self.os_boot_autostart);
                            self.show_toast(msg);
                        }
                        uw::setting_switch_row(
                            ui,
                            dark,
                            "Minimize on Window Close",
                            "Keep active audio stream running in background when closing window",
                            &mut self.minimize_on_close,
                        );
                        if uw::setting_switch_row(
                            ui,
                            dark,
                            "Dark Studio Theme",
                            "Use high-contrast obsidian slate palette",
                            &mut self.dark_mode,
                        ) {
                            uw::apply_studio_theme(ctx, self.dark_mode);
                        }

                        ui.add_space(8.0);
                        ui.separator();
                        ui.label(
                            egui::RichText::new("Diagnostics").size(13.5).strong(),
                        );
                        ui.label(
                            egui::RichText::new(format!(
                                "AudioShare Ultra v{} ({}/{})",
                                env!("CARGO_PKG_VERSION"),
                                std::env::consts::OS,
                                std::env::consts::ARCH,
                            ))
                            .size(11.5)
                            .color(p.muted_text),
                        );
                        if ui.button("Copy Diagnostics Report").clicked() {
                            let mut summary = format!(
                                "HostMsg : {}\nClientMg: {}",
                                self.host_status_msg, self.client_status_msg
                            );
                            if let Some(ref host) = self.host_engine {
                                summary.push('\n');
                                summary.push_str(&host.telemetry.diagnostics_summary());
                                for c in host.connected_clients() {
                                    summary.push_str(&format!(
                                        "\nPeer    : {} usb={} loss={:.1}% bt={}ms hw={}ms gaming={} muted={}",
                                        c.addr,
                                        c.is_usb,
                                        c.fec_rate_permille as f32 / 10.0,
                                        c.bluetooth_delay_ms,
                                        c.hardware_delay_ms,
                                        c.gaming_lock,
                                        c.muted,
                                    ));
                                }
                            }
                            if let Some(ref client) = self.client_engine {
                                summary.push('\n');
                                summary.push_str(
                                    &client
                                        .telemetry
                                        .diagnostics_summary(DESKTOP_OUTPUT_BUFFER_MS),
                                );
                            }
                            if let Some(ref mic) = self.phone_mic_receiver {
                                summary.push_str(&format!(
                                    "\nPhoneRx : mode={:?} out={} rx_pkts={} peak={}% auth_fail={}",
                                    self.phone_rx_mode,
                                    mic.output_device_name,
                                    mic.mic_packets_received.load(Ordering::Relaxed),
                                    mic.mic_peak_level_pct.load(Ordering::Relaxed),
                                    mic.auth_failures.load(Ordering::Relaxed),
                                ));
                            }
                            #[cfg(target_os = "windows")]
                            summary.push_str(&format!(
                                "\nWinFW   : configured={}",
                                self.windows_firewall_ok
                            ));
                            ui.ctx().copy_text(
                                audioshare_protocol::diagnostics::export_report(&summary),
                            );
                            self.show_toast("Diagnostics copied to clipboard");
                        }
                    }
                });
            });
        if !open && self.show_settings_modal {
            if self.pin_dirty {
                self.apply_pin_now();
            } else {
                self.save_persistent_config();
            }
        }
        self.show_settings_modal = open;
    }

    fn render_host_tab(&mut self, ui: &mut egui::Ui) {
        let is_running = self.host_engine.is_some();
        let dark = self.dark_mode;
        let p = uw::Palette::for_mode(dark);
        let vu_pct = self
            .host_engine
            .as_ref()
            .map(|h| h.telemetry.host_vu_peak_pct.load(Ordering::Relaxed))
            .unwrap_or(0);
        // Decaying studio peak-hold marker: snap up instantly, fall slowly.
        if vu_pct as f32 >= self.vu_peak_hold {
            self.vu_peak_hold = vu_pct as f32;
        } else {
            self.vu_peak_hold = (self.vu_peak_hold - 1.8).max(0.0);
        }

        // =========================================================================
        // 1. ATMOSPHERIC HERO AUDIO STAGE: CTA, Peak LED Meter & Master Volume
        // =========================================================================
        uw::hero_stage_card(ui, dark, is_running, vu_pct, |ui| {
            ui.horizontal(|ui| {
                if uw::primary_cta_button(ui, is_running, "Start Broadcast", "Stop Broadcast") {
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
                                p.ok_fg
                            } else {
                                p.muted_text
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
                        .color(p.muted_text),
                    );
                });
            });

            ui.add_space(6.0);
            ui.separator();

            let bt_delay = self
                .host_engine
                .as_ref()
                .map(|h| h.telemetry.max_client_bt_delay_ms.load(Ordering::Relaxed))
                .unwrap_or(0);
            let total_lipsync = self
                .host_engine
                .as_ref()
                .map(|h| h.telemetry.video_lipsync_offset_ms())
                .unwrap_or(0);

            ui.columns(2, |cols| {
                cols[0].horizontal(|ui| {
                    ui.label(egui::RichText::new("Audio Peak").size(12.5).strong());
                    uw::draw_led_vu_meter(ui, vu_pct, self.vu_peak_hold);
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

            // Surface Virtual Speaker right on the Hero card to eliminate the
            // first-run dual-speaker echo trap (no need to open a pro drawer).
            ui.add_space(4.0);
            if uw::setting_switch_row(
                ui,
                dark,
                "Mute PC Speakers While Streaming (Virtual Speaker)",
                "Routes PC audio exclusively to connected devices and restores speakers on disconnect",
                &mut self.enable_virtual_speaker,
            ) {
                if self.host_engine.is_some() {
                    let _ = self.os_guard.take();
                    self.os_guard = Some(VirtualSpeakerAndRadioGuard::acquire(
                        self.enable_virtual_speaker,
                    ));
                }
                self.save_persistent_config();
            }

            if bt_delay > 40 {
                ui.add_space(2.0);
                #[cfg(target_os = "linux")]
                let bt_pill_text = if self.auto_bt_video_lipsync {
                    format!(
                        "Phone Bluetooth Active (A2DP {} ms -> Total Video Offset +{} ms)",
                        bt_delay, total_lipsync
                    )
                } else {
                    format!("Phone Bluetooth Active (A2DP {} ms)", bt_delay)
                };
                #[cfg(not(target_os = "linux"))]
                let bt_mode_label = if bt_delay <= 65 {
                    "LE/Low-Latency"
                } else {
                    "A2DP"
                };
                #[cfg(not(target_os = "linux"))]
                let bt_pill_text = format!(
                    "Phone Bluetooth Active ({} {} ms • Total Stream Delay ~{} ms)",
                    bt_mode_label, bt_delay, total_lipsync
                );
                let _ = total_lipsync;
                ui.horizontal_wrapped(|ui| {
                    uw::status_pill(ui, &bt_pill_text, p.warn_bg, p.warn_fg);
                    #[cfg(target_os = "linux")]
                    if !self.auto_bt_video_lipsync
                        && ui.small_button("Enable Video Lip-Sync").clicked()
                    {
                        self.auto_bt_video_lipsync = true;
                        self.save_persistent_config();
                    }
                    #[cfg(target_os = "windows")]
                    if let Some(hint) = format_windows_bt_lipsync_hint(u32::from(
                        if total_lipsync > 0 {
                            total_lipsync
                        } else {
                            bt_delay
                        },
                    )) {
                        ui.label(
                            egui::RichText::new(hint)
                                .size(11.5)
                                .color(p.muted_text),
                        );
                    }
                });
            }

            #[cfg(target_os = "windows")]
            {
                if !self.windows_firewall_ok {
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        uw::status_pill(
                            ui,
                            "Windows Firewall may block phone connections (especially on Public/Hotspot)",
                            p.warn_bg,
                            p.warn_fg,
                        );
                        let fw_lbl = if self.firewall_task_in_flight {
                            "Opening Firewall…"
                        } else {
                            "Allow Through Firewall (UAC)"
                        };
                        if ui
                            .add_enabled(!self.firewall_task_in_flight, egui::Button::new(fw_lbl))
                            .clicked()
                        {
                            // pkexec/netsh can block for seconds — dispatch to
                            // a worker; logic() applies the result + toast.
                            let ctx = ui.ctx().clone();
                            self.dispatch_open_firewall(&ctx);
                        }
                    });
                }

                if self.enable_virtual_speaker
                    && self
                        .os_guard
                        .as_ref()
                        .map(|g| !g.is_virtual_speaker_available())
                        .unwrap_or(false)
                {
                    ui.add_space(2.0);
                    uw::status_pill(
                        ui,
                        "Virtual Speaker not found — install VB-Cable (CABLE Input) or mute PC speakers",
                        p.warn_bg,
                        p.warn_fg,
                    );
                }
            }

            let rejected = self
                .host_engine
                .as_ref()
                .map(|h| h.telemetry.rejected_handshakes.load(Ordering::Relaxed))
                .unwrap_or(0);
            if rejected > 0 {
                ui.add_space(2.0);
                uw::status_pill(
                    ui,
                    &format!(
                        "Rejected {} client handshake(s) — check matching PIN & app version (v2)",
                        rejected
                    ),
                    p.warn_bg,
                    p.warn_fg,
                );
            }
        });

        ui.add_space(8.0);

        // =========================================================================
        // 2. SLIDING SEGMENTED MODE BAR (zero-scroll "Simple First" view)
        // =========================================================================
        let is_lossless = matches!(
            self.quality_profile,
            QualityProfile::StudioLossless | QualityProfile::StudioLossless24Bit
        );
        let active_idx = match self.quality_profile {
            QualityProfile::SmartAuto => 0,
            QualityProfile::StudioLossless | QualityProfile::StudioLossless24Bit => 1,
            QualityProfile::LowLatencyCompressed => 2,
            QualityProfile::MaxStability24Ghz => 3,
        };
        let lossless_badge = if self.quality_profile == QualityProfile::StudioLossless24Bit {
            "24-Bit"
        } else {
            "16-Bit"
        };
        let smooth_badge = format!("Opus {}k", self.target_bitrate_kbps);
        let mode_bar_id = ui.make_persistent_id("host_quality_mode_bar");
        if let Some(idx) = uw::segmented_mode_bar(
            ui,
            dark,
            active_idx,
            &[
                ("Smart Auto", "Recommended"),
                ("Studio Lossless", lossless_badge),
                ("Smooth Wi-Fi", &smooth_badge),
                ("Max Range", "10 ms Walls"),
            ],
            mode_bar_id,
        ) {
            match idx {
                0 => self.quality_profile = QualityProfile::SmartAuto,
                1 => {
                    // Preserve an explicit 24-bit master selection.
                    if !is_lossless {
                        self.quality_profile = QualityProfile::StudioLossless;
                    }
                }
                2 => self.quality_profile = QualityProfile::LowLatencyCompressed,
                3 => self.quality_profile = QualityProfile::MaxStability24Ghz,
                _ => {}
            }
            if let Some(ref host) = self.host_engine {
                host.telemetry
                    .quality_profile
                    .store(self.quality_profile as u8, Ordering::Relaxed);
            }
        }

        ui.add_space(8.0);

        // =========================================================================
        // 3. CONNECTED DEVICES CARD (Always Visible, Clean Empty State)
        // =========================================================================
        uw::studio_card(ui, dark, |ui| {
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
                        self.refresh_interfaces_preserving_qr();
                        self.show_qr_modal = true;
                        self.show_settings_modal = false;
                    }
                });
            });

            if clients.is_empty() {
                ui.label(
                    egui::RichText::new(
                        "No receivers connected — tap this PC in the Android app or click 'Pair New Phone (QR)'.",
                    )
                    .size(12.0)
                    .color(p.muted_text),
                );
            } else if let Some(ref host) = self.host_engine {
                ui.separator();
                for client in clients {
                    ui.push_id(client.addr, |ui| {
                        ui.horizontal(|ui| {
                            // Link-health dot: green (<1% loss), amber (1-5%), red (>5%).
                            let (dot_rect, _) =
                                ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                            let health_col = if client.fec_rate_permille < 10 {
                                egui::Color32::from_rgb(16, 185, 129)
                            } else if client.fec_rate_permille < 50 {
                                egui::Color32::from_rgb(245, 158, 11)
                            } else {
                                egui::Color32::from_rgb(239, 68, 68)
                            };
                            ui.painter().circle_filled(dot_rect.center(), 4.0, health_col);
                            let badge = if client.is_usb { "USB" } else { "Wi-Fi" };
                            uw::status_pill(
                                ui,
                                badge,
                                egui::Color32::from_rgb(30, 58, 138),
                                egui::Color32::WHITE,
                            );
                            ui.monospace(format!(
                                "{} • Vol {}% • Loss {:.1}% • BT {} ms",
                                client.addr,
                                client.client_volume,
                                (client.fec_rate_permille as f32) / 10.0,
                                client.bluetooth_delay_ms
                            ));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.small_button("Kick").clicked() {
                                    host.kick_client(client.addr);
                                }
                                let mute_label = if client.muted { "Unmute" } else { "Mute" };
                                if ui.small_button(mute_label).clicked() {
                                    host.set_client_muted(client.addr, !client.muted);
                                }
                            });
                        });
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
            QualityProfile::LowLatencyCompressed => "Compressed Opus",
            QualityProfile::MaxStability24Ghz => "10ms 2.4G",
        };
        let engine_badge = "Opus RLD";
        let pacing_badge = if self.usb_low_latency_pacing {
            "2.5ms USB"
        } else {
            "5.0ms Wi-Fi"
        };
        uw::drawer_header(
            ui,
            dark,
            &mut self.host_pro_codec_open,
            "Pro Codec, Bitrate & Sync Tuning",
            &[profile_badge, engine_badge, pacing_badge],
        );

        if self.host_pro_codec_open {
            ui.add_space(4.0);
            uw::studio_card(ui, dark, |ui| {
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
                        if uw::pill_choice(ui, dark, self.quality_profile == profile, label) {
                            self.quality_profile = profile;
                            if let Some(ref host) = self.host_engine {
                                host.telemetry
                                    .quality_profile
                                    .store(self.quality_profile as u8, Ordering::Relaxed);
                            }
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
                        &[96, 160, 256, 320, 510]
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
                            if uw::pill_choice(
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
                            .color(p.muted_text),
                        );
                    }
                }

                ui.add_space(4.0);
                ui.separator();

                ui.columns(2, |cols| {
                    cols[0].vertical(|ui| {
                        if uw::setting_switch_row(
                            ui,
                            dark,
                            "2.5 ms USB Frame Pacing",
                            "Single-subframe packets (OFF = 5.0 ms bundled Wi-Fi)",
                            &mut self.usb_low_latency_pacing,
                        ) {
                            if let Some(ref host) = self.host_engine {
                                host.telemetry.configured_subframes_per_packet.store(
                                    if self.usb_low_latency_pacing { 1 } else { 2 },
                                    Ordering::Relaxed,
                                );
                            }
                        }
                        if uw::setting_switch_row(
                            ui,
                            dark,
                            "Smart DTX Silence Saver",
                            "Halts audio packets during silence (0.6 kbps heartbeat)",
                            &mut self.enable_smart_dtx,
                        ) {
                            if let Some(ref host) = self.host_engine {
                                host.telemetry
                                    .enable_smart_dtx
                                    .store(self.enable_smart_dtx, Ordering::Relaxed);
                            }
                        }
                    });
                    cols[1].vertical(|ui| {
                        #[cfg(target_os = "linux")]
                        uw::setting_switch_row(
                            ui,
                            dark,
                            "Auto Bluetooth Video Lip-Sync",
                            "Applies PipeWire offset matching phone Bluetooth delay",
                            &mut self.auto_bt_video_lipsync,
                        );
                        if uw::setting_switch_row(
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
                        if uw::setting_switch_row(
                            ui,
                            dark,
                            "440 Hz / 554 Hz Test Chord",
                            "Synthesizes reference tone instead of system capture",
                            &mut self.use_test_tone,
                        ) {
                            if let Some(ref host) = self.host_engine {
                                host.telemetry
                                    .use_test_tone
                                    .store(self.use_test_tone, Ordering::Relaxed);
                            }
                        }
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
        uw::drawer_header(
            ui,
            dark,
            &mut self.host_routing_mic_open,
            "App Audio Picker & Phone Microphone (Port 65532)",
            &[source_summary, mic_summary],
        );

        if self.host_routing_mic_open {
            ui.add_space(4.0);
            uw::studio_card(ui, dark, |ui| {
                ui.columns(2, |cols| {
                    // Left: Per-App Capture
                    cols[0].vertical(|ui| {
                        ui.label(
                            egui::RichText::new("Per-Application Audio Capture")
                                .size(13.0)
                                .strong(),
                        );
                        ui.horizontal_wrapped(|ui| {
                            if uw::pill_choice(
                                ui,
                                dark,
                                self.selected_app_idx.is_none(),
                                "All System Audio",
                            ) {
                                self.selected_app_idx = None;
                                self.app_route_status = restore_all_apps_to_default_sink();
                            }
                            if ui.button("Refresh Apps").clicked() {
                                let prev_selected = self
                                    .selected_app_idx
                                    .and_then(|idx| self.audio_apps.get(idx).map(|a| (a.id, a.pid)));
                                self.audio_apps = list_active_audio_apps();
                                self.selected_app_idx = prev_selected.and_then(|(id, pid)| {
                                    self.audio_apps
                                        .iter()
                                        .position(|a| a.id == id)
                                        .or_else(|| {
                                            if pid != 0 {
                                                self.audio_apps.iter().position(|a| a.pid == pid)
                                            } else {
                                                None
                                            }
                                        })
                                });
                                if prev_selected.is_some() && self.selected_app_idx.is_none() {
                                    self.app_route_status = restore_all_apps_to_default_sink();
                                }
                            }
                        });
                        if !self.audio_apps.is_empty() {
                            ui.horizontal_wrapped(|ui| {
                                for (idx, app) in self.audio_apps.iter().enumerate() {
                                    let selected = self.selected_app_idx == Some(idx);
                                    if uw::pill_choice(
                                        ui,
                                        dark,
                                        selected,
                                        &format!("{} ({})", app.name, app.pid),
                                    ) {
                                        match route_single_app_to_audioshare(app) {
                                            Ok(msg) => {
                                                self.selected_app_idx = Some(idx);
                                                self.app_route_status = msg;
                                            }
                                            Err(err) => self.app_route_status = err,
                                        }
                                    }
                                }
                            });
                        }
                        ui.label(
                            egui::RichText::new(&self.app_route_status)
                                .size(11.0)
                                .color(p.muted_text),
                        );
                    });

                    // Right: Phone -> PC Receiver
                    cols[1].vertical(|ui| {
                        ui.label(
                            egui::RichText::new("Phone-to-PC Receiver (Port 65532)")
                                .size(13.0)
                                .strong(),
                        );
                        let prev_mode = self.phone_rx_mode;
                        let mut mode_changed = false;
                        ui.horizontal_wrapped(|ui| {
                            if uw::pill_choice(
                                ui,
                                dark,
                                self.phone_rx_mode == PhoneRxRoutingMode::VirtualMicForCalls,
                                "Virtual Mic (Zoom / Discord)",
                            ) && self.phone_rx_mode != PhoneRxRoutingMode::VirtualMicForCalls
                            {
                                self.phone_rx_mode = PhoneRxRoutingMode::VirtualMicForCalls;
                                mode_changed = true;
                            }
                            if uw::pill_choice(
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
                            // Pre-flight check on Windows so switching to
                            // Virtual Mic without VB-Cable never destroys a
                            // working `PlayOnPcSpeakers` stream.
                            #[cfg(target_os = "windows")]
                            let vbcable_missing = self.phone_rx_mode
                                == PhoneRxRoutingMode::VirtualMicForCalls
                                && audioshare_host_engine::find_vbcable_output_device_name()
                                    .is_none();
                            #[cfg(not(target_os = "windows"))]
                            let vbcable_missing = false;

                            if vbcable_missing {
                                self.phone_rx_mode = prev_mode;
                                let msg = "VB-Cable not found — kept 'Play on PC Speakers' active.";
                                self.phone_rx_status = format!("Error: {}", msg);
                                self.show_toast(msg);
                            } else {
                                if self.pin_dirty {
                                    self.apply_pin_now();
                                }
                                if let Some(mut m) = self.phone_mic_receiver.take() {
                                    m.stop();
                                }
                                let phone_pin = if self.enable_pin_encryption
                                    && !self.pairing_pin.trim().is_empty()
                                {
                                    Some(self.pairing_pin.trim().to_string())
                                } else {
                                    None
                                };
                                match PhoneMicReceiver::start_with_mode_and_pin(
                                    PHONE_MIC_PORT,
                                    self.phone_rx_mode,
                                    phone_pin.as_deref(),
                                ) {
                                    Ok(rx) => {
                                        self.phone_rx_status = if self.phone_rx_mode
                                            == PhoneRxRoutingMode::VirtualMicForCalls
                                        {
                                            #[cfg(target_os = "windows")]
                                            {
                                                format!(
                                                    "Listening (UDP/USB 65532) -> {} • In Zoom/Discord/Windows select 'CABLE Output (VB-Audio Virtual Cable)' as Microphone",
                                                    rx.output_device_name
                                                )
                                            }
                                            #[cfg(not(target_os = "windows"))]
                                            {
                                                format!(
                                                    "Listening (UDP/USB 65532) -> {}",
                                                    rx.output_device_name
                                                )
                                            }
                                        } else {
                                            format!(
                                                "Listening (UDP/USB 65532) -> {}",
                                                rx.output_device_name
                                            )
                                        };
                                        self.phone_mic_receiver = Some(rx);
                                    }
                                    Err(e) => {
                                        self.phone_rx_mode = prev_mode;
                                        self.phone_rx_status = format!("Error: {}", e);
                                        self.show_toast(format!(
                                            "Phone receiver could not restart: {}",
                                            e
                                        ));
                                    }
                                }
                            }
                        }

                        ui.horizontal_wrapped(|ui| {
                            let mic_active = self.phone_mic_receiver.is_some();
                            let (btn_label, btn_col) = if mic_active {
                                (
                                    "■  Stop Phone Receiver",
                                    p.danger,
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
                                    self.measured_sonar_ms = None;
                                    self.measured_sonar_at = None;
                                    self.phone_rx_status = "Phone receiver stopped.".into();
                                } else {
                                    if self.pin_dirty {
                                        self.apply_pin_now();
                                    }
                                    let phone_pin = if self.enable_pin_encryption
                                        && !self.pairing_pin.trim().is_empty()
                                    {
                                        Some(self.pairing_pin.trim().to_string())
                                    } else {
                                        None
                                    };
                                    match PhoneMicReceiver::start_with_mode_and_pin(
                                        PHONE_MIC_PORT,
                                        self.phone_rx_mode,
                                        phone_pin.as_deref(),
                                    ) {
                                        Ok(rx) => {
                                            self.phone_rx_status = if self.phone_rx_mode
                                                == PhoneRxRoutingMode::VirtualMicForCalls
                                            {
                                                #[cfg(target_os = "windows")]
                                                {
                                                    format!(
                                                        "Listening (UDP/USB 65532) -> {} • In Zoom/Discord/Windows select 'CABLE Output (VB-Audio Virtual Cable)' as Microphone",
                                                        rx.output_device_name
                                                    )
                                                }
                                                #[cfg(not(target_os = "windows"))]
                                                {
                                                    format!(
                                                        "Listening (UDP/USB 65532) -> {}",
                                                        rx.output_device_name
                                                    )
                                                }
                                            } else {
                                                format!(
                                                    "Listening (UDP/USB 65532) -> {}",
                                                    rx.output_device_name
                                                )
                                            };
                                            self.phone_mic_receiver = Some(rx);
                                        }
                                        Err(e) => {
                                            self.phone_rx_status = format!("Error: {}", e);
                                            self.show_toast(format!(
                                                "Phone receiver could not start: {}",
                                                e
                                            ));
                                        }
                                    }
                                }
                            }

                            if let Some(ref mic) = self.phone_mic_receiver {
                                let pkts = mic.mic_packets_received.load(Ordering::Relaxed);
                                let peak = mic.mic_peak_level_pct.load(Ordering::Relaxed);
                                let auth_fails = mic.auth_failures.load(Ordering::Relaxed);
                                uw::status_pill(
                                    ui,
                                    &format!("RX: {} pkts • Peak: {}%", pkts, peak),
                                    p.ok_bg,
                                    p.ok_fg,
                                );
                                if auth_fails > 0 {
                                    uw::status_pill(
                                        ui,
                                        &format!("PIN Mismatch ({} dropped)", auth_fails),
                                        p.warn_bg,
                                        p.warn_fg,
                                    );
                                }

                                if let Some(ref host) = self.host_engine {
                                    if ui.button("Ping Acoustic Latency (Sonar)").clicked() {
                                        host.telemetry
                                            .trigger_sonar_ping
                                            .store(true, Ordering::Relaxed);
                                    }
                                }
                            }
                            if let Some(ms) = self.measured_sonar_ms {
                                let age_s = self
                                    .measured_sonar_at
                                    .map(|t| t.elapsed().as_secs())
                                    .unwrap_or(0);
                                uw::status_pill(
                                    ui,
                                    &format!("Measured One-Way: {:.1} ms ({}s ago)", ms, age_s),
                                    p.ok_bg,
                                    p.ok_fg,
                                );
                            }
                        });
                        ui.label(
                            egui::RichText::new(&self.phone_rx_status)
                                .size(11.0)
                                .color(if self.phone_rx_status.starts_with("Error:") {
                                    p.warn_fg
                                } else {
                                    p.muted_text
                                }),
                        );
                        #[cfg(target_os = "windows")]
                        if self.phone_mic_receiver.is_some() && !self.windows_firewall_ok {
                            ui.horizontal_wrapped(|ui| {
                                uw::status_pill(
                                    ui,
                                    "Firewall not open for Wi-Fi mic (UDP 65532) — allow firewall or use USB",
                                    p.warn_bg,
                                    p.warn_fg,
                                );
                                if ui
                                    .add_enabled(
                                        !self.firewall_task_in_flight,
                                        egui::Button::new("Allow Firewall (UAC)"),
                                    )
                                    .clicked()
                                {
                                    let ctx = ui.ctx().clone();
                                    self.dispatch_open_firewall(&ctx);
                                }
                            });
                        }
                    });
                });
            });
        }
    }

    fn render_client_tab(&mut self, ui: &mut egui::Ui) {
        let dark = self.dark_mode;
        let p = uw::Palette::for_mode(dark);

        // =========================================================================
        // 1. SIMPLE CONNECTION CONSOLE (Auto-Discovered Hosts + 1-Click Connect)
        // =========================================================================
        uw::studio_card(ui, dark, |ui| {
            let discovered = self
                .scanner
                .as_ref()
                .map(|s| s.discovered_hosts())
                .unwrap_or_default();
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
                        .fill(p.danger);
                        if ui.add(disc_btn).clicked() {
                            if let Some(mut c) = self.client_engine.take() {
                                c.stop();
                            }
                            self.drift_history = VecDeque::from(vec![0.0; 120]);
                            self.client_status_msg = "Disconnected.".into();
                        }
                    }
                    let manual_lbl = if self.client_manual_ip_open {
                        "Hide Manual IP"
                    } else {
                        "Manual IP / USB"
                    };
                    if uw::pill_choice(ui, dark, self.client_manual_ip_open, manual_lbl) {
                        self.client_manual_ip_open = !self.client_manual_ip_open;
                    }
                });
            });

            ui.label(
                egui::RichText::new(&self.client_status_msg)
                    .size(12.0)
                    .color(if self.client_engine.is_some() {
                        p.ok_fg
                    } else {
                        p.muted_text
                    }),
            );

            if let Some(ref client) = self.client_engine {
                let auth_err = client.telemetry.auth_failures.load(Ordering::Relaxed);
                let idle_secs = client
                    .telemetry
                    .seconds_since_last_packet
                    .load(Ordering::Relaxed);
                if auth_err > 0 {
                    ui.add_space(2.0);
                    uw::status_pill(
                        ui,
                        &format!(
                            "Authentication failed ({} packet(s)) — verify Pairing PIN in Settings",
                            auth_err
                        ),
                        p.warn_bg,
                        p.warn_fg,
                    );
                } else if idle_secs >= 3 {
                    ui.add_space(2.0);
                    uw::status_pill(
                        ui,
                        &format!(
                            "No audio packets from Host for {}s — waiting for stream or reconnect…",
                            idle_secs
                        ),
                        p.warn_bg,
                        p.warn_fg,
                    );
                }
            }

            if !discovered.is_empty() {
                ui.separator();
                for host in &discovered {
                    ui.horizontal(|ui| {
                        let badge = if host.is_usb { "USB" } else { "Wi-Fi" };
                        uw::status_pill(
                            ui,
                            badge,
                            egui::Color32::from_rgb(30, 58, 138),
                            egui::Color32::WHITE,
                        );
                        // Surface the beacon's `enc=1` bit so the operator knows
                        // this host needs a matching PIN before connecting.
                        if host.encrypted {
                            let (lock_rect, _) = ui.allocate_exact_size(
                                egui::vec2(10.0, 12.0),
                                egui::Sense::hover(),
                            );
                            uw::paint_lock_icon(ui.painter(), lock_rect.center(), p.ok_fg);
                            ui.label(
                                egui::RichText::new("PIN").size(10.5).strong().color(p.ok_fg),
                            )
                            .on_hover_text("Host requires a matching pairing PIN");
                        }
                        ui.monospace(format!("{} ({})", host.hostname, host.addr));
                        if ui.button("▶  Connect Now").clicked() {
                            if host.encrypted
                                && !(self.enable_pin_encryption
                                    && !self.pairing_pin.trim().is_empty())
                            {
                                self.show_toast(
                                    "Encrypted host — enable PIN encryption in Settings and enter the host's PIN",
                                );
                                self.client_status_msg =
                                    "PIN required: enable PIN encryption in Settings to connect to this host.".into();
                                self.settings_tab = SettingsTab::NetworkAndSecurity;
                                self.show_settings_modal = true;
                                return;
                            }
                            self.usb_buffer_mode = host.is_usb;
                            self.connect_client(host.addr, host.encrypted);
                        }
                    });
                }
            } else {
                ui.label(
                    egui::RichText::new(if self.client_manual_ip_open {
                        "Scanning LAN & USB for beacons... Or enter a Host IP:Port below."
                    } else {
                        "Scanning LAN & USB for beacons... Click 'Manual IP / USB' to enter an address directly."
                    })
                    .size(11.5)
                    .color(p.muted_text),
                );
            }

            if self.client_manual_ip_open {
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
                        let raw = self.manual_server_ip.trim();
                        // Full IP:PORT first, then a bare IP (including a bare
                        // IPv6 literal, which must NOT be split on ':') with
                        // the default stream port appended.
                        let parsed = raw.parse::<SocketAddr>().or_else(|_| {
                            raw.parse::<IpAddr>()
                                .map(|ip| SocketAddr::new(ip, DEFAULT_PORT))
                        });
                        match parsed {
                            Ok(addr) => {
                                self.usb_buffer_mode = is_usb_subnet_ip(addr.ip());
                                self.connect_client(addr, self.enable_pin_encryption);
                            }
                            Err(_) => {
                                self.client_status_msg = format!(
                                    "Invalid host address '{}' — use IP or IP:PORT (e.g. 192.168.1.5:{})",
                                    raw, DEFAULT_PORT
                                );
                            }
                        }
                    }
                    if ui.button("USB Localhost").clicked() {
                        self.manual_server_ip = format!("127.0.0.1:{}", DEFAULT_PORT);
                    }
                });
            }
        });

        // =========================================================================
        // 1b. REMOTE HOST CONTROL STRIP (feature parity with the Android Connect tab)
        // =========================================================================
        if let Some(ref client) = self.client_engine {
            ui.add_space(8.0);
            uw::studio_card(ui, dark, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        egui::RichText::new("Remote Host Control:")
                            .size(13.0)
                            .strong(),
                    );
                    if ui.button("Prev").clicked() {
                        client
                            .telemetry
                            .pending_media_command
                            .store(12, Ordering::Relaxed);
                    }
                    if ui.button("Play / Pause").clicked() {
                        client
                            .telemetry
                            .pending_media_command
                            .store(10, Ordering::Relaxed);
                    }
                    if ui.button("Next").clicked() {
                        client
                            .telemetry
                            .pending_media_command
                            .store(11, Ordering::Relaxed);
                    }
                    ui.separator();
                    for (cmd, lbl) in [
                        (1u8, "Auto"),
                        (2, "16-Bit"),
                        (5, "24-Bit"),
                        (3, "Smooth Wi-Fi"),
                        (4, "Max Range"),
                    ] {
                        if ui.small_button(lbl).clicked() {
                            client
                                .telemetry
                                .pending_media_command
                                .store(cmd, Ordering::Relaxed);
                            // Disjoint field write: `client` still borrows
                            // `self.client_engine`.
                            self.toast = Some((format!("Requested host mode: {lbl}"), Instant::now()));
                        }
                    }
                });
            });
        }

        ui.add_space(8.0);

        // =========================================================================
        // 2. SIMPLE SOUND PROFILE & 3 ENHANCEMENT CARDS (Always Visible)
        // =========================================================================
        let mut dsp_changed = false;
        uw::studio_card(ui, dark, |ui| {
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
                    if uw::pill_choice(ui, dark, is_preset, name) {
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
                dsp_changed |= uw::setting_switch_row(
                    &mut cols[0],
                    dark,
                    "3D Headphone Stage",
                    "Binaural studio monitor crossfeed",
                    &mut self.binaural_crossfeed,
                );
                dsp_changed |= uw::setting_switch_row(
                    &mut cols[1],
                    dark,
                    "Night Mode DRC",
                    "Boost whispers & tame explosions",
                    &mut self.night_mode_drc,
                );
                dsp_changed |= uw::setting_switch_row(
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
        uw::drawer_header(
            ui,
            dark,
            &mut self.client_eq_drawer_open,
            "Custom 3-Band Parametric EQ, Pre-Amp & Speaker Role",
            &[&preamp_pill, role_pill],
        );

        if self.client_eq_drawer_open {
            ui.add_space(4.0);
            uw::studio_card(ui, dark, |ui| {
                ui.columns(2, |cols| {
                    dsp_changed |= uw::studio_fader_row(
                        &mut cols[0],
                        dark,
                        "Pre-Amp Gain",
                        "True-Peak Limited",
                        &mut self.preamp_gain_db,
                        -12.0..=24.0,
                    );
                    dsp_changed |= uw::studio_fader_row(
                        &mut cols[0],
                        dark,
                        "Sub-Bass",
                        "90 Hz",
                        &mut self.eq_bass_db,
                        -12.0..=12.0,
                    );
                    dsp_changed |= uw::studio_fader_row(
                        &mut cols[1],
                        dark,
                        "Vocal Presence",
                        "2.5 kHz",
                        &mut self.eq_presence_db,
                        -12.0..=12.0,
                    );
                    dsp_changed |= uw::studio_fader_row(
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
                        if uw::pill_choice(ui, dark, self.channel_role == role, label) {
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
                // Scope history is sampled on a fixed timer in logic(); here we
                // only read the current value for the numeric tile.
                let drift_now = client.telemetry.clock_drift_ppm.load(Ordering::Relaxed);
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
        uw::drawer_header(
            ui,
            dark,
            &mut self.client_telemetry_drawer_open,
            "Real-Time WSOLA Sequencer & Clock-Drift Oscilloscope",
            &[&fec_pill, &drift_pill],
        );

        if self.client_telemetry_drawer_open {
            ui.add_space(4.0);
            uw::studio_card(ui, dark, |ui| {
                if self.client_engine.is_some() && idle_secs >= 3 {
                    uw::status_pill(
                        ui,
                        &format!(
                            "No packets from Host for {}s — Waiting for automatic reconnect...",
                            idle_secs
                        ),
                        p.warn_bg,
                        p.warn_fg,
                    );
                }

                ui.columns(4, |cols| {
                    uw::metric_tile(
                        &mut cols[0],
                        dark,
                        "RX Packets / Lossless",
                        &format!("{} / {}", pkts, prim),
                    );
                    uw::metric_tile(
                        &mut cols[1],
                        dark,
                        "FEC Healed / WSOLA PLC",
                        &format!("{} / {}", fec, plc),
                    );
                    uw::metric_tile(
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
                    uw::metric_tile(
                        &mut cols[3],
                        dark,
                        "Host Vol / Group Sync",
                        &format!("{}% / +{} ms ({})", host_vol, group_ms, state_str),
                    );
                });

                ui.add_space(4.0);
                uw::draw_drift_oscilloscope(ui, &self.drift_history);
            });
        }
    }

    fn render_latency_debug(&mut self, ui: &mut egui::Ui) {
        let dark = self.dark_mode;
        let p = uw::Palette::for_mode(dark);
        ui.label(
            egui::RichText::new(
                "Live values are published by the audio threads. The estimate is not an acoustic measurement.",
            )
            .size(11.5)
            .color(p.muted_text),
        );
        ui.add_space(4.0);

        let sonar_ms = self.measured_sonar_ms;
        let sonar_age_s = self.measured_sonar_at.map(|t| t.elapsed().as_secs_f32());
        let mut any_engine = false;

        if let Some(ref host) = self.host_engine {
            any_engine = true;
            uw::studio_card(ui, dark, |ui| {
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
                    uw::metric_tile(
                        &mut cols[0],
                        dark,
                        "Packetization",
                        &format!(
                            "{:.1} ms ({} x {:.1} ms)",
                            pacing_ms, subframes, SUBFRAME_MS
                        ),
                    );
                    uw::metric_tile(
                        &mut cols[1],
                        dark,
                        "Capture Backlog",
                        &format!("{:.1} ms", capture_ms),
                    );
                    uw::metric_tile(
                        &mut cols[2],
                        dark,
                        "Host Pipeline Total",
                        &format!("{:.1} ms ({})", capture_ms + pacing_ms, codec),
                    );
                });

                ui.columns(4, |cols| {
                    uw::metric_tile(
                        &mut cols[0],
                        dark,
                        "Downstream Max",
                        &format!("{} ms (BT max {} ms)", downstream_max, bt_max),
                    );
                    uw::metric_tile(
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
                    uw::metric_tile(&mut cols[2], dark, "Acoustic Sonar", &sonar_label);
                    uw::metric_tile(
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

            uw::studio_card(ui, dark, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("Client Pipeline: Ingress -> Speaker")
                            .size(14.5)
                            .strong(),
                    );
                    uw::status_pill(
                        ui,
                        &format!("Estimated One-Way: {:.1} ms", snap.estimated_one_way_ms()),
                        p.ok_bg,
                        p.ok_fg,
                    );
                    uw::status_pill(
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
                    uw::metric_tile(
                        &mut cols[0],
                        dark,
                        "Sender Packetization",
                        &format!("{:.1} ms ({})", snap.packet_interval_ms, snap.codec_name()),
                    );
                    uw::metric_tile(
                        &mut cols[1],
                        dark,
                        "Network Jitter",
                        &format!("{:.1} ms", snap.network_jitter_ms),
                    );
                    uw::metric_tile(
                        &mut cols[2],
                        dark,
                        "Adaptive Cushion",
                        &format!("{:.1} ms", snap.adaptive_buffer_ms),
                    );
                });

                ui.columns(3, |cols| {
                    uw::metric_tile(
                        &mut cols[0],
                        dark,
                        "Target FIFO",
                        &format!(
                            "{:.1} ms (Group +{:.1} ms)",
                            snap.target_buffer_ms, snap.group_sync_extra_ms
                        ),
                    );
                    uw::metric_tile(
                        &mut cols[1],
                        dark,
                        "Buffered Now",
                        &format!("{:.1} ms", snap.buffered_audio_ms),
                    );
                    uw::metric_tile(
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
                    uw::metric_tile(
                        &mut cols[0],
                        dark,
                        "DAC Queue",
                        &if snap.hardware_delay_ms > 0 {
                            format!("{} ms (measured)", snap.hardware_delay_ms)
                        } else {
                            "not measured".to_string()
                        },
                    );
                    uw::metric_tile(
                        &mut cols[1],
                        dark,
                        "Bluetooth Estimate",
                        &if snap.bluetooth_delay_ms > 0 {
                            format!("{} ms (A2DP)", snap.bluetooth_delay_ms)
                        } else {
                            "not routed".to_string()
                        },
                    );
                    uw::metric_tile(
                        &mut cols[2],
                        dark,
                        "Desktop Output Buffer",
                        &format!("{:.1} ms", snap.output_buffer_ms),
                    );
                    uw::metric_tile(
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
                    .color(p.muted_text),
                );
            });
        }

        if !any_engine {
            uw::studio_card(ui, dark, |ui| {
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
    fn test_lipsync_toggle_off_clears_applied_offset() {
        // Enabled: feed the first BT report through, then apply only changes
        // that pass the hysteresis filter.
        assert_eq!(lipsync_offset_to_apply(true, 145, 0), Some(145));
        assert_eq!(lipsync_offset_to_apply(true, 150, 145), None);
        assert_eq!(lipsync_offset_to_apply(true, 160, 145), Some(160));

        // Disabled after a 160 ms offset was applied: the sink must be reset
        // instead of keeping audio 160 ms behind the video forever.
        assert_eq!(lipsync_offset_to_apply(false, 160, 160), Some(0));
        // ... and the reset happens exactly once.
        assert_eq!(lipsync_offset_to_apply(false, 160, 0), None);
        // Disabled before anything was applied: nothing to do.
        assert_eq!(lipsync_offset_to_apply(false, 145, 0), None);
    }

    #[test]
    fn test_format_windows_bt_lipsync_hint() {
        assert_eq!(format_windows_bt_lipsync_hint(0), None);
        let hint = format_windows_bt_lipsync_hint(245).expect("non-zero delay produces hint");
        assert!(hint.contains("-245 ms"), "unexpected hint: {hint}");
        assert!(
            hint.contains(
                "Enable Earbud Gaming / Low-Latency Mode or LE Audio (LC3) to reduce browser lag to ~40-60 ms."
            ),
            "missing low-latency/LC3 tip: {hint}"
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
        let mut app = AudioShareApp::test_instance();
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

    #[test]
    fn test_prefer_opus_config_always_migrates_to_true() {
        let mut app = AudioShareApp::test_instance();
        // Legacy VBR opt-outs and arbitrary values must all migrate to Opus:
        // Pure-Rust VBR is no longer a user-selectable codec backend.
        app.parse_config_string("prefer_opus=false\n");
        assert!(app.prefer_opus);
        app.parse_config_string("prefer_opus=1\n");
        assert!(app.prefer_opus);
        app.parse_config_string("prefer_opus=anything\n");
        assert!(app.prefer_opus);
        app.parse_config_string("prefer_opus=\n");
        assert!(app.prefer_opus);
    }

    #[test]
    fn test_desktop_config_string_parser_bitrate_clamping() {
        let mut app = AudioShareApp::test_instance();
        // Lower bound clamp (min 96)
        app.parse_config_string("target_bitrate_kbps=32\n");
        assert_eq!(app.target_bitrate_kbps, 96);

        // Upper bound clamp (max 320)
        app.parse_config_string("target_bitrate_kbps=1000\n");
        assert_eq!(app.target_bitrate_kbps, 510);

        // Within range
        app.parse_config_string("target_bitrate_kbps=160\n");
        assert_eq!(app.target_bitrate_kbps, 160);
    }

    #[test]
    fn test_app_selection_pid_retention_across_refresh() {
        let app1 = AudioAppEntry {
            id: 1,
            pid: 1001,
            name: "Browser".into(),
        };
        let app2 = AudioAppEntry {
            id: 2,
            pid: 2002,
            name: "Music Player".into(),
        };
        let app3 = AudioAppEntry {
            id: 3,
            pid: 3003,
            name: "Game".into(),
        };

        let audio_apps_initial = [app1.clone(), app2.clone(), app3.clone()];
        // Select app2 (index 1, PID 2002)
        let selected_app_idx = Some(1);
        let prev_pid = selected_app_idx.and_then(|idx| audio_apps_initial.get(idx).map(|a| a.pid));
        assert_eq!(prev_pid, Some(2002));

        // After refresh, Browser (app1) closed, and a new app (Chat) opened before Music Player
        let app_chat = AudioAppEntry {
            id: 4,
            pid: 4004,
            name: "Chat".into(),
        };
        let audio_apps_refreshed = [app_chat, app2.clone(), app3.clone()];

        // With PID-based preservation:
        let new_selected_idx = prev_pid.and_then(|pid| {
            audio_apps_refreshed.iter().position(|a| a.pid == pid)
        });
        assert_eq!(new_selected_idx, Some(1)); // Still points to Music Player (PID 2002)

        // Now test when Music Player (PID 2002) exits entirely
        let audio_apps_closed = [app3.clone()];
        let closed_selected_idx = prev_pid.and_then(|pid| {
            audio_apps_closed.iter().position(|a| a.pid == pid)
        });
        assert_eq!(closed_selected_idx, None);
    }

    #[test]
    fn test_pairing_pin_effective_resolution() {
        fn effective_pin(enabled: bool, pin: &str) -> Option<&str> {
            if enabled && !pin.trim().is_empty() {
                Some(pin)
            } else {
                None
            }
        }

        assert_eq!(effective_pin(true, "1234"), Some("1234"));
        assert_eq!(effective_pin(true, "  "), None);
        assert_eq!(effective_pin(true, ""), None);
        assert_eq!(effective_pin(false, "1234"), None);
        assert_eq!(effective_pin(false, ""), None);
    }

    #[test]
    fn test_effective_client_pin_respects_open_host_flag() {
        // Discovered open host (use_pin = false) ignores local host PIN encryption.
        assert_eq!(effective_client_pin(false, true, "482910"), None);
        // Encrypted host or manual IP with encryption ON uses trimmed PIN.
        assert_eq!(effective_client_pin(true, true, " 482910 "), Some("482910"));
        assert_eq!(effective_client_pin(true, false, "482910"), None);
        assert_eq!(effective_client_pin(true, true, "   "), None);
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
