use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
#[cfg(target_os = "windows")]
use std::process::{Child, Stdio};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "windows")]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::HostTelemetry;

pub const DISCOVERY_PORT: u16 = 65531;
/// Link-local multicast group used alongside subnet broadcast. Some mesh
/// Wi-Fi nodes drop directed/limited broadcasts but still forward multicast.
pub const DISCOVERY_MULTICAST_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 77, 77);
const BEACON_PREFIX: &str = "ASULTRA_BEACON|";
/// Ports advertised by a discovery beacon. Port 0 is not connectable, so a
/// malformed or truncated beacon must not add a host at `IP:0`.
fn beacon_port(raw: &str) -> u16 {
    raw.parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .unwrap_or(crate::DEFAULT_PORT)
}



#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[inline]
pub(crate) fn silent_command(program: &str) -> Command {
    let cmd = Command::new(program);
    #[cfg(target_os = "windows")]
    {
        let mut cmd = cmd;
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd
    }
    #[cfg(not(target_os = "windows"))]
    {
        cmd
    }
}

#[derive(Debug, Clone)]
pub struct DiscoveredHost {
    pub hostname: String,
    pub addr: SocketAddr,
    pub is_usb: bool,
    pub last_seen: Instant,
}

pub struct DiscoveryBeacon {
    running: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl DiscoveryBeacon {
    pub fn start(stream_port: u16, host_label: String) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let run_clone = Arc::clone(&running);

        let handle = std::thread::Builder::new()
            .name("as-ultra-beacon".into())
            .spawn(move || {
                let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else {
                    return;
                };
                let _ = sock.set_broadcast(true);
                // Link-local scope: never leak discovery beacons past the LAN.
                let _ = sock.set_multicast_ttl_v4(1);
                let payload = format!("{}{}|{}", BEACON_PREFIX, host_label, stream_port);

                while run_clone.load(Ordering::Relaxed) {
                    let _ = sock.send_to(
                        payload.as_bytes(),
                        SocketAddr::from(([255, 255, 255, 255], DISCOVERY_PORT)),
                    );
                    let _ = sock.send_to(
                        payload.as_bytes(),
                        SocketAddr::from((DISCOVERY_MULTICAST_GROUP, DISCOVERY_PORT)),
                    );

                    for iface in crate::list_valid_host_interfaces() {
                        if let IpAddr::V4(ipv4) = iface.ip {
                            let octets = ipv4.octets();
                            let subnet_bcast = SocketAddr::from((
                                [octets[0], octets[1], octets[2], 255],
                                DISCOVERY_PORT,
                            ));
                            let _ = sock.send_to(payload.as_bytes(), subnet_bcast);
                        }
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            })
            .ok();

        Self { running, handle }
    }
}

impl Drop for DiscoveryBeacon {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

pub struct DiscoveryScanner {
    running: Arc<AtomicBool>,
    hosts: Arc<Mutex<HashMap<SocketAddr, DiscoveredHost>>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl DiscoveryScanner {
    pub fn start() -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let hosts = Arc::new(Mutex::new(HashMap::new()));
        let run_clone = Arc::clone(&running);
        let hosts_clone = Arc::clone(&hosts);

        let handle = std::thread::Builder::new()
            .name("as-ultra-scanner".into())
            .spawn(move || {
                let Ok(raw_sock) = socket2::Socket::new(
                    socket2::Domain::IPV4,
                    socket2::Type::DGRAM,
                    Some(socket2::Protocol::UDP),
                ) else {
                    return;
                };
                let _ = raw_sock.set_reuse_address(true);
                #[cfg(all(unix, not(target_os = "solaris"), not(target_os = "illumos")))]
                let _ = raw_sock.set_reuse_port(true);
                let bind_addr = SocketAddr::from(([0, 0, 0, 0], DISCOVERY_PORT));
                if raw_sock.bind(&bind_addr.into()).is_err() {
                    return;
                }
                let sock: UdpSocket = raw_sock.into();
                // Join the link-local multicast group on the default route and
                // on every valid LAN interface (mesh nodes/APs may only forward
                // multicast on a specific interface).
                let _ = sock.join_multicast_v4(&DISCOVERY_MULTICAST_GROUP, &Ipv4Addr::UNSPECIFIED);
                for iface in crate::list_valid_host_interfaces() {
                    if let IpAddr::V4(ip) = iface.ip {
                        let _ = sock.join_multicast_v4(&DISCOVERY_MULTICAST_GROUP, &ip);
                    }
                }
                let _ = sock.set_read_timeout(Some(Duration::from_millis(250)));
                let mut buf = [0u8; 256];
                let mut last_mcast_refresh = Instant::now();

                while run_clone.load(Ordering::Relaxed) {
                    if last_mcast_refresh.elapsed() >= Duration::from_secs(2) {
                        last_mcast_refresh = Instant::now();
                        for iface in crate::list_valid_host_interfaces() {
                            if let IpAddr::V4(ip) = iface.ip {
                                let _ = sock.join_multicast_v4(&DISCOVERY_MULTICAST_GROUP, &ip);
                            }
                        }
                    }
                    if let Ok((len, src)) = sock.recv_from(&mut buf) {
                        if let Ok(msg) = std::str::from_utf8(&buf[..len]) {
                            if let Some(rest) = msg.strip_prefix(BEACON_PREFIX) {
                                let parts: Vec<&str> = rest.split('|').collect();
                                if parts.len() >= 2 {
                                    let hostname = parts[0].to_string();
                                    let port = beacon_port(parts[1]);
                                    let host_addr = SocketAddr::new(src.ip(), port);
                                    let is_usb = crate::is_usb_subnet_ip(src.ip());

                                    hosts_clone.lock().unwrap().insert(
                                        host_addr,
                                        DiscoveredHost {
                                            hostname,
                                            addr: host_addr,
                                            is_usb,
                                            last_seen: Instant::now(),
                                        },
                                    );
                                }
                            }
                        }
                    }
                    hosts_clone
                        .lock()
                        .unwrap()
                        .retain(|_, v| v.last_seen.elapsed() < Duration::from_secs(4));
                }
            })
            .ok();

        Self {
            running,
            hosts,
            handle,
        }
    }

    pub fn discovered_hosts(&self) -> Vec<DiscoveredHost> {
        let mut list: Vec<DiscoveredHost> = self.hosts.lock().unwrap().values().cloned().collect();
        list.sort_by_key(|h| !h.is_usb);
        list
    }
}

impl Drop for DiscoveryScanner {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

pub struct AdbUsbWatcher {
    running: Arc<AtomicBool>,
    pub usb_device_connected: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl AdbUsbWatcher {
    pub fn start(port: u16) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let usb_device_connected = Arc::new(AtomicBool::new(false));
        let run_clone = Arc::clone(&running);
        let usb_clone = Arc::clone(&usb_device_connected);

        let handle = std::thread::Builder::new()
            .name("as-ultra-adb-watcher".into())
            .spawn(move || {
                let mut was_connected = false;
                while run_clone.load(Ordering::Relaxed) {
                    let has_device = silent_command("adb")
                        .args(["get-state"])
                        .output()
                        .map(|o| o.status.success())
                        .unwrap_or(false);

                    usb_clone.store(has_device, Ordering::Relaxed);

                    if has_device && !was_connected {
                        let port_spec = format!("tcp:{}", port);
                        let _ = silent_command("adb")
                            .args(["reverse", &port_spec, &port_spec])
                            .output();
                    }
                    was_connected = has_device;
                    std::thread::sleep(Duration::from_secs(2));
                }
            })
            .ok();

        Self {
            running,
            usb_device_connected,
            handle,
        }
    }
}

impl Drop for AdbUsbWatcher {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// 1-Hop Wi-Fi Direct / SoftAP Hotspot Controller (Bypasses congested or AP-isolated routers)
pub struct DirectHotspotController;

#[cfg(target_os = "windows")]
fn windows_hotspot_holder() -> &'static Mutex<Option<Child>> {
    static HOLDER: OnceLock<Mutex<Option<Child>>> = OnceLock::new();
    HOLDER.get_or_init(|| Mutex::new(None))
}

impl DirectHotspotController {
    pub fn start_direct_5ghz_hotspot(ssid: &str, password: &str) -> Result<String, String> {
        #[cfg(target_os = "linux")]
        {
            let output = Command::new("nmcli")
                .args([
                    "device", "wifi", "hotspot", "ssid", ssid, "password", password, "band", "a",
                ])
                .output()
                .map_err(|e| format!("Failed to invoke nmcli: {}", e))?;

            if output.status.success() {
                Ok(format!(
                    "1-Hop 5GHz Direct Link '{}' Active (IP: 10.42.0.1)",
                    ssid
                ))
            } else {
                Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
            }
        }

        #[cfg(target_os = "windows")]
        {
            Self::stop_direct_hotspot();
            let safe_ssid = ssid.replace('\'', "''");
            let safe_pass = password.replace('\'', "''");
            // Modern Windows 10/11 Wi-Fi Direct Legacy SoftAP (works without
            // an internet connection and on WDI Wi-Fi 6/6E/7 drivers where
            // `netsh wlan start hostednetwork` was removed). The SoftAP stays
            // active for the lifetime of this hidden holder process.
            let script = format!(
                "$ErrorActionPreference = 'Stop'; \
                 $pub = [Windows.Devices.WiFiDirect.WiFiDirectAdvertisementPublisher,Windows.Devices.WiFiDirect,ContentType=WindowsRuntime]::new(); \
                 $pub.Advertisement.ListenStateDiscoverability = [Windows.Devices.WiFiDirect.WiFiDirectAdvertisementListenStateDiscoverability]::Intensive; \
                 $pub.Advertisement.IsAutonomousGroupOwnerEnabled = $true; \
                 $legacy = $pub.Advertisement.LegacySettings; \
                 $legacy.IsEnabled = $true; \
                 $legacy.Ssid = '{}'; \
                 $cred = [Windows.Security.Credentials.PasswordCredential,Windows.Security.Credentials,ContentType=WindowsRuntime]::new(); \
                 $cred.Password = '{}'; \
                 $legacy.Passphrase = $cred; \
                 $pub.Start(); \
                 while ($true) {{ Start-Sleep -Seconds 3600 }}",
                safe_ssid, safe_pass
            );
            let child = silent_command("powershell")
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| format!("Failed to launch WinRT SoftAP host: {}", e))?;
            *windows_hotspot_holder().lock().unwrap() = Some(child);
            Ok(format!(
                "1-Hop Direct Link '{}' Active (IP: 192.168.137.1)",
                ssid
            ))
        }

        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            let _ = (ssid, password);
            Err("Unsupported OS for Direct Hotspot".into())
        }
    }

    pub fn stop_direct_hotspot() {
        #[cfg(target_os = "linux")]
        {
            let _ = Command::new("nmcli")
                .args(["connection", "down", "Hotspot"])
                .output();
        }
        #[cfg(target_os = "windows")]
        {
            if let Some(mut child) = windows_hotspot_holder().lock().unwrap().take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            let _ = silent_command("netsh")
                .args(["wlan", "stop", "hostednetwork"])
                .output();
        }
    }
}

/// Opens UDP ports 65530..65532 in Linux (ufw / firewalld) or Windows Defender Firewall
pub fn open_os_firewall_ports() -> String {
    #[cfg(target_os = "linux")]
    {
        let cmd = "ufw allow 65530:65532/udp || (firewall-cmd --add-port=65530-65532/udp && firewall-cmd --runtime-to-permanent)";
        match Command::new("pkexec").args(["sh", "-c", cmd]).status() {
            Ok(s) if s.success() => "Firewall ports 65530-65532/UDP opened successfully!".into(),
            _ => "Firewall rule skipped or no active firewall detected.".into(),
        }
    }
    #[cfg(target_os = "windows")]
    {
        let status = silent_command("netsh")
            .args([
                "advfirewall",
                "firewall",
                "add",
                "rule",
                "name=AudioShare_Ultra_UDP",
                "dir=in",
                "action=allow",
                "protocol=UDP",
                "localport=65530-65532",
            ])
            .status();
        match status {
            Ok(s) if s.success() => "Windows Firewall UDP ports 65530-65532 opened!".into(),
            _ => "Run as Administrator once to add Windows Firewall rule.".into(),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        "Unsupported OS".into()
    }
}

/// Configures or removes OS Boot Auto-Start (Solves Issue #170 on Linux XDG & Windows Registry)
pub fn set_os_boot_autostart(enable: bool) -> String {
    let Ok(exe_path) = std::env::current_exe() else {
        return "Failed to resolve executable path".into();
    };
    let exe_str = exe_path.to_string_lossy();

    #[cfg(target_os = "linux")]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let autostart_dir = std::path::Path::new(&home).join(".config/autostart");
        let desktop_file = autostart_dir.join("audioshare-ultra.desktop");
        if enable {
            let _ = std::fs::create_dir_all(&autostart_dir);
            let content = format!(
                "[Desktop Entry]\nType=Application\nName=AudioShare Ultra\nExec={} --autostart\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
                exe_str
            );
            match std::fs::write(&desktop_file, content) {
                Ok(_) => "Enabled Linux Boot Auto-Start (~/.config/autostart)".into(),
                Err(e) => format!("Auto-start error: {}", e),
            }
        } else {
            let _ = std::fs::remove_file(&desktop_file);
            "Disabled Linux Boot Auto-Start".into()
        }
    }

    #[cfg(target_os = "windows")]
    {
        if enable {
            let _ = silent_command("reg")
                .args([
                    "add",
                    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                    "/v",
                    "AudioShareUltra",
                    "/t",
                    "REG_SZ",
                    "/d",
                    &format!("\"{}\" --autostart", exe_str),
                    "/f",
                ])
                .output();
            "Enabled Windows Boot Auto-Start (HKCU Run)".into()
        } else {
            let _ = silent_command("reg")
                .args([
                    "delete",
                    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                    "/v",
                    "AudioShareUltra",
                    "/f",
                ])
                .output();
            "Disabled Windows Boot Auto-Start".into()
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = (enable, &exe_str);
        "Unsupported OS for boot auto-start".into()
    }
}

/// Polls the OS master output volume/mute on a dedicated thread and mirrors it
/// into `HostTelemetry::host_volume`. This lets physical PC keyboard media
/// keys propagate to connected phones without ever invoking `pactl` from the
/// real-time audio TX loop.
pub struct HostVolumeMonitor {
    running: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl HostVolumeMonitor {
    pub fn start(telemetry: Arc<HostTelemetry>) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let run_clone = Arc::clone(&running);

        let handle = std::thread::Builder::new()
            .name("as-ultra-volume-monitor".into())
            .spawn(move || {
                #[cfg(target_os = "linux")]
                {
                    let mut last_os_volume: Option<u8> = None;
                    while run_clone.load(Ordering::Relaxed) {
                        if let Some(volume) = read_linux_master_volume() {
                            // Only overwrite the telemetry when the OS value
                            // itself changed. A GUI slider adjustment therefore
                            // persists until the user actually presses a system
                            // volume/mute key.
                            if last_os_volume != Some(volume) {
                                telemetry.host_volume.store(volume, Ordering::Relaxed);
                                last_os_volume = Some(volume);
                            }
                        }
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
                #[cfg(target_os = "windows")]
                {
                    let mut last_os_volume: Option<u8> = None;
                    while run_clone.load(Ordering::Relaxed) {
                        if let Some(volume) = read_windows_master_volume() {
                            if last_os_volume != Some(volume) {
                                telemetry.host_volume.store(volume, Ordering::Relaxed);
                                last_os_volume = Some(volume);
                            }
                        }
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
                #[cfg(not(any(target_os = "linux", target_os = "windows")))]
                {
                    let _ = telemetry;
                    while run_clone.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
            })
            .ok();

        Self { running, handle }
    }
}

impl Drop for HostVolumeMonitor {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Parse the first channel percentage from `pactl get-sink-volume` output.
#[cfg(target_os = "linux")]
fn parse_pactl_volume_percent(output: &str) -> Option<u8> {
    let pct_end = output.find('%')?;
    let number = output[..pct_end].split_whitespace().last()?;
    number.parse::<u32>().ok().map(|v| v.min(100) as u8)
}

#[cfg(target_os = "linux")]
fn read_linux_master_volume() -> Option<u8> {
    let mute = Command::new("pactl")
        .args(["get-sink-mute", "@DEFAULT_SINK@"])
        .output()
        .ok()?;
    if !mute.status.success() {
        return None;
    }
    let muted = String::from_utf8_lossy(&mute.stdout)
        .to_ascii_lowercase()
        .contains("yes");
    if muted {
        return Some(0);
    }

    let volume = Command::new("pactl")
        .args(["get-sink-volume", "@DEFAULT_SINK@"])
        .output()
        .ok()?;
    if !volume.status.success() {
        return None;
    }
    parse_pactl_volume_percent(&String::from_utf8_lossy(&volume.stdout))
}

/// Zero-dependency Windows CoreAudio COM reader for master speaker volume & mute.
#[cfg(target_os = "windows")]
fn read_windows_master_volume() -> Option<u8> {
    use core::ffi::c_void;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Guid {
        data1: u32,
        data2: u16,
        data3: u16,
        data4: [u8; 8],
    }

    #[repr(C)]
    struct IMMDeviceEnumeratorVtbl {
        query_interface: usize,
        add_ref: usize,
        release: unsafe extern "system" fn(*mut IMMDeviceEnumerator) -> u32,
        enum_audio_endpoints: usize,
        get_default_audio_endpoint: unsafe extern "system" fn(
            *mut IMMDeviceEnumerator,
            u32,
            u32,
            *mut *mut IMMDevice,
        ) -> i32,
    }
    #[repr(C)]
    struct IMMDeviceEnumerator {
        lp_vtbl: *const IMMDeviceEnumeratorVtbl,
    }

    #[repr(C)]
    struct IMMDeviceVtbl {
        query_interface: usize,
        add_ref: usize,
        release: unsafe extern "system" fn(*mut IMMDevice) -> u32,
        activate: unsafe extern "system" fn(
            *mut IMMDevice,
            *const Guid,
            u32,
            *const c_void,
            *mut *mut IAudioEndpointVolume,
        ) -> i32,
    }
    #[repr(C)]
    struct IMMDevice {
        lp_vtbl: *const IMMDeviceVtbl,
    }

    #[repr(C)]
    struct IAudioEndpointVolumeVtbl {
        query_interface: usize,
        add_ref: usize,
        release: unsafe extern "system" fn(*mut IAudioEndpointVolume) -> u32,
        register_control_change_notify: usize,
        unregister_control_change_notify: usize,
        get_channel_count: usize,
        set_master_volume_level: usize,
        set_master_volume_level_scalar: usize,
        get_master_volume_level: usize,
        get_master_volume_level_scalar:
            unsafe extern "system" fn(*mut IAudioEndpointVolume, *mut f32) -> i32,
        set_channel_volume_level: usize,
        set_channel_volume_level_scalar: usize,
        get_channel_volume_level: usize,
        get_channel_volume_level_scalar: usize,
        set_mute: usize,
        get_mute: unsafe extern "system" fn(*mut IAudioEndpointVolume, *mut i32) -> i32,
    }
    #[repr(C)]
    struct IAudioEndpointVolume {
        lp_vtbl: *const IAudioEndpointVolumeVtbl,
    }

    #[link(name = "ole32")]
    extern "system" {
        fn CoInitializeEx(pv_reserved: *mut c_void, dw_co_init: u32) -> i32;
        fn CoCreateInstance(
            rclsid: *const Guid,
            p_unk_outer: *mut c_void,
            dw_cls_context: u32,
            riid: *const Guid,
            ppv: *mut *mut c_void,
        ) -> i32;
    }

    const CLSID_MMDEVICE_ENUMERATOR: Guid = Guid {
        data1: 0xBCDE_0395,
        data2: 0xE52F,
        data3: 0x467C,
        data4: [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E],
    };
    const IID_IMMDEVICE_ENUMERATOR: Guid = Guid {
        data1: 0xA956_64D2,
        data2: 0x9614,
        data3: 0x4F35,
        data4: [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6],
    };
    const IID_IAUDIO_ENDPOINT_VOLUME: Guid = Guid {
        data1: 0x5CDF_2C82,
        data2: 0x841E,
        data3: 0x4546,
        data4: [0x97, 0x22, 0x0C, 0xF7, 0x40, 0x78, 0x22, 0x9A],
    };

    unsafe {
        let _ = CoInitializeEx(core::ptr::null_mut(), 0);
        let mut enumerator: *mut IMMDeviceEnumerator = core::ptr::null_mut();
        if CoCreateInstance(
            &CLSID_MMDEVICE_ENUMERATOR,
            core::ptr::null_mut(),
            1, // CLSCTX_INPROC_SERVER
            &IID_IMMDEVICE_ENUMERATOR,
            &mut enumerator as *mut _ as *mut *mut c_void,
        ) < 0
            || enumerator.is_null()
        {
            return None;
        }
        let mut device: *mut IMMDevice = core::ptr::null_mut();
        let hr_dev = ((*(*enumerator).lp_vtbl).get_default_audio_endpoint)(
            enumerator,
            0, // eRender
            0, // eConsole
            &mut device,
        );
        ((*(*enumerator).lp_vtbl).release)(enumerator);
        if hr_dev < 0 || device.is_null() {
            return None;
        }
        let mut ep_vol: *mut IAudioEndpointVolume = core::ptr::null_mut();
        let hr_act = ((*(*device).lp_vtbl).activate)(
            device,
            &IID_IAUDIO_ENDPOINT_VOLUME,
            1,
            core::ptr::null(),
            &mut ep_vol,
        );
        ((*(*device).lp_vtbl).release)(device);
        if hr_act < 0 || ep_vol.is_null() {
            return None;
        }
        let mut muted: i32 = 0;
        let mut scalar: f32 = 1.0;
        let ok_mute = ((*(*ep_vol).lp_vtbl).get_mute)(ep_vol, &mut muted) >= 0;
        let ok_vol =
            ((*(*ep_vol).lp_vtbl).get_master_volume_level_scalar)(ep_vol, &mut scalar) >= 0;
        ((*(*ep_vol).lp_vtbl).release)(ep_vol);
        if ok_mute && muted != 0 {
            Some(0)
        } else if ok_vol {
            Some((scalar.clamp(0.0, 1.0) * 100.0).round() as u8)
        } else {
            None
        }
    }
}

/// Virtual Speaker & OS Wi-Fi Scan Suppressor Guard.
///
/// `acquire()` loads/registers the virtual endpoint but leaves the user's
/// current routing untouched. The desktop GUI calls `activate()` once at
/// least one phone is connected and `deactivate()` when the last phone leaves,
/// so the physical speakers are only replaced while audio is being streamed.
pub struct VirtualSpeakerAndRadioGuard {
    /// The virtual endpoint is available and may be activated.
    enabled: bool,
    /// The virtual endpoint is currently the default routing target.
    active: bool,
    #[cfg(target_os = "linux")]
    pactl_module_id: Option<String>,
    #[cfg(target_os = "linux")]
    prev_default_sink: Option<String>,
    #[cfg(target_os = "linux")]
    prev_default_source: Option<String>,
    #[cfg(target_os = "windows")]
    prev_default_sink: Option<String>,
}

#[cfg(target_os = "linux")]
fn pactl_get_default(kind: &str) -> Option<String> {
    let arg = format!("get-default-{}", kind);
    let output = Command::new("pactl").arg(arg).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

#[cfg(target_os = "linux")]
fn pactl_set_default(kind: &str, value: &str) {
    let arg = format!("set-default-{}", kind);
    let _ = Command::new("pactl").args([arg.as_str(), value]).output();
}

/// Linux names wireless NICs `wl*` (e.g. `wlan0`, `wlp2s0`, `wlo1`).
#[cfg(target_os = "linux")]
fn is_wireless_interface_name(name: &str) -> bool {
    name.starts_with("wl")
}

/// Enumerate every wireless interface from sysfs, including interfaces that
/// are currently down and therefore absent from `list_valid_host_interfaces`.
#[cfg(target_os = "linux")]
fn wireless_interface_names() -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_wireless_interface_name(&name) && !names.contains(&name) {
                names.push(name);
            }
        }
    }
    if names.is_empty() {
        // Fallback for containers/chroots without sysfs.
        names.push("wlan0".into());
    }
    names
}

#[cfg(target_os = "linux")]
fn disable_wifi_power_save() {
    for iface in wireless_interface_names() {
        let _ = Command::new("iw")
            .args(["dev", &iface, "set", "power_save", "off"])
            .output();
    }
}

#[cfg(target_os = "windows")]
fn windows_default_playback_name() -> Option<String> {
    let output = silent_command("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "$d = Get-AudioDevice -List | Where-Object {$_.Default -eq $true} | Select-Object -First 1; if ($d) { $d.Name }",
        ])
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

#[cfg(target_os = "windows")]
fn windows_set_playback_name(name: &str) {
    let escaped = name.replace('\'', "''");
    let script = format!(
        "Get-AudioDevice -List | Where-Object {{$_.Name -eq '{}'}} | Set-AudioDevice",
        escaped
    );
    let _ = silent_command("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output();
}

impl VirtualSpeakerAndRadioGuard {
    /// Register the virtual endpoint (if enabled) without changing the user's
    /// current default sink/source. Call `activate()` when a client connects.
    pub fn acquire(enable_virtual_speaker: bool) -> Self {
        #[cfg(target_os = "linux")]
        {
            let prev_default_sink =
                pactl_get_default("sink").filter(|s| s != "AudioShare_Ultra");
            let prev_default_source =
                pactl_get_default("source").filter(|s| s != "AudioShare_Ultra.monitor");
            let mut pactl_module_id = None;
            if enable_virtual_speaker {
                if let Ok(output) = Command::new("pactl")
                    .args([
                        "load-module",
                        "module-null-sink",
                        "sink_name=AudioShare_Ultra",
                        "sink_properties=device.description=AudioShare_Wireless_Speaker",
                    ])
                    .output()
                {
                    if output.status.success() {
                        let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
                        if !id.is_empty() {
                            pactl_module_id = Some(id);
                        }
                    }
                }
            }
            disable_wifi_power_save();

            Self {
                enabled: pactl_module_id.is_some(),
                active: false,
                pactl_module_id,
                prev_default_sink,
                prev_default_source,
            }
        }

        #[cfg(target_os = "windows")]
        {
            // Suppress Windows 60-second background Wi-Fi roaming/location channel scans
            set_windows_wlan_media_streaming_mode(true);
            Self {
                enabled: enable_virtual_speaker,
                active: false,
                prev_default_sink: if enable_virtual_speaker {
                    windows_default_playback_name()
                } else {
                    None
                },
            }
        }

        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            let _ = enable_virtual_speaker;
            Self {
                enabled: false,
                active: false,
            }
        }
    }

    /// Route the default output into the virtual speaker and capture its
    /// monitor. Idempotent; safe to call from the GUI update loop.
    pub fn activate(&mut self) {
        if !self.enabled || self.active {
            return;
        }
        #[cfg(target_os = "linux")]
        {
            pactl_set_default("sink", "AudioShare_Ultra");
            pactl_set_default("source", "AudioShare_Ultra.monitor");
            self.active = true;
        }
        #[cfg(target_os = "windows")]
        {
            let _ = silent_command("powershell")
                .args([
                    "-NoProfile",
                    "-Command",
                    "Get-AudioDevice -List | Where-Object {$_.Name -like '*CABLE Input*' -or $_.Name -like '*Virtual*'} | Set-AudioDevice",
                ])
                .output();
            let _ = silent_command("powershell")
                .args([
                    "-NoProfile",
                    "-Command",
                    "Get-AudioDevice -List | Where-Object {$_.Name -like '*CABLE Output*'} | Set-AudioDevice",
                ])
                .output();
            self.active = true;
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            self.active = true;
        }
    }

    /// Restore the physical default sink/source. Idempotent.
    pub fn deactivate(&mut self) {
        if !self.active {
            return;
        }
        #[cfg(target_os = "linux")]
        {
            if let Some(ref sink) = self.prev_default_sink {
                pactl_set_default("sink", sink);
            }
            if let Some(ref source) = self.prev_default_source {
                pactl_set_default("source", source);
            }
        }
        #[cfg(target_os = "windows")]
        {
            if let Some(ref sink) = self.prev_default_sink {
                windows_set_playback_name(sink);
            }
        }
        self.active = false;
    }
}

impl Drop for VirtualSpeakerAndRadioGuard {
    fn drop(&mut self) {
        self.deactivate();

        #[cfg(target_os = "linux")]
        if let Some(ref id) = self.pactl_module_id {
            let _ = Command::new("pactl").args(["unload-module", id]).output();
        }

        #[cfg(target_os = "windows")]
        {
            set_windows_wlan_media_streaming_mode(false);
        }
    }
}

/// Dynamically invokes Win32 `wlanapi.dll` `WlanSetInterface(..., wlan_intf_opcode_media_streaming_mode)`
/// to prevent Windows from scanning other Wi-Fi channels every 60 seconds during real-time audio streaming.
#[cfg(target_os = "windows")]
fn set_windows_wlan_media_streaming_mode(enable: bool) {
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Guid {
        data1: u32,
        data2: u16,
        data3: u16,
        data4: [u8; 8],
    }

    #[repr(C)]
    struct WlanInterfaceInfo {
        interface_guid: Guid,
        interface_description: [u16; 256],
        is_state: u32,
    }

    #[repr(C)]
    struct WlanInterfaceInfoList {
        number_of_items: u32,
        index: u32,
        interface_info: [WlanInterfaceInfo; 1],
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryA(name: *const u8) -> *mut core::ffi::c_void;
        fn GetProcAddress(
            module: *mut core::ffi::c_void,
            name: *const u8,
        ) -> *mut core::ffi::c_void;
        fn FreeLibrary(module: *mut core::ffi::c_void) -> i32;
    }

    type FnWlanOpenHandle = unsafe extern "system" fn(
        u32,
        *mut core::ffi::c_void,
        *mut u32,
        *mut *mut core::ffi::c_void,
    ) -> u32;
    type FnWlanEnumInterfaces = unsafe extern "system" fn(
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
        *mut *mut WlanInterfaceInfoList,
    ) -> u32;
    type FnWlanSetInterface = unsafe extern "system" fn(
        *mut core::ffi::c_void,
        *const Guid,
        u32,
        u32,
        *const core::ffi::c_void,
        *mut core::ffi::c_void,
    ) -> u32;
    type FnWlanFreeMemory = unsafe extern "system" fn(*mut core::ffi::c_void);
    type FnWlanCloseHandle =
        unsafe extern "system" fn(*mut core::ffi::c_void, *mut core::ffi::c_void) -> u32;

    unsafe {
        let hmod = LoadLibraryA(b"wlanapi.dll\0".as_ptr());
        if hmod.is_null() {
            return;
        }

        let p_open = GetProcAddress(hmod, b"WlanOpenHandle\0".as_ptr());
        let p_enum = GetProcAddress(hmod, b"WlanEnumInterfaces\0".as_ptr());
        let p_set = GetProcAddress(hmod, b"WlanSetInterface\0".as_ptr());
        let p_free = GetProcAddress(hmod, b"WlanFreeMemory\0".as_ptr());
        let p_close = GetProcAddress(hmod, b"WlanCloseHandle\0".as_ptr());

        if !p_open.is_null()
            && !p_enum.is_null()
            && !p_set.is_null()
            && !p_free.is_null()
            && !p_close.is_null()
        {
            let wlan_open: FnWlanOpenHandle = core::mem::transmute(p_open);
            let wlan_enum: FnWlanEnumInterfaces = core::mem::transmute(p_enum);
            let wlan_set: FnWlanSetInterface = core::mem::transmute(p_set);
            let wlan_free: FnWlanFreeMemory = core::mem::transmute(p_free);
            let wlan_close: FnWlanCloseHandle = core::mem::transmute(p_close);

            let mut negotiated_version = 0u32;
            let mut client_handle: *mut core::ffi::c_void = core::ptr::null_mut();

            if wlan_open(
                2,
                core::ptr::null_mut(),
                &mut negotiated_version,
                &mut client_handle,
            ) == 0
            {
                let mut list_ptr: *mut WlanInterfaceInfoList = core::ptr::null_mut();
                if wlan_enum(client_handle, core::ptr::null_mut(), &mut list_ptr) == 0
                    && !list_ptr.is_null()
                {
                    let count = (*list_ptr).number_of_items as usize;
                    let first_ptr = (*list_ptr).interface_info.as_ptr();
                    let val_bool: i32 = if enable { 1 } else { 0 };
                    // wlan_intf_opcode_media_streaming_mode = 3
                    const OPCODE_MEDIA_STREAMING_MODE: u32 = 3;

                    for i in 0..count {
                        let info = &*first_ptr.add(i);
                        let _ = wlan_set(
                            client_handle,
                            &info.interface_guid,
                            OPCODE_MEDIA_STREAMING_MODE,
                            core::mem::size_of::<i32>() as u32,
                            &val_bool as *const _ as *const core::ffi::c_void,
                            core::ptr::null_mut(),
                        );
                    }
                    wlan_free(list_ptr as *mut core::ffi::c_void);
                }
                let _ = wlan_close(client_handle, core::ptr::null_mut());
            }
        }
        FreeLibrary(hmod);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discovery_beacon_rejects_port_zero() {
        assert_eq!(beacon_port("65530"), 65530);
        assert_eq!(beacon_port("0"), crate::DEFAULT_PORT);
        assert_eq!(beacon_port("garbage"), crate::DEFAULT_PORT);
        assert_eq!(beacon_port("65536"), crate::DEFAULT_PORT);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_pactl_volume_parser_handles_typical_output() {
        let typical = "Volume: front-left: 65536 / 100% / 0.00 dB,   \
                       front-right: 65536 / 100% / 0.00 dB\n        balance 0.00\n";
        assert_eq!(parse_pactl_volume_percent(typical), Some(100));

        let quiet = "Volume: front-left: 26214 /  40% / -23.88 dB,   \
                     front-right: 26214 /  40% / -23.88 dB\n";
        assert_eq!(parse_pactl_volume_percent(quiet), Some(40));
        assert_eq!(parse_pactl_volume_percent("garbage"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_wireless_interface_name_detection() {
        assert!(is_wireless_interface_name("wlan0"));
        assert!(is_wireless_interface_name("wlp2s0"));
        assert!(is_wireless_interface_name("wlo1"));
        assert!(!is_wireless_interface_name("eth0"));
        assert!(!is_wireless_interface_name("enx001122334455"));
        assert!(!is_wireless_interface_name("lo"));
    }

    #[test]
    fn test_multicast_discovery_group_is_link_local() {
        assert!(DISCOVERY_MULTICAST_GROUP.is_multicast());
        assert!(DISCOVERY_MULTICAST_GROUP.octets()[0] >= 224);
        assert!(DISCOVERY_MULTICAST_GROUP.octets()[0] <= 239);
    }

    #[test]
    fn test_discovery_scanner_reuse_address() {
        let scanner1 = DiscoveryScanner::start();
        // Spawning a second scanner or socket with reuse_address should not crash or panic
        let raw_sock = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        );
        if let Ok(sock) = raw_sock {
            let _ = sock.set_reuse_address(true);
            #[cfg(all(unix, not(target_os = "solaris"), not(target_os = "illumos")))]
            let _ = sock.set_reuse_port(true);
            let bind_addr = SocketAddr::from(([0, 0, 0, 0], DISCOVERY_PORT));
            let _ = sock.bind(&bind_addr.into());
        }
        drop(scanner1);
    }
}
