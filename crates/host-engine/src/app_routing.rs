#[cfg(target_os = "linux")]
use std::process::Command;

/// Sink-input IDs this host moved into `AudioShare_Ultra`. Only these are
/// restored on `restore_all_apps_to_default_sink`; unrelated applications must
/// keep whatever output the user chose.
#[cfg(target_os = "linux")]
static ROUTED_SINK_INPUTS: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

#[cfg(target_os = "windows")]
use crate::discovery_and_os::silent_command;

/// Native Win32 CoreAudio endpoint switching via `IMMDeviceEnumerator` +
/// `IPolicyConfig::SetDefaultEndpoint` (vtable slot 13). Works on stock
/// Windows 10/11 with zero PowerShell / third-party AudioDeviceCmdlets.
#[cfg(target_os = "windows")]
pub(crate) mod windows_endpoint_switch {
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
            let mut best_preferred: Option<(u8, Vec<u16>)> = None;
            let mut fallback = None;
            for i in 0..count {
                let mut dev: *mut c_void = core::ptr::null_mut();
                if ((*col_vtbl).item)(col, i, &mut dev) >= 0 && !dev.is_null() {
                    if let Some(name) = device_friendly_name(dev) {
                        let lower = name.to_ascii_lowercase();
                        // Prioritize VB-Cable (including renamed "Speakers
                        // (VB-Audio Virtual Cable)" and "CABLE Input") and
                        // AudioShare virtual speakers over generic "virtual"
                        // devices so unrelated drivers never hijack VB-Cable.
                        let prio = if lower.contains("audioshare") {
                            Some(0u8)
                        } else {
                            crate::vbcable_output_match_priority(&name)
                        };
                        if let Some(p) = prio {
                            if best_preferred.as_ref().map(|(bp, _)| p < *bp).unwrap_or(true) {
                                if let Some(id) = device_id(dev) {
                                    best_preferred = Some((p, id));
                                }
                            }
                        } else if fallback.is_none() && lower.contains("virtual") {
                            fallback = device_id(dev);
                        }
                    }
                    let dev_vtbl = *(dev as *const *const IMMDeviceVtbl);
                    ((*dev_vtbl).unk.release)(dev);
                }
            }
            ((*col_vtbl).unk.release)(col);
            best_preferred.map(|(_, id)| id).or(fallback)
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
pub(crate) fn spawn_windows_process_loopback(
    target_pid: u32,
    ring: std::sync::Arc<audioshare_protocol::dsp::BurstDecouplerRing>,
    wakeup: std::sync::Arc<crate::CaptureWakeup>,
    heartbeat_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    stream_error: std::sync::Arc<std::sync::atomic::AtomicBool>,
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    start_epoch: std::time::Instant,
) -> Result<WindowsProcessLoopbackStream, String> {
    use windows::core::{implement, IUnknown, Interface};
    use windows::core::PROPVARIANT;
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
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
    use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};

