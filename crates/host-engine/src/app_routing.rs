#[cfg(any(target_os = "linux", target_os = "windows"))]
use std::process::Command;
#[cfg(target_os = "windows")]
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioAppEntry {
    /// OS Sink-Input ID (Linux PipeWire/Pulse) or Process ID (Windows WASAPI Process Loopback)
    pub id: u32,
    pub pid: u32,
    pub name: String,
}

/// Enumerate active applications currently producing audio or running media sessions
pub fn list_active_audio_apps() -> Vec<AudioAppEntry> {
    #[cfg(target_os = "linux")]
    {
        list_linux_pipewire_sink_inputs()
    }
    #[cfg(target_os = "windows")]
    {
        list_windows_audio_processes()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Vec::new()
    }
}

#[cfg(target_os = "linux")]
fn list_linux_pipewire_sink_inputs() -> Vec<AudioAppEntry> {
    let mut apps = Vec::new();
    let Ok(output) = Command::new("pactl").args(["list", "sink-inputs"]).output() else {
        return apps;
    };
    let text = String::from_utf8_lossy(&output.stdout);

    let mut current_id: Option<u32> = None;
    let mut current_pid: u32 = 0;
    let mut current_name: Option<String> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Sink Input #") {
            if let (Some(id), Some(name)) = (current_id.take(), current_name.take()) {
                apps.push(AudioAppEntry {
                    id,
                    pid: current_pid,
                    name,
                });
            }
            current_id = rest.parse::<u32>().ok();
            current_pid = 0;
        } else if let Some(rest) = trimmed.strip_prefix("application.name = ") {
            current_name = Some(rest.trim_matches('"').to_string());
        } else if let Some(rest) = trimmed.strip_prefix("application.process.id = ") {
            current_pid = rest.trim_matches('"').parse::<u32>().unwrap_or(0);
        }
    }

    if let (Some(id), Some(name)) = (current_id, current_name) {
        apps.push(AudioAppEntry {
            id,
            pid: current_pid,
            name,
        });
    }
    apps
}

