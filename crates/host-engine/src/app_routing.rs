#[cfg(target_os = "linux")]
use std::process::Command;
#[cfg(target_os = "windows")]
use std::sync::{Mutex, OnceLock};

#[cfg(target_os = "windows")]
use crate::discovery_and_os::silent_command;

/// Native Win32 CoreAudio endpoint switching via `IMMDeviceEnumerator` +
/// `IPolicyConfig::SetDefaultEndpoint` (vtable slot 13). Works on stock
/// Windows 10/11 with zero PowerShell / third-party AudioDeviceCmdlets.
#[cfg(target_os = "windows")]
mod windows_endpoint_switch {
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
    #[derive(Clone, Copy)]
    struct PropertyKey {
        fmtid: Guid,
        pid: u32,
    }

    #[repr(C)]
    struct PropVariant {
        vt: u16,
        r1: u16,
        r2: u16,
        r3: u16,
        val: usize,
        extra: usize,
    }

    #[repr(C)]
    struct IUnknownVtbl {
        query_interface: usize,
        add_ref: usize,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
    }
    #[repr(C)]
    struct IMMDeviceEnumeratorVtbl {
        unk: IUnknownVtbl,
        enum_audio_endpoints:
            unsafe extern "system" fn(*mut c_void, u32, u32, *mut *mut c_void) -> i32,
        get_default_audio_endpoint:
            unsafe extern "system" fn(*mut c_void, u32, u32, *mut *mut c_void) -> i32,
    }
    #[repr(C)]
    struct IMMDeviceCollectionVtbl {
        unk: IUnknownVtbl,
        get_count: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32,
        item: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32,
    }
    #[repr(C)]
    struct IMMDeviceVtbl {
        unk: IUnknownVtbl,
        activate: usize,
        open_property_store:
            unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32,
        get_id: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> i32,
    }
    #[repr(C)]
    struct IPropertyStoreVtbl {
        unk: IUnknownVtbl,
        get_count: usize,
        get_at: usize,
        get_value:
            unsafe extern "system" fn(*mut c_void, *const PropertyKey, *mut PropVariant) -> i32,
    }
    #[repr(C)]
    struct IPolicyConfigVtbl {
        unk: IUnknownVtbl,
        _slots_3_to_12: [usize; 10],
        set_default_endpoint:
            unsafe extern "system" fn(*mut c_void, *const u16, u32) -> i32,
    }

    #[link(name = "ole32")]
    extern "system" {
        fn CoInitializeEx(pv_reserved: *mut c_void, dw_co_init: u32) -> i32;
        fn CoUninitialize();
        fn CoCreateInstance(
            rclsid: *const Guid,
            p_unk_outer: *mut c_void,
            dw_cls_context: u32,
            riid: *const Guid,
            ppv: *mut *mut c_void,
        ) -> i32;
        fn CoTaskMemFree(pv: *mut c_void);
        fn PropVariantClear(pvar: *mut PropVariant) -> i32;
    }

    struct ComInit(bool);
    impl ComInit {
        fn new() -> Self {
            Self(unsafe { CoInitializeEx(core::ptr::null_mut(), 0) >= 0 })
        }
    }
    impl Drop for ComInit {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }

    const CLSID_MMDEVICE_ENUMERATOR: Guid = Guid {
        data1: 0xBCDE_0395, data2: 0xE52F, data3: 0x467C,
        data4: [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E],
    };
    const IID_IMMDEVICE_ENUMERATOR: Guid = Guid {
        data1: 0xA956_64D2, data2: 0x9614, data3: 0x4F35,
        data4: [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6],
    };
    const CLSID_POLICY_CONFIG_CLIENT: Guid = Guid {
        data1: 0x870A_F99C, data2: 0x171D, data3: 0x4F9E,
        data4: [0xAF, 0x0D, 0xE6, 0x3D, 0xF4, 0x0C, 0x2B, 0xC9],
    };
    const IID_IPOLICY_CONFIG: Guid = Guid {
        data1: 0xF867_9F50, data2: 0x850A, data3: 0x41CF,
        data4: [0x9C, 0x72, 0x43, 0x0F, 0x29, 0x02, 0x90, 0xC8],
    };
    const PKEY_DEVICE_FRIENDLY_NAME: PropertyKey = PropertyKey {
        fmtid: Guid {
            data1: 0xA45C_254E, data2: 0xDF1C, data3: 0x4EFD,
            data4: [0x80, 0x20, 0x67, 0xD1, 0x46, 0xA8, 0x50, 0xE0],
        },
        pid: 14,
    };

    unsafe fn read_wide_z(ptr: *const u16) -> Vec<u16> {
        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        let mut v = Vec::with_capacity(len + 1);
        v.extend_from_slice(core::slice::from_raw_parts(ptr, len));
        v.push(0);
        v
    }