    /// RAII wrapper around a Win32 event HANDLE so late COM callbacks after a
    /// timeout never call `SetEvent` on a closed/recycled handle.
    struct OwnedHandle(HANDLE);
    unsafe impl Send for OwnedHandle {}
    unsafe impl Sync for OwnedHandle {}
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
    }

    // `windows-implement` 0.58 answers `IAgileObject` automatically in QI;
    // listing `IAgileObject` explicitly without `impl IAgileObject_Impl` fails compilation.
    #[implement(IActivateAudioInterfaceCompletionHandler)]
    struct CompletionHandler {
        event: std::sync::Arc<OwnedHandle>,
        result: std::sync::Arc<std::sync::Mutex<Option<Result<IAudioClient, String>>>>,
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
            unsafe { let _ = SetEvent(self.event.0); }
            Ok(())
        }
    }

    let active = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let active_t = std::sync::Arc::clone(&active);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);
    stream_error.store(false, std::sync::atomic::Ordering::Release);

    let handle = std::thread::Builder::new()
        .name("as-ultra-win-proc-loopback".into())
        .spawn(move || {
            let _rt = crate::RealtimeThreadGuard::enter();
            unsafe {
                let com_ok = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
                let mut started_ok = false;
                let res = (|| -> Result<(), String> {
                    let done_evt = std::sync::Arc::new(OwnedHandle(
                        CreateEventW(None, true, false, None)
                            .map_err(|e| format!("CreateEventW failed: {e}"))?,
                    ));
                    let result_slot = std::sync::Arc::new(std::sync::Mutex::new(None));
                    let handler_impl = CompletionHandler {
                        event: std::sync::Arc::clone(&done_evt),
                        result: std::sync::Arc::clone(&result_slot),
                    };
                    let handler: IActivateAudioInterfaceCompletionHandler = handler_impl.into();

                    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
                        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
                        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                                TargetProcessId: target_pid,
                                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                            },
                        },
                    };
                    // ActivateAudioInterfaceAsync requires VT_BLOB (0x0041);
                    // InitPropVariantFromBuffer produces VT_VECTOR|VT_UI1 -> E_INVALIDARG.
                    #[repr(C)]
                    struct RawBlobPropVariant {
                        vt: u16,
                        r1: u16,
                        r2: u16,
                        r3: u16,
                        cb_size: u32,
                        p_blob_data: *mut u8,
                    }
                    const _: () = assert!(
                        core::mem::size_of::<RawBlobPropVariant>()
                            == core::mem::size_of::<PROPVARIANT>()
                    );
                    let raw_prop = RawBlobPropVariant {
                        vt: 0x0041, // VT_BLOB
                        r1: 0,
                        r2: 0,
                        r3: 0,
                        cb_size: core::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        p_blob_data: &mut params as *mut _ as *mut u8,
                    };

                    let _op = ActivateAudioInterfaceAsync(
                        VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
                        &IAudioClient::IID,
                        Some(&raw_prop as *const RawBlobPropVariant as *const PROPVARIANT),
                        &handler,
                    )
                    .map_err(|e| format!("ActivateAudioInterfaceAsync failed: {e}"))?;

                    if WaitForSingleObject(done_evt.0, 3000) != WAIT_OBJECT_0 {
                        return Err("Process loopback activation timed out".into());
                    }
                    let client = result_slot
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
                    let sample_evt = OwnedHandle(
                        CreateEventW(None, false, false, None)
                            .map_err(|e| format!("CreateEventW(sample) failed: {e}"))?,
                    );
                    client.SetEventHandle(sample_evt.0)
                        .map_err(|e| format!("SetEventHandle failed: {e}"))?;
                    let capture: IAudioCaptureClient = client
                        .GetService()
                        .map_err(|e| format!("GetService(IAudioCaptureClient) failed: {e}"))?;
                    client.Start().map_err(|e| format!("IAudioClient::Start failed: {e}"))?;
                    started_ok = true;
                    let _ = ready_tx.send(Ok(()));

                    let silence = [0.0f32; 2048];
                    'capture: while active_t.load(std::sync::atomic::Ordering::Relaxed)
                        && running.load(std::sync::atomic::Ordering::Relaxed)
                    {
                        let _ = WaitForSingleObject(sample_evt.0, 20);
                        loop {
                            let pkt_frames = match capture.GetNextPacketSize() {
                                Ok(n) => n,
                                Err(_) => {
                                    // Endpoint/service invalidated: signal the
                                    // host watchdog so it rebinds cleanly
                                    // instead of spinning silently forever.
                                    stream_error.store(true, std::sync::atomic::Ordering::Release);
                                    break 'capture;
                                }
                            };
                            if pkt_frames == 0 {
                                break;
                            }
                            let mut data_ptr = core::ptr::null_mut();
                            let mut frames = 0u32;
                            let mut buf_flags = 0u32;
                            if capture.GetBuffer(&mut data_ptr, &mut frames, &mut buf_flags, None, None).is_err() {
                                stream_error.store(true, std::sync::atomic::Ordering::Release);
                                break 'capture;
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
                            // Never block the RT capture thread; wake the TX
                            // loop after publishing samples into the ring.
                            wakeup.notify();
                            let _ = capture.ReleaseBuffer(frames);
                        }
                    }
                    let _ = client.Stop();
                    Ok(())
                })();
                if let Err(err) = res {
                    audioshare_protocol::diagnostics::warn("win-proc-loopback", &err);
                    if !started_ok {
                        let _ = ready_tx.send(Err(err));
                    }
                    stream_error.store(true, std::sync::atomic::Ordering::Release);
                }
                if com_ok { CoUninitialize(); }
            }
        })
        .map_err(|e| format!("Failed to spawn process loopback thread: {e}"))?;

    match ready_rx.recv_timeout(std::time::Duration::from_secs(4)) {
        Ok(Ok(())) => Ok(WindowsProcessLoopbackStream { active, handle: Some(handle) }),
        Ok(Err(err)) => {
            active.store(false, std::sync::atomic::Ordering::Release);
            let _ = handle.join();
            Err(err)
        }
        Err(_) => {
            active.store(false, std::sync::atomic::Ordering::Release);
            Err("Process loopback startup timed out".into())
        }
    }
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

