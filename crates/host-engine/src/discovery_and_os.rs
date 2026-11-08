use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const DISCOVERY_PORT: u16 = 65531;
const BEACON_PREFIX: &str = "ASULTRA_BEACON|";

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
                let payload = format!("{}{}|{}", BEACON_PREFIX, host_label, stream_port);

                while run_clone.load(Ordering::Relaxed) {
                    let _ = sock.send_to(
                        payload.as_bytes(),
                        SocketAddr::from(([255, 255, 255, 255], DISCOVERY_PORT)),
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
                let Ok(sock) = UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT)) else {
                    return;
                };
                let _ = sock.set_read_timeout(Some(Duration::from_millis(250)));
                let mut buf = [0u8; 256];

                while run_clone.load(Ordering::Relaxed) {
                    if let Ok((len, src)) = sock.recv_from(&mut buf) {
                        if let Ok(msg) = std::str::from_utf8(&buf[..len]) {
                            if let Some(rest) = msg.strip_prefix(BEACON_PREFIX) {
                                let parts: Vec<&str> = rest.split('|').collect();
                                if parts.len() >= 2 {
                                    let hostname = parts[0].to_string();
                                    let port: u16 = parts[1].parse().unwrap_or(crate::DEFAULT_PORT);
                                    let host_addr = SocketAddr::new(src.ip(), port);
                                    let is_usb = src.ip().to_string().starts_with("192.168.42.")
                                        || src.ip().to_string().starts_with("192.168.137.");

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
        let mut list: Vec<DiscoveredHost> =
            self.hosts.lock().unwrap().values().cloned().collect();
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
                    let has_device = Command::new("adb")
                        .args(["get-state"])
                        .output()
                        .map(|o| o.status.success())
                        .unwrap_or(false);

                    usb_clone.store(has_device, Ordering::Relaxed);

                    if has_device && !was_connected {
                        let port_spec = format!("tcp:{}", port);
                        let _ = Command::new("adb")
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

impl DirectHotspotController {
    pub fn start_direct_5ghz_hotspot(ssid: &str, password: &str) -> Result<String, String> {
        #[cfg(target_os = "linux")]
        {
            let output = Command::new("nmcli")
                .args([
                    "device",
                    "wifi",
                    "hotspot",
                    "ssid",
                    ssid,
                    "password",
                    password,
                    "band",
                    "a",
                ])
                .output()
                .map_err(|e| format!("Failed to invoke nmcli: {}", e))?;

            if output.status.success() {
                Ok(format!("1-Hop 5GHz Direct Link '{}' Active (IP: 10.42.0.1)", ssid))
            } else {
                Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
            }
        }

        #[cfg(target_os = "windows")]
        {
            let script = format!(
                "netsh wlan set hostednetwork mode=allow ssid={} key={}; netsh wlan start hostednetwork",
                ssid, password
            );
            let _ = Command::new("powershell")
                .args(["-NoProfile", "-Command", &script])
                .output();
            Ok(format!("1-Hop Direct Link '{}' Requested", ssid))
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
            let _ = Command::new("netsh")
                .args(["wlan", "stop", "hostednetwork"])
                .output();
        }
    }
}

/// Virtual Speaker & OS Wi-Fi Scan Suppressor Guard
pub struct VirtualSpeakerAndRadioGuard {
    #[cfg(target_os = "linux")]
    pactl_module_id: Option<String>,
    #[cfg(target_os = "linux")]
    prev_default_sink: Option<String>,
    #[cfg(target_os = "linux")]
    prev_default_source: Option<String>,
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

impl VirtualSpeakerAndRadioGuard {
    pub fn acquire(enable_virtual_speaker: bool) -> Self {
        #[cfg(target_os = "linux")]
        {
            let prev_default_sink = pactl_get_default("sink");
            let prev_default_source = pactl_get_default("source");
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
                            // Route the default output into the streamed null sink,
                            // and capture that sink's monitor as the default input.
                            pactl_set_default("sink", "AudioShare_Ultra");
                            pactl_set_default("source", "AudioShare_Ultra.monitor");
                            pactl_module_id = Some(id);
                        }
                    }
                }
            }
            let _ = Command::new("iw")
                .args(["dev", "wlan0", "set", "power_save", "off"])
                .output();

            Self {
                pactl_module_id,
                prev_default_sink,
                prev_default_source,
            }
        }

        #[cfg(target_os = "windows")]
        {
            if enable_virtual_speaker {
                // Switch playback to a Virtual Audio Cable / Virtual Speaker and
                // capture from its recording endpoint.
                let _ = Command::new("powershell")
                    .args([
                        "-NoProfile",
                        "-Command",
                        "Get-AudioDevice -List | Where-Object {$_.Name -like '*CABLE Input*' -or $_.Name -like '*Virtual*'} | Set-AudioDevice",
                    ])
                    .output();
                let _ = Command::new("powershell")
                    .args([
                        "-NoProfile",
                        "-Command",
                        "Get-AudioDevice -List | Where-Object {$_.Name -like '*CABLE Output*'} | Set-AudioDevice",
                    ])
                    .output();
            }
            // Suppress Windows 60-second background Wi-Fi roaming/location channel scans
            set_windows_wlan_media_streaming_mode(true);
            Self {}
        }

        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            let _ = enable_virtual_speaker;
            Self {}
        }
    }
}

impl Drop for VirtualSpeakerAndRadioGuard {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        {
            // Restore the user's previous routing before removing the null sink.
            if let Some(sink) = self.prev_default_sink.take() {
                pactl_set_default("sink", &sink);
            }
            if let Some(source) = self.prev_default_source.take() {
                pactl_set_default("source", &source);
            }
            if let Some(ref id) = self.pactl_module_id {
                let _ = Command::new("pactl")
                    .args(["unload-module", id])
                    .output();
            }
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
        fn GetProcAddress(module: *mut core::ffi::c_void, name: *const u8) -> *mut core::ffi::c_void;
        fn FreeLibrary(module: *mut core::ffi::c_void) -> i32;
    }

    type FnWlanOpenHandle =
        unsafe extern "system" fn(u32, *mut core::ffi::c_void, *mut u32, *mut *mut core::ffi::c_void) -> u32;
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

            if wlan_open(2, core::ptr::null_mut(), &mut negotiated_version, &mut client_handle) == 0
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