    unsafe fn device_id(dev: *mut c_void) -> Option<Vec<u16>> {
        let vtbl = *(dev as *const *const IMMDeviceVtbl);
        let mut raw_id: *mut u16 = core::ptr::null_mut();
        if ((*vtbl).get_id)(dev, &mut raw_id) < 0 || raw_id.is_null() {
            return None;
        }
        let id = read_wide_z(raw_id);
        CoTaskMemFree(raw_id as *mut c_void);
        Some(id)
    }

    unsafe fn device_friendly_name(dev: *mut c_void) -> Option<String> {
        let vtbl = *(dev as *const *const IMMDeviceVtbl);
        let mut store: *mut c_void = core::ptr::null_mut();
        if ((*vtbl).open_property_store)(dev, 0, &mut store) < 0 || store.is_null() {
            return None;
        }
        let store_vtbl = *(store as *const *const IPropertyStoreVtbl);
        let mut pv = PropVariant { vt: 0, r1: 0, r2: 0, r3: 0, val: 0, extra: 0 };
        let hr = ((*store_vtbl).get_value)(store, &PKEY_DEVICE_FRIENDLY_NAME, &mut pv);
        let result = if hr >= 0 && pv.vt == 31 && pv.val != 0 {
            let wide = read_wide_z(pv.val as *const u16);
            Some(String::from_utf16_lossy(&wide[..wide.len().saturating_sub(1)]))
        } else {
            None
        };
        let _ = PropVariantClear(&mut pv);
        ((*store_vtbl).unk.release)(store);
        result
    }

    pub fn default_playback_id() -> Option<Vec<u16>> {
        unsafe {
            let _com = ComInit::new();
            let mut en: *mut c_void = core::ptr::null_mut();
            if CoCreateInstance(&CLSID_MMDEVICE_ENUMERATOR, core::ptr::null_mut(), 1, &IID_IMMDEVICE_ENUMERATOR, &mut en) < 0 || en.is_null() {
                return None;
            }
            let en_vtbl = *(en as *const *const IMMDeviceEnumeratorVtbl);
            let mut dev: *mut c_void = core::ptr::null_mut();
            let hr = ((*en_vtbl).get_default_audio_endpoint)(en, 0, 0, &mut dev);
            ((*en_vtbl).unk.release)(en);
            if hr < 0 || dev.is_null() {
                return None;
            }
            let id = device_id(dev);
            let dev_vtbl = *(dev as *const *const IMMDeviceVtbl);
            ((*dev_vtbl).unk.release)(dev);
            id
        }
    }

    pub fn find_virtual_playback_id() -> Option<Vec<u16>> {
        unsafe {
            let _com = ComInit::new();
            let mut en: *mut c_void = core::ptr::null_mut();
            if CoCreateInstance(&CLSID_MMDEVICE_ENUMERATOR, core::ptr::null_mut(), 1, &IID_IMMDEVICE_ENUMERATOR, &mut en) < 0 || en.is_null() {
                return None;
            }
            let en_vtbl = *(en as *const *const IMMDeviceEnumeratorVtbl);
            let mut col: *mut c_void = core::ptr::null_mut();
            let hr = ((*en_vtbl).enum_audio_endpoints)(en, 0, 1, &mut col);
            ((*en_vtbl).unk.release)(en);
            if hr < 0 || col.is_null() {
                return None;
            }
            let col_vtbl = *(col as *const *const IMMDeviceCollectionVtbl);
            let mut count = 0u32;
            let _ = ((*col_vtbl).get_count)(col, &mut count);
            let mut found = None;
            for i in 0..count {
                let mut dev: *mut c_void = core::ptr::null_mut();
                if ((*col_vtbl).item)(col, i, &mut dev) >= 0 && !dev.is_null() {
                    if let Some(name) = device_friendly_name(dev) {
                        let lower = name.to_ascii_lowercase();
                        if lower.contains("cable input")
                            || lower.contains("audioshare")
                            || lower.contains("virtual")
                        {
                            found = device_id(dev);
                        }
                    }
                    let dev_vtbl = *(dev as *const *const IMMDeviceVtbl);
                    ((*dev_vtbl).unk.release)(dev);
                    if found.is_some() {
                        break;
                    }
                }
            }
            ((*col_vtbl).unk.release)(col);
            found
        }
    }

    pub fn set_default_endpoint(device_id: &[u16]) {
        if device_id.is_empty() {
            return;
        }
        unsafe {
            let _com = ComInit::new();
            let mut policy: *mut c_void = core::ptr::null_mut();
            if CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, core::ptr::null_mut(), 1, &IID_IPOLICY_CONFIG, &mut policy) < 0 || policy.is_null() {
                return;
            }
            let vtbl = *(policy as *const *const IPolicyConfigVtbl);
            for role in 0..3u32 {
                let _ = ((*vtbl).set_default_endpoint)(policy, device_id.as_ptr(), role);
            }
            ((*vtbl).unk.release)(policy);
        }
    }
}