/// Live sink-input IDs currently routed to `sink_name` (PipeWire/Pulse).
#[cfg(target_os = "linux")]
fn sink_inputs_on_sink(sink_name: &str) -> Vec<u32> {
    // Column 2 of `pactl list short sink-inputs` is the numeric sink index
    // (not the sink name), so resolve `sink_name` -> numeric index first.
    let target_sink_idx: Option<String> = Command::new("pactl")
        .args(["list", "short", "sinks"])
        .output()
        .ok()
        .and_then(|o| {
            let text = String::from_utf8_lossy(&o.stdout);
            text.lines().find_map(|line| {
                let mut fields = line.split_whitespace();
                let idx = fields.next()?;
                let name = fields.next()?;
                if name == sink_name {
                    Some(idx.to_string())
                } else {
                    None
                }
            })
        });

    let Ok(output) = Command::new("pactl")
        .args(["list", "short", "sink-inputs"])
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut ids = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(id) = fields.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if let Some(col2) = fields.next() {
            if col2 == sink_name || target_sink_idx.as_deref() == Some(col2) {
                ids.push(id);
            }
        }
    }
    ids
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
    // Authoritative source: CoreAudio's own session enumerator. An app only
    // qualifies for WASAPI process loopback while it actually holds a render
    // session, which avoids the stale/guessed entries of a plain tasklist scan.
    let mut apps = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for pid in windows_audio_session_pids() {
        if pid == 0 || !seen.insert(pid) {
            continue; // dedup by PID: one process can own several sessions
        }
        let name = windows_process_image_name(pid).unwrap_or_else(|| format!("PID {pid}"));
        apps.push(AudioAppEntry {
            id: pid,
            pid,
            name,
        });
    }
    if !apps.is_empty() {
        return apps;
    }
    // Fallback for systems where COM enumeration is unavailable/empty.
    list_windows_audio_processes_tasklist()
}

/// Enumerate the PIDs behind every active CoreAudio render session via
/// `IAudioSessionManager2::GetSessionEnumerator` ->
/// `IAudioSessionControl2::GetProcessId`.
#[cfg(target_os = "windows")]
fn windows_audio_session_pids() -> Vec<u32> {
    use windows::core::Interface;
    use windows::Win32::Media::Audio::{
        IAudioSessionControl2, IAudioSessionEnumerator, IAudioSessionManager2, IMMDeviceEnumerator,
        MMDeviceEnumerator,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    let mut pids = Vec::new();
    unsafe {
        let com_ok = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
        let enumerated = (|| -> windows::core::Result<()> {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let endpoint = enumerator.GetDefaultAudioEndpoint(
                windows::Win32::Media::Audio::eRender,
                windows::Win32::Media::Audio::eMultimedia,
            )?;
            let manager: IAudioSessionManager2 = endpoint.Activate(CLSCTX_ALL, None)?;
            let sessions: IAudioSessionEnumerator = manager.GetSessionEnumerator()?;
            for index in 0..sessions.GetCount()? {
                if let Ok(control) = sessions.GetSession(index) {
                    if let Ok(control2) = control.cast::<IAudioSessionControl2>() {
                        if let Ok(pid) = control2.GetProcessId() {
                            pids.push(pid);
                        }
                    }
                }
            }
            Ok(())
        })();
        let _ = enumerated;
        if com_ok {
            CoUninitialize();
        }
    }
    pids
}

/// Resolve a PID to its executable image name (file name only).
#[cfg(target_os = "windows")]
fn windows_process_image_name(pid: u32) -> Option<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buffer = [0u16; 260];
        let mut len = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(handle);
        if !ok || len == 0 || len as usize > buffer.len() {
            return None;
        }
        let path = String::from_utf16_lossy(&buffer[..len as usize]);
        let name = path.rsplit(|c| c == '\\' || c == '/').next().unwrap_or(&path);
        if name.is_empty() {
            None
        } else {
            Some(name.to_string())
        }
    }
}