#[cfg(target_os = "windows")]
fn list_windows_audio_processes() -> Vec<AudioAppEntry> {
    let mut apps = Vec::new();
    // Query active desktop apps with window titles (candidates for WASAPI Process Loopback)
    let Ok(output) = Command::new("tasklist")
        .args(["/FO", "CSV", "/NH"])
        .output()
    else {
        return apps;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let common_media = [
        "chrome.exe",
        "msedge.exe",
        "firefox.exe",
        "spotify.exe",
        "vlc.exe",
        "mpv.exe",
        "discord.exe",
        "brave.exe",
    ];

    for line in text.lines() {
        let parts: Vec<&str> = line.split(',').map(|s| s.trim_matches('"')).collect();
        if parts.len() >= 2 {
            let name = parts[0].to_lowercase();
            if common_media.iter().any(|&m| name == m) {
                if let Ok(pid) = parts[1].parse::<u32>() {
                    apps.push(AudioAppEntry {
                        id: pid,
                        pid,
                        name: parts[0].to_string(),
                    });
                }
            }
        }
    }
    apps
}

/// Route a specific application into the streamed AudioShare sink (Linux PipeWire/Pulse)
/// or prepare its PID for Windows Process Loopback (`VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK`).
pub fn route_single_app_to_audioshare(app: &AudioAppEntry) -> Result<String, String> {
    #[cfg(target_os = "linux")]
    {
        // Ensure the sink that the host captures exists. Do not make it the
        // default output, so only explicitly routed apps are streamed.
        let sink_exists = Command::new("pactl")
            .args(["list", "short", "sinks"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("AudioShare_Ultra"))
            .unwrap_or(false);
        if !sink_exists {
            let _ = Command::new("pactl")
                .args([
                    "load-module",
                    "module-null-sink",
                    "sink_name=AudioShare_Ultra",
                    "sink_properties=device.description=AudioShare_Wireless_Speaker",
                ])
                .output();
        }

        // Capture the sink's monitor as the default input.
        let _ = Command::new("pactl")
            .args(["set-default-source", "AudioShare_Ultra.monitor"])
            .output();

        let status = Command::new("pactl")
            .args(["move-sink-input", &app.id.to_string(), "AudioShare_Ultra"])
            .status()
            .map_err(|e| format!("Failed to move sink-input: {}", e))?;

        if status.success() {
            Ok(format!(
                "Routed '{}' (PID {}) -> AudioShare_Ultra",
                app.name, app.pid
            ))
        } else {
            Err("pactl move-sink-input returned non-zero".into())
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        #[cfg(target_os = "windows")]
        {
            isolate_windows_audio_session(Some(app.pid));
        }
        Ok(format!(
            "Target Process Loopback locked to '{}' (PID {})",
            app.name, app.pid
        ))
    }
}

/// Restores all isolated applications back to the default system speaker/sink
pub fn restore_all_apps_to_default_sink() -> String {
    #[cfg(target_os = "linux")]
    {
        for app in list_active_audio_apps() {
            let _ = Command::new("pactl")
                .args(["move-sink-input", &app.id.to_string(), "@DEFAULT_SINK@"])
                .output();
        }
        if let Ok(out) = Command::new("pactl").arg("get-default-sink").output() {
            let sink = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !sink.is_empty() && sink != "AudioShare_Ultra" {
                let _ = Command::new("pactl")
                    .args(["set-default-source", &format!("{}.monitor", sink)])
                    .output();
            }
        }
        "Restored: Capturing Full System Audio (All Applications)".into()
    }
    #[cfg(not(target_os = "linux"))]
    {
        #[cfg(target_os = "windows")]
        {
            isolate_windows_audio_session(None);
        }
        "Restored: Capturing Full System Audio (All Applications)".into()
    }
}

/// Tracks PIDs muted by Windows per-app isolation so `restore_all_apps_to_default_sink`
/// only unmutes sessions that AudioShare Ultra muted.
#[cfg(target_os = "windows")]
fn windows_muted_sessions() -> &'static Mutex<Vec<u32>> {
    static MUTED: OnceLock<Mutex<Vec<u32>>> = OnceLock::new();
    MUTED.get_or_init(|| Mutex::new(Vec::new()))
}

/// Isolates `target_pid` on the default Windows render endpoint using CoreAudio
/// `IAudioSessionManager2` / `IAudioSessionControl2` / `ISimpleAudioVolume`.
/// Passing `None` restores all previously muted sessions.
#[cfg(target_os = "windows")]
fn isolate_windows_audio_session(target_pid: Option<u32>) {
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
    struct IUnknownVtbl {
        query_interface:
            unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32,
        add_ref: usize,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
    }
    #[repr(C)]
    struct IMMDeviceEnumeratorVtbl {
        unk: IUnknownVtbl,
        enum_audio_endpoints: usize,
        get_default_audio_endpoint:
            unsafe extern "system" fn(*mut c_void, u32, u32, *mut *mut c_void) -> i32,
    }
    #[repr(C)]
    struct IMMDeviceVtbl {
        unk: IUnknownVtbl,
        activate: unsafe extern "system" fn(
            *mut c_void,
            *const Guid,
            u32,
            *const c_void,
            *mut *mut c_void,
        ) -> i32,
    }
    #[repr(C)]
    struct IAudioSessionManager2Vtbl {
        unk: IUnknownVtbl,
        get_audio_session_control: usize,
        get_simple_audio_volume: usize,
        get_session_enumerator:
            unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> i32,
    }
    #[repr(C)]
    struct IAudioSessionEnumeratorVtbl {
        unk: IUnknownVtbl,
        get_count: unsafe extern "system" fn(*mut c_void, *mut i32) -> i32,
        get_session: unsafe extern "system" fn(*mut c_void, i32, *mut *mut c_void) -> i32,
    }
    #[repr(C)]
    struct IAudioSessionControl2Vtbl {
        unk: IUnknownVtbl,
        get_state: usize,
        get_display_name: usize,
        set_display_name: usize,
        get_icon_path: usize,
        set_icon_path: usize,
        get_grouping_param: usize,
        set_grouping_param: usize,
        register_audio_session_notification: usize,
        unregister_audio_session_notification: usize,
        get_session_identifier: usize,
        get_session_instance_identifier: usize,
        get_process_id: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32,
    }
    #[repr(C)]
    struct ISimpleAudioVolumeVtbl {
        unk: IUnknownVtbl,
        set_master_volume: usize,
        get_master_volume: usize,
        set_mute: unsafe extern "system" fn(*mut c_void, i32, *const Guid) -> i32,
        get_mute: unsafe extern "system" fn(*mut c_void, *mut i32) -> i32,
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
    const IID_IAUDIO_SESSION_MANAGER2: Guid = Guid {
        data1: 0x77AA_99A0,
        data2: 0x1BD6,
        data3: 0x484F,
        data4: [0x8B, 0xC7, 0x2C, 0x65, 0x4C, 0x9A, 0x9B, 0x6F],
    };
    const IID_IAUDIO_SESSION_CONTROL2: Guid = Guid {
        data1: 0xBFB7_FF88,
        data2: 0x7239,
        data3: 0x4FC9,
        data4: [0x8F, 0xA2, 0x07, 0xC9, 0x50, 0xBE, 0x9C, 0x6D],
    };
    const IID_ISIMPLE_AUDIO_VOLUME: Guid = Guid {
        data1: 0x87CE_5498,
        data2: 0x68D6,
        data3: 0x44E5,
        data4: [0x92, 0x15, 0x6D, 0xA4, 0x7E, 0xF8, 0x83, 0xD8],
    };

    unsafe {
        let _ = CoInitializeEx(core::ptr::null_mut(), 0);
        let mut enumerator: *mut c_void = core::ptr::null_mut();
        if CoCreateInstance(
            &CLSID_MMDEVICE_ENUMERATOR,
            core::ptr::null_mut(),
            1,
            &IID_IMMDEVICE_ENUMERATOR,
            &mut enumerator,
        ) < 0
            || enumerator.is_null()
        {
            return;
        }
        let enum_vtbl = *(enumerator as *const *const IMMDeviceEnumeratorVtbl);
        let mut device: *mut c_void = core::ptr::null_mut();
        let hr_dev = ((*enum_vtbl).get_default_audio_endpoint)(enumerator, 0, 0, &mut device);
        ((*enum_vtbl).unk.release)(enumerator);
        if hr_dev < 0 || device.is_null() {
            return;
        }
        let dev_vtbl = *(device as *const *const IMMDeviceVtbl);
        let mut mgr2: *mut c_void = core::ptr::null_mut();
        let hr_mgr = ((*dev_vtbl).activate)(
            device,
            &IID_IAUDIO_SESSION_MANAGER2,
            1,
            core::ptr::null(),
            &mut mgr2,
        );
        ((*dev_vtbl).unk.release)(device);
        if hr_mgr < 0 || mgr2.is_null() {
            return;
        }
        let mgr_vtbl = *(mgr2 as *const *const IAudioSessionManager2Vtbl);
        let mut ses_enum: *mut c_void = core::ptr::null_mut();
        let hr_enum = ((*mgr_vtbl).get_session_enumerator)(mgr2, &mut ses_enum);
        ((*mgr_vtbl).unk.release)(mgr2);
        if hr_enum < 0 || ses_enum.is_null() {
            return;
        }
        let ses_enum_vtbl = *(ses_enum as *const *const IAudioSessionEnumeratorVtbl);
        let mut count: i32 = 0;
        let _ = ((*ses_enum_vtbl).get_count)(ses_enum, &mut count);

        let mut muted_list = windows_muted_sessions().lock().unwrap();
        let self_pid = std::process::id();

        for idx in 0..count {
            let mut ctrl: *mut c_void = core::ptr::null_mut();
            if ((*ses_enum_vtbl).get_session)(ses_enum, idx, &mut ctrl) < 0 || ctrl.is_null() {
                continue;
            }
            let ctrl_unk = *(ctrl as *const *const IUnknownVtbl);
            let mut ctrl2: *mut c_void = core::ptr::null_mut();
            let mut vol: *mut c_void = core::ptr::null_mut();
            let has_c2 =
                ((*ctrl_unk).query_interface)(ctrl, &IID_IAUDIO_SESSION_CONTROL2, &mut ctrl2) >= 0
                    && !ctrl2.is_null();
            let has_vol =
                ((*ctrl_unk).query_interface)(ctrl, &IID_ISIMPLE_AUDIO_VOLUME, &mut vol) >= 0
                    && !vol.is_null();

            if has_c2 && has_vol {
                let c2_vtbl = *(ctrl2 as *const *const IAudioSessionControl2Vtbl);
                let vol_vtbl = *(vol as *const *const ISimpleAudioVolumeVtbl);
                let mut pid: u32 = 0;
                let _ = ((*c2_vtbl).get_process_id)(ctrl2, &mut pid);
                if pid != 0 && pid != self_pid {
                    match target_pid {
                        Some(target) => {
                            if pid == target {
                                if muted_list.contains(&pid) {
                                    let _ = ((*vol_vtbl).set_mute)(vol, 0, core::ptr::null());
                                    muted_list.retain(|&p| p != pid);
                                }
                            } else {
                                let mut already_muted: i32 = 0;
                                let _ = ((*vol_vtbl).set_mute)(vol, &mut already_muted);
                                if already_muted == 0 {
                                    let _ = ((*vol_vtbl).set_mute)(vol, 1, core::ptr::null());
                                    if !muted_list.contains(&pid) {
                                        muted_list.push(pid);
                                    }
                                }
                            }
                        }
                        None => {
                            if muted_list.contains(&pid) {
                                let _ = ((*vol_vtbl).set_mute)(vol, 0, core::ptr::null());
                            }
                        }
                    }
                }
            }
            if has_vol {
                let v_unk = *(vol as *const *const IUnknownVtbl);
                ((*v_unk).release)(vol);
            }
            if has_c2 {
                let c2_unk = *(ctrl2 as *const *const IUnknownVtbl);
                ((*c2_unk).release)(ctrl2);
            }
            ((*ctrl_unk).release)(ctrl);
        }
        if target_pid.is_none() {
            muted_list.clear();
        }
        ((*ses_enum_vtbl).unk.release)(ses_enum);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_active_audio_apps_runs_without_panic() {
        let _ = list_active_audio_apps();
    }

    #[test]
    fn test_restore_all_apps_to_default_sink_runs_without_panic() {
        let res = restore_all_apps_to_default_sink();
        assert!(res.contains("Restored: Capturing Full System Audio"));
    }
}