#[cfg(target_os = "windows")]
static WINDOWS_TARGET_CAPTURE_PID: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

#[cfg(target_os = "windows")]
pub fn windows_target_capture_pid() -> u32 {
    WINDOWS_TARGET_CAPTURE_PID.load(std::sync::atomic::Ordering::Acquire)
}

/// Owning handle for a WASAPI Process Loopback capture thread.
#[cfg(target_os = "windows")]
pub struct WindowsProcessLoopbackStream {
    active: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "windows")]
impl Drop for WindowsProcessLoopbackStream {
    fn drop(&mut self) {
        self.active.store(false, std::sync::atomic::Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
pub fn spawn_windows_process_loopback(
    target_pid: u32,
    ring: std::sync::Arc<audioshare_protocol::dsp::BurstDecouplerRing>,
    heartbeat_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    stream_error: std::sync::Arc<std::sync::atomic::AtomicBool>,
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    start_epoch: std::time::Instant,
) -> Result<WindowsProcessLoopbackStream, String> {
    use windows::core::{implement, AsImpl, IUnknown, Interface};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::Media::Audio::{
        ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
        IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
        IAudioCaptureClient, IAudioClient, AUDCLNT_BUFFERFLAGS_SILENT,
        AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
        AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
        AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDIOCLIENT_ACTIVATION_PARAMS,
        AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
        VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX,
    };
    use windows::Win32::System::Com::StructuredStorage::InitPropVariantFromBuffer;
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
    use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};

    #[implement(IActivateAudioInterfaceCompletionHandler)]
    struct CompletionHandler {
        event: HANDLE,
        result: std::sync::Mutex<Option<Result<IAudioClient, String>>>,
    }

    impl IActivateAudioInterfaceCompletionHandler_Impl for CompletionHandler_Impl {
        fn ActivateCompleted(
            &self,
            op: Option<&IActivateAudioInterfaceAsyncOperation>,
        ) -> windows::core::Result<()> {
            let res = (|| -> Result<IAudioClient, String> {
                let op = op.ok_or("Missing async operation")?;
                let mut hr = windows::core::HRESULT(0);
                let mut iface: Option<IUnknown> = None;
                unsafe { op.GetActivateResult(&mut hr, &mut iface) }
                    .map_err(|e| format!("GetActivateResult failed: {e}"))?;
                hr.ok().map_err(|e| format!("Activation HRESULT failed: {e}"))?;
                let unk = iface.ok_or("Null activated IAudioClient")?;
                unk.cast::<IAudioClient>()
                    .map_err(|e| format!("Cast to IAudioClient failed: {e}"))
            })();
            *self.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(res);
            unsafe { let _ = SetEvent(self.event); }
            Ok(())
        }
    }

    let active = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let active_t = std::sync::Arc::clone(&active);
    stream_error.store(false, std::sync::atomic::Ordering::Release);

    let handle = std::thread::Builder::new()
        .name("as-ultra-win-proc-loopback".into())
        .spawn(move || {
            let _rt = crate::RealtimeThreadGuard::enter();
            unsafe {
                let com_ok = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
                let res = (|| -> Result<(), String> {
                    let done_evt = CreateEventW(None, true, false, None)
                        .map_err(|e| format!("CreateEventW failed: {e}"))?;
                    let handler_impl = CompletionHandler {
                        event: done_evt,
                        result: std::sync::Mutex::new(None),
                    };
                    let handler: IActivateAudioInterfaceCompletionHandler = handler_impl.into();

                    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
                        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
                        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                                TargetProcessId: target_pid,
                                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                            },
                        },
                    };
                    let param_bytes = core::slice::from_raw_parts(
                        &params as *const AUDIOCLIENT_ACTIVATION_PARAMS as *const u8,
                        core::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>(),
                    );
                    let prop = InitPropVariantFromBuffer(
                        param_bytes.as_ptr() as *const core::ffi::c_void,
                        param_bytes.len() as u32,
                    )
                    .map_err(|e| format!("InitPropVariantFromBuffer failed: {e}"))?;

                    let _op = ActivateAudioInterfaceAsync(
                        VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
                        &IAudioClient::IID,
                        Some(&prop),
                        &handler,
                    )
                    .map_err(|e| format!("ActivateAudioInterfaceAsync failed: {e}"))?;

                    if WaitForSingleObject(done_evt, 3000) != WAIT_OBJECT_0 {
                        let _ = CloseHandle(done_evt);
                        return Err("Process loopback activation timed out".into());
                    }
                    let _ = CloseHandle(done_evt);
                    let client = handler.as_impl()
                        .result
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take()
                        .ok_or("No activation result")??;

                    let fmt = WAVEFORMATEX {
                        wFormatTag: 3,
                        nChannels: 2,
                        nSamplesPerSec: audioshare_protocol::SAMPLE_RATE,
                        nAvgBytesPerSec: audioshare_protocol::SAMPLE_RATE * 8,
                        nBlockAlign: 8,
                        wBitsPerSample: 32,
                        cbSize: 0,
                    };
                    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK
                        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY
                        | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
                    client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 200_000, 0, &fmt, None)
                        .map_err(|e| format!("IAudioClient::Initialize failed: {e}"))?;
                    let sample_evt = CreateEventW(None, false, false, None)
                        .map_err(|e| format!("CreateEventW(sample) failed: {e}"))?;
                    client.SetEventHandle(sample_evt)
                        .map_err(|e| format!("SetEventHandle failed: {e}"))?;
                    let capture: IAudioCaptureClient = client
                        .GetService()
                        .map_err(|e| format!("GetService(IAudioCaptureClient) failed: {e}"))?;
                    client.Start().map_err(|e| format!("IAudioClient::Start failed: {e}"))?;

                    let silence = [0.0f32; 2048];
                    while active_t.load(std::sync::atomic::Ordering::Relaxed)
                        && running.load(std::sync::atomic::Ordering::Relaxed)
                    {
                        let _ = WaitForSingleObject(sample_evt, 20);
                        while let Ok(pkt_frames) = capture.GetNextPacketSize() {
                            if pkt_frames == 0 { break; }
                            let mut data_ptr = core::ptr::null_mut();
                            let mut frames = 0u32;
                            let mut buf_flags = 0u32;
                            if capture.GetBuffer(&mut data_ptr, &mut frames, &mut buf_flags, None, None).is_err() {
                                stream_error.store(true, std::sync::atomic::Ordering::Release);
                                break;
                            }
                            heartbeat_ms.store(
                                start_epoch.elapsed().as_millis() as u64,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                            let total_samples = (frames as usize) * 2;
                            if (buf_flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data_ptr.is_null() {
                                let mut rem = total_samples;
                                while rem > 0 {
                                    let chunk = rem.min(silence.len());
                                    ring.push_samples(&silence[..chunk]);
                                    rem -= chunk;
                                }
                            } else {
                                let slice = core::slice::from_raw_parts(data_ptr as *const f32, total_samples);
                                ring.push_samples(slice);
                            }
                            let _ = capture.ReleaseBuffer(frames);
                        }
                    }
                    let _ = client.Stop();
                    let _ = CloseHandle(sample_evt);
                    Ok(())
                })();
                if let Err(err) = res {
                    audioshare_protocol::diagnostics::warn("win-proc-loopback", err);
                    stream_error.store(true, std::sync::atomic::Ordering::Release);
                }
                if com_ok { CoUninitialize(); }
            }
        })
        .map_err(|e| format!("Failed to spawn process loopback thread: {e}"))?;