#[cfg(target_os = "windows")]
fn list_windows_audio_processes_tasklist() -> Vec<AudioAppEntry> {
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

        // Return any previously isolated apps back to @DEFAULT_SINK@ before
        // isolating the newly selected app so switching App A -> App B never
        // leaves App A muted on the null sink.
        let prev_routed: Vec<u32> = {
            let mut routed = ROUTED_SINK_INPUTS
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut prev = std::mem::take(&mut *routed);
            for id in sink_inputs_on_sink("AudioShare_Ultra") {
                if !prev.contains(&id) {
                    prev.push(id);
                }
            }
            prev
        };
        for id in prev_routed {
            if id != app.id {
                let _ = Command::new("pactl")
                    .args(["move-sink-input", &id.to_string(), "@DEFAULT_SINK@"])
                    .output();
            }
        }

        let status = Command::new("pactl")
            .args(["move-sink-input", &app.id.to_string(), "AudioShare_Ultra"])
            .status()
            .map_err(|e| format!("Failed to move sink-input: {}", e))?;

        if status.success() {
            // Only switch the capture source AFTER move-sink-input succeeded so
            // a failed route never leaves @DEFAULT_SOURCE@ stranded on an empty
            // null sink monitor.
            let _ = Command::new("pactl")
                .args(["set-default-source", "AudioShare_Ultra.monitor"])
                .output();
            if let Ok(mut routed) = ROUTED_SINK_INPUTS.lock() {
                if !routed.contains(&app.id) {
                    routed.push(app.id);
                }
            }
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
            // HostEngine captures only this process tree via native Win10/11
            // WASAPI Process Loopback without muting any other application or
            // touching the PC's default playback device.
            WINDOWS_TARGET_CAPTURE_PID.store(
                app.pid,
                std::sync::atomic::Ordering::Release,
            );
        }
        Ok(format!(
            "WASAPI Process Loopback locked to '{}' (PID {})",
            app.name, app.pid
        ))
    }
}

/// Restores all isolated applications back to the default system speaker/sink
pub fn restore_all_apps_to_default_sink() -> String {
    #[cfg(target_os = "linux")]
    {
        // Restore only what this host moved, plus any live input still parked
        // on AudioShare_Ultra (e.g. after a host restart lost the in-memory
        // record). Applications on their own output are left untouched.
        let recorded: Vec<u32> = {
            let mut routed = ROUTED_SINK_INPUTS
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *routed)
        };
        let mut to_restore = recorded;
        for id in sink_inputs_on_sink("AudioShare_Ultra") {
            if !to_restore.contains(&id) {
                to_restore.push(id);
            }
        }
        for id in &to_restore {
            let _ = Command::new("pactl")
                .args(["move-sink-input", &id.to_string(), "@DEFAULT_SINK@"])
                .output();
        }
        // Always restore `@DEFAULT_SOURCE@` if it is still pointing at
        // `AudioShare_Ultra.monitor`, even if the isolated app already exited
        // (`to_restore.is_empty()`).
        let current_source = Command::new("pactl")
            .arg("get-default-source")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        if !to_restore.is_empty() || current_source == "AudioShare_Ultra.monitor" {
            if let Ok(out) = Command::new("pactl").arg("get-default-sink").output() {
                let sink = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !sink.is_empty() && sink != "AudioShare_Ultra" {
                    let _ = Command::new("pactl")
                        .args(["set-default-source", &format!("{}.monitor", sink)])
                        .output();
                }
            }
        }
        "Restored: Capturing Full System Audio (All Applications)".into()
    }
    #[cfg(not(target_os = "linux"))]
    {
        #[cfg(target_os = "windows")]
        {
            WINDOWS_TARGET_CAPTURE_PID.store(0, std::sync::atomic::Ordering::Release);
        }
        "Restored: Capturing Full System Audio (All Applications)".into()
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