    Ok(WindowsProcessLoopbackStream { active, handle: Some(handle) })
}

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
    let Ok(output) = silent_command("tasklist")
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
            // Capture only the chosen process tree via WASAPI Process Loopback
            // instead of muting every other app (which could stay muted on a
            // panic=abort crash). Keep the legacy mute path as a fallback when
            // no virtual endpoint is installed.
            WINDOWS_TARGET_CAPTURE_PID.store(
                app.pid,
                std::sync::atomic::Ordering::Release,
            );
            if let Some(virtual_id) = windows_endpoint_switch::find_virtual_playback_id() {
                windows_endpoint_switch::set_default_endpoint(&virtual_id);
            } else {
                isolate_windows_audio_session(Some(app.pid));
            }
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
            WINDOWS_TARGET_CAPTURE_PID.store(0, std::sync::atomic::Ordering::Release);
            if let Some(previous) = windows_endpoint_switch::default_playback_id() {
                windows_endpoint_switch::set_default_endpoint(&previous);
            }
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
        fn CoUninitialize();
        fn CoCreateInstance(
            rclsid: *const Guid,
            p_unk_outer: *mut c_void,
            dw_cls_context: u32,
            riid: *const Guid,
            ppv: *mut *mut c_void,
        ) -> i32;
    }

    // Balance COM init on every return path (including S_FALSE when COM was
    // already initialized on this thread).
    struct ComInit(bool);
    impl ComInit {
        fn new() -> Self {
            Self(unsafe { CoInitializeEx(core::ptr::null_mut(), 0) >= 0 })
        }
    }
    impl Drop for ComInit {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
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
        let _com_init = ComInit::new();
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
                                let _ = ((*vol_vtbl).get_mute)(vol, &mut already_muted);
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
