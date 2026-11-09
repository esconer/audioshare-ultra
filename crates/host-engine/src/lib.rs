use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use audioshare_protocol::crypto::SessionKey;
#[cfg(not(target_os = "android"))]
use audioshare_protocol::dsp::downmix_to_stereo;
use audioshare_protocol::dsp::BurstDecouplerRing;
use audioshare_protocol::lpc_lossless::{
    encode_fec_subframe, encode_lossless_pcm24_subframe, encode_lossless_subframe,
    encode_vbr_subframe, QualityProfile, ADPCM_FEC_BYTES, MAX_LOSSLESS_PCM24_BYTES,
    MAX_LOSSLESS_SUBFRAME_BYTES,
};
use audioshare_protocol::wire::{
    pack_datagram_with_key, ClientFeedbackPacket, CodecMode, WireHeader, CONTROL_FEEDBACK_MAGIC,
};
use audioshare_protocol::{
    MAX_SAFE_UDP_PAYLOAD, SAMPLE_RATE, SUBFRAME_FRAMES, SUBFRAME_STEREO_SAMPLES,
};
#[cfg(not(target_os = "android"))]
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

pub mod app_routing;
pub mod bt_optimization;
pub mod discovery_and_os;
pub mod virtual_mic;

use discovery_and_os::HostVolumeMonitor;

pub const DEFAULT_PORT: u16 = 65530;
const CONTROL_MAGIC_HELLO: &[u8; 8] = b"ASULTRA1";

#[cfg(not(target_os = "android"))]
pub(crate) struct ClientPreviewSink;
#[cfg(not(target_os = "android"))]
impl ClientPreviewSink {
    #[allow(dead_code)]
    pub fn spawn(fifo: Arc<BurstDecouplerRing>) -> Result<cpal::Stream, String> {
        Self::spawn_preferred(fifo, None)
    }

    /// Spawn playback on the first output device whose name contains
    /// `preferred_name` (e.g. "CABLE Input"), falling back to the default.
    pub fn spawn_preferred(
        fifo: Arc<BurstDecouplerRing>,
        preferred_name: Option<&str>,
    ) -> Result<cpal::Stream, String> {
        let host = cpal::default_host();
        let preferred = preferred_name.and_then(|needle| {
            host.output_devices().ok().and_then(|mut devices| {
                devices.find(|device| {
                    device
                        .name()
                        .map(|name| name.contains(needle))
                        .unwrap_or(false)
                })
            })
        });
        let dev = preferred
            .or_else(|| host.default_output_device())
            .ok_or("No output device")?;
        let cfg = cpal::StreamConfig {
            channels: 2,
            sample_rate: cpal::SampleRate(SAMPLE_RATE),
            buffer_size: cpal::BufferSize::Default,
        };
        let mut rng = 12345u32;
        let stream = dev
            .build_output_stream(
                &cfg,
                move |out: &mut [f32], _| {
                    fifo.pop_exact_or_dither(out, &mut rng);
                },
                |_| {},
                None,
            )
            .map_err(|e| e.to_string())?;
        stream.play().map_err(|e| e.to_string())?;
        Ok(stream)
    }
}

#[derive(Debug, Clone)]
pub struct NetworkInterfaceInfo {
    pub name: String,
    pub ip: IpAddr,
    pub is_usb_tethering: bool,
}

pub fn list_valid_host_interfaces() -> Vec<NetworkInterfaceInfo> {
    let mut valid = Vec::new();
    if let Ok(addrs) = if_addrs::get_if_addrs() {
        for iface in addrs {
            if iface.is_loopback() {
                continue;
            }
            let ip = iface.ip();
            if !ip.is_ipv4() {
                continue;
            }
            let lower = iface.name.to_lowercase();
            let is_virtual = lower.contains("docker")
                || lower.contains("wsl")
                || lower.contains("vethernet")
                || lower.contains("vmware")
                || lower.contains("vmnet")
                || lower.contains("vbox")
                || lower.contains("virbr")
                || lower.contains("br-");
            if is_virtual {
                continue;
            }

            let ip_str = ip.to_string();
            let is_usb = lower.contains("rndis")
                || lower.contains("usb")
                || lower.contains("ncm")
                || ip_str.starts_with("192.168.42.")
                || ip_str.starts_with("192.168.137.");

            valid.push(NetworkInterfaceInfo {
                name: iface.name,
                ip,
                is_usb_tethering: is_usb,
            });
        }
    }
    valid.sort_by_key(|i| !i.is_usb_tethering);
    valid
}

pub fn configure_realtime_udp_socket(socket: &UdpSocket) -> std::io::Result<()> {
    let sock_ref = socket2::SockRef::from(socket);
    // DSCP CS5 (0xA0) -> 802.11e WMM AC_VI (High-Priority Media Queue with full 802.11n/ax MCS & Block-ACK on 2.4GHz!)
    let _ = sock_ref.set_tos(0xA0);
    let _ = sock_ref.set_send_buffer_size(262_144);
    let _ = sock_ref.set_recv_buffer_size(262_144);

    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe {
        use std::os::fd::AsRawFd;
        let prio: libc::c_int = 5; // Maps to 802.11e AC_VI (Video/Audio High-Throughput Queue)
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PRIORITY,
            &prio as *const _ as *const libc::c_void,
            std::mem::size_of_val(&prio) as libc::socklen_t,
        );
    }
    Ok(())
}

pub fn promote_thread_realtime() {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe {
        #[cfg(target_os = "linux")]
        libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE);
        libc::setpriority(libc::PRIO_PROCESS, 0, -19);
        // Hard real-time SCHED_FIFO for the audio TX/RX loops. Requires
        // CAP_SYS_NICE/root; failure is non-fatal and falls back to nice -19.
        let param = libc::sched_param { sched_priority: 50 };
        let _ = libc::sched_setscheduler(0, libc::SCHED_FIFO, &param);

        // Pin the audio/network loop to the upper half of the CPU set on ARM
        // big.LITTLE SoCs (Android and ARM64 Linux), where high-numbered cores
        // are the performance cores. This prevents the real-time thread from
        // being scheduled onto a slow LITTLE core. We deliberately skip x86
        // desktops: on Intel hybrid CPUs the upper-numbered cores can be the
        // E-cores. Any failure (restricted cpuset, SELinux, etc.) is non-fatal.
        #[cfg(any(
            target_os = "android",
            all(target_os = "linux", target_arch = "aarch64")
        ))]
        {
            let num_cpus = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1);
            if num_cpus >= 4 {
                let first_upper = num_cpus / 2;
                let last = num_cpus.min(libc::CPU_SETSIZE as usize);
                if first_upper < last {
                    let mut cpuset: libc::cpu_set_t = core::mem::zeroed();
                    for cpu in first_upper..last {
                        libc::CPU_SET(cpu, &mut cpuset);
                    }
                    let _ = libc::sched_setaffinity(
                        0,
                        core::mem::size_of::<libc::cpu_set_t>(),
                        &cpuset,
                    );
                }
            }
        }
    }

    #[cfg(target_os = "windows")]
    unsafe {
        use windows_sys::Win32::System::Threading::{
            AvSetMmThreadCharacteristicsW, GetCurrentThread, SetThreadPriority,
            THREAD_PRIORITY_TIME_CRITICAL,
        };
        let mut task_idx = 0u32;
        let name: [u16; 10] = [
            b'P' as u16,
            b'r' as u16,
            b'o' as u16,
            b' ' as u16,
            b'A' as u16,
            b'u' as u16,
            b'd' as u16,
            b'i' as u16,
            b'o' as u16,
            0,
        ];
        AvSetMmThreadCharacteristicsW(name.as_ptr(), &mut task_idx);
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
    }
}

#[derive(Debug, Clone)]
pub struct ConnectedClient {
    pub addr: SocketAddr,
    pub last_seen: Instant,
    pub is_usb: bool,
    pub muted: bool,
    pub fec_rate_permille: u16,
    pub bluetooth_delay_ms: u16,
    pub client_volume: u8,
}

#[derive(Debug, Clone, Copy)]
pub struct HostConfig {
    pub bind_port: u16,
    pub subframes_per_packet: u8,
    pub use_test_tone: bool,
    pub enable_smart_dtx: bool,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            bind_port: DEFAULT_PORT,
            subframes_per_packet: 2,
            use_test_tone: false,
            enable_smart_dtx: true,
        }
    }
}

pub struct HostTelemetry {
    pub packets_sent: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub dtx_silent_mode: AtomicBool,
    pub active_codec_mode: AtomicU8,
    pub quality_profile: AtomicU8,
    pub target_bitrate_kbps: AtomicU16,
    pub host_volume: AtomicU8,
    pub host_vu_peak_pct: AtomicU8,
    pub active_clients_count: AtomicU8,
    pub max_client_bt_delay_ms: AtomicU16,
    pub hotplug_rebinds: AtomicU16,
    pub trigger_sonar_ping: AtomicBool,
    pub sonar_tx_timestamp_us: AtomicU64,
}

#[cfg(not(target_os = "android"))]
pub enum HostCaptureStream {
    #[cfg(target_os = "linux")]
    Process {
        stream_active: Arc<AtomicBool>,
        child_pid: Arc<std::sync::atomic::AtomicU32>,
    },
    Cpal(cpal::Stream),
}

#[cfg(not(target_os = "android"))]
impl Drop for HostCaptureStream {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if let HostCaptureStream::Process {
            stream_active,
            child_pid,
        } = self
        {
            stream_active.store(false, Ordering::Release);
            let pid = child_pid.load(Ordering::Relaxed);
            if pid != 0 {
                unsafe {
                    libc::kill(pid as i32, libc::SIGTERM);
                }
            }
        }
    }
}

pub struct HostEngine {
    running: Arc<AtomicBool>,
    pub telemetry: Arc<HostTelemetry>,
    clients: Arc<Mutex<HashMap<SocketAddr, ConnectedClient>>>,
    session_key: Arc<Mutex<Option<SessionKey>>>,
    worker_handle: Option<std::thread::JoinHandle<()>>,
    volume_monitor: Option<HostVolumeMonitor>,
}

impl HostEngine {
    pub fn start(config: HostConfig, initial_targets: Vec<SocketAddr>) -> Result<Self, String> {
        let running = Arc::new(AtomicBool::new(true));
        let telemetry = Arc::new(HostTelemetry {
            packets_sent: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            dtx_silent_mode: AtomicBool::new(false),
            active_codec_mode: AtomicU8::new(CodecMode::StudioLosslessLpc as u8),
            quality_profile: AtomicU8::new(QualityProfile::SmartAuto as u8),
            target_bitrate_kbps: AtomicU16::new(256),
            host_volume: AtomicU8::new(100),
            host_vu_peak_pct: AtomicU8::new(0),
            active_clients_count: AtomicU8::new(initial_targets.len() as u8),
            max_client_bt_delay_ms: AtomicU16::new(0),
            hotplug_rebinds: AtomicU16::new(0),
            trigger_sonar_ping: AtomicBool::new(false),
            sonar_tx_timestamp_us: AtomicU64::new(0),
        });

        let mut client_map = HashMap::new();
        for addr in initial_targets {
            let is_usb = addr.ip().to_string().starts_with("192.168.42.")
                || addr.ip().to_string().starts_with("127.0.0.1");
            client_map.insert(
                addr,
                ConnectedClient {
                    addr,
                    last_seen: Instant::now(),
                    is_usb,
                    muted: false,
                    fec_rate_permille: 0,
                    bluetooth_delay_ms: 0,
                    client_volume: 100,
                },
            );
        }
        let clients = Arc::new(Mutex::new(client_map));
        let session_key = Arc::new(Mutex::new(None));

        // Poll the OS master sink volume/mute state on a dedicated, non-RT
        // thread so physical PC keyboard Vol+/Vol-/Mute keys propagate to the
        // phone without ever blocking the real-time TX loop on `pactl`.
        let volume_monitor = Some(HostVolumeMonitor::start(Arc::clone(&telemetry)));

        let bind_addr = SocketAddr::from(([0, 0, 0, 0], config.bind_port));
        let socket = UdpSocket::bind(bind_addr)
            .map_err(|e| format!("Failed to bind UDP port {}: {}", config.bind_port, e))?;
        configure_realtime_udp_socket(&socket)
            .map_err(|e| format!("Failed to set real-time socket options: {}", e))?;
        let _ = socket.set_nonblocking(true);

        let ring = Arc::new(BurstDecouplerRing::new());
        let last_capture_heartbeat_ms = Arc::new(AtomicU64::new(0));

        let running_clone = Arc::clone(&running);
        let telemetry_clone = Arc::clone(&telemetry);
        let clients_clone = Arc::clone(&clients);
        let key_clone = Arc::clone(&session_key);

        let worker_handle = std::thread::Builder::new()
            .name("as-ultra-host-tx".into())
            .spawn(move || {
                promote_thread_realtime();

                Self::realtime_tx_loop(
                    socket,
                    config,
                    ring,
                    last_capture_heartbeat_ms,
                    running_clone,
                    telemetry_clone,
                    clients_clone,
                    key_clone,
                );
            })
            .map_err(|e| format!("Failed to spawn real-time TX thread: {}", e))?;

        Ok(Self {
            running,
            telemetry,
            clients,
            session_key,
            worker_handle: Some(worker_handle),
            volume_monitor,
        })
    }

    pub fn set_encryption_pin(&self, pin: Option<&str>) {
        let mut guard = self.session_key.lock().unwrap();
        *guard = pin.map(SessionKey::from_pin);
    }

    pub fn connected_clients(&self) -> Vec<ConnectedClient> {
        self.clients.lock().unwrap().values().cloned().collect()
    }

    pub fn set_client_muted(&self, addr: SocketAddr, muted: bool) {
        if let Some(client) = self.clients.lock().unwrap().get_mut(&addr) {
            client.muted = muted;
        }
    }

    pub fn kick_client(&self, addr: SocketAddr) {
        let mut map = self.clients.lock().unwrap();
        map.remove(&addr);
        self.telemetry
            .active_clients_count
            .store(map.len() as u8, Ordering::Relaxed);
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(handle) = self.worker_handle.take() {
            let _ = handle.join();
        }
        // Dropping the monitor stops and joins its polling thread.
        self.volume_monitor = None;
    }

    #[cfg(target_os = "linux")]
    fn spawn_linux_parec_monitor(
        ring: Arc<BurstDecouplerRing>,
        heartbeat_ms: Arc<AtomicU64>,
        running: Arc<AtomicBool>,
        start_epoch: Instant,
    ) -> Result<HostCaptureStream, String> {
        use std::io::Read;
        use std::process::{Command, Stdio};

        let stream_active = Arc::new(AtomicBool::new(true));
        let child_pid = Arc::new(std::sync::atomic::AtomicU32::new(0));

        let stream_active_clone = Arc::clone(&stream_active);
        let child_pid_clone = Arc::clone(&child_pid);

        std::thread::Builder::new()
            .name("as-ultra-linux-parec".into())
            .spawn(move || {
                promote_thread_realtime();
                let mut byte_buf = [0u8; SUBFRAME_STEREO_SAMPLES * 4]; // 960 bytes = 2.5ms stereo f32
                let mut f32_buf = [0.0f32; SUBFRAME_STEREO_SAMPLES];

                let get_default_monitor = || -> String {
                    Command::new("pactl")
                        .arg("get-default-sink")
                        .output()
                        .ok()
                        .and_then(|o| {
                            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                            if s.is_empty() {
                                None
                            } else {
                                Some(format!("{}.monitor", s))
                            }
                        })
                        .unwrap_or_else(|| "@DEFAULT_MONITOR@".to_string())
                };

                while stream_active_clone.load(Ordering::Relaxed) && running.load(Ordering::Relaxed)
                {
                    let current_monitor = get_default_monitor();

                    // 1. Try direct in-process libpulse-simple.so.0 (2.5ms hardware fragsize, zero pipe buffering!)
                    if Self::try_inprocess_pulse_simple_capture(
                        &current_monitor,
                        &ring,
                        &heartbeat_ms,
                        &running,
                        &stream_active_clone,
                        start_epoch,
                        &get_default_monitor,
                    ) {
                        if stream_active_clone.load(Ordering::Relaxed)
                            && running.load(Ordering::Relaxed)
                        {
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        continue;
                    }

                    // 2. Fallback to parec CLI if libpulse-simple.so.0 is unavailable
                    if let Ok(mut child) = Command::new("parec")
                        .env("PIPEWIRE_LATENCY", "64/48000")
                        .args([
                            "--raw",
                            "--format=float32le",
                            "--rate=48000",
                            "--channels=2",
                            "--latency-msec=1",
                            "--process-time-msec=1",
                            &format!("--device={}", current_monitor),
                        ])
                        .stdout(Stdio::piped())
                        .stderr(Stdio::null())
                        .spawn()
                    {
                        let pid = child.id();
                        child_pid_clone.store(pid, Ordering::Relaxed);
                        let run_watch = Arc::clone(&running);
                        let stream_watch = Arc::clone(&stream_active_clone);
                        std::thread::spawn(move || {
                            while run_watch.load(Ordering::Relaxed)
                                && stream_watch.load(Ordering::Relaxed)
                            {
                                std::thread::sleep(Duration::from_millis(100));
                            }
                            unsafe {
                                libc::kill(pid as i32, libc::SIGTERM);
                            }
                        });
                        if let Some(mut stdout) = child.stdout.take() {
                            let mut frames_read = 0u32;
                            while stream_active_clone.load(Ordering::Relaxed)
                                && running.load(Ordering::Relaxed)
                            {
                                if stdout.read_exact(&mut byte_buf).is_ok() {
                                    heartbeat_ms.store(
                                        start_epoch.elapsed().as_millis() as u64,
                                        Ordering::Relaxed,
                                    );
                                    for (i, chunk) in byte_buf.chunks_exact(4).enumerate() {
                                        f32_buf[i] = f32::from_le_bytes([
                                            chunk[0], chunk[1], chunk[2], chunk[3],
                                        ]);
                                    }
                                    ring.push_samples(&f32_buf);

                                    // Every 2 seconds (800 x 2.5ms subframes), check if default sink changed
                                    frames_read += 1;
                                    if frames_read >= 800 {
                                        frames_read = 0;
                                        if get_default_monitor() != current_monitor {
                                            break; // Re-open parec on the newly selected sink monitor!
                                        }
                                    }
                                } else {
                                    break;
                                }
                            }
                        }
                        child_pid_clone.store(0, Ordering::Relaxed);
                        let _ = child.kill();
                        let _ = child.wait();
                    }

                    if stream_active_clone.load(Ordering::Relaxed)
                        && running.load(Ordering::Relaxed)
                    {
                        std::thread::sleep(Duration::from_millis(150));
                    }
                }

                stream_active_clone.store(false, Ordering::Release);
            })
            .map_err(|e| format!("Failed to spawn parec reader thread: {}", e))?;

        Ok(HostCaptureStream::Process {
            stream_active,
            child_pid,
        })
    }

    #[cfg(target_os = "linux")]
    fn try_inprocess_pulse_simple_capture<F>(
        monitor_name: &str,
        ring: &Arc<BurstDecouplerRing>,
        heartbeat_ms: &Arc<AtomicU64>,
        running: &Arc<AtomicBool>,
        stream_active: &Arc<AtomicBool>,
        start_epoch: Instant,
        get_default_monitor: &F,
    ) -> bool
    where
        F: Fn() -> String,
    {
        #[repr(C)]
        struct PaSampleSpec {
            format: i32,
            rate: u32,
            channels: u8,
        }
        #[repr(C)]
        struct PaBufferAttr {
            maxlength: u32,
            tlength: u32,
            prebuf: u32,
            minreq: u32,
            fragsize: u32,
        }
        type FnPaSimpleNew = unsafe extern "C" fn(
            *const i8,
            *const i8,
            i32,
            *const i8,
            *const i8,
            *const PaSampleSpec,
            *const core::ffi::c_void,
            *const PaBufferAttr,
            *mut i32,
        ) -> *mut core::ffi::c_void;
        type FnPaSimpleRead =
            unsafe extern "C" fn(*mut core::ffi::c_void, *mut u8, usize, *mut i32) -> i32;
        type FnPaSimpleFree = unsafe extern "C" fn(*mut core::ffi::c_void);

        unsafe {
            let lib = libc::dlopen(
                b"libpulse-simple.so.0\0".as_ptr() as *const i8,
                libc::RTLD_LAZY,
            );
            if lib.is_null() {
                return false;
            }
            let p_new = libc::dlsym(lib, b"pa_simple_new\0".as_ptr() as *const i8);
            let p_read = libc::dlsym(lib, b"pa_simple_read\0".as_ptr() as *const i8);
            let p_free = libc::dlsym(lib, b"pa_simple_free\0".as_ptr() as *const i8);
            if p_new.is_null() || p_read.is_null() || p_free.is_null() {
                libc::dlclose(lib);
                return false;
            }
            let pa_new: FnPaSimpleNew = core::mem::transmute(p_new);
            let pa_read: FnPaSimpleRead = core::mem::transmute(p_read);
            let pa_free: FnPaSimpleFree = core::mem::transmute(p_free);

            let dev_cstr = if monitor_name.is_empty() || monitor_name == "@DEFAULT_MONITOR@" {
                None
            } else {
                std::ffi::CString::new(monitor_name).ok()
            };
            let dev_ptr = dev_cstr
                .as_ref()
                .map(|c| c.as_ptr())
                .unwrap_or(core::ptr::null());

            let ss = PaSampleSpec {
                format: 5, // PA_SAMPLE_FLOAT32LE = 5
                rate: 48_000,
                channels: 2,
            };
            let attr = PaBufferAttr {
                maxlength: (SUBFRAME_STEREO_SAMPLES * 4 * 2) as u32, // 5.0ms max buffer
                tlength: u32::MAX,
                prebuf: u32::MAX,
                minreq: u32::MAX,
                fragsize: (SUBFRAME_STEREO_SAMPLES * 4) as u32, // Exact 2.5ms (960B) DMA fragment!
            };
            let mut err = 0i32;
            let handle = pa_new(
                core::ptr::null(),
                b"AudioShare Ultra\0".as_ptr() as *const i8,
                2, // PA_STREAM_RECORD = 2
                dev_ptr,
                b"2.5ms Lossless Monitor\0".as_ptr() as *const i8,
                &ss,
                core::ptr::null(),
                &attr,
                &mut err,
            );
            if handle.is_null() {
                libc::dlclose(lib);
                return false;
            }

            let mut f32_buf = [0.0f32; SUBFRAME_STEREO_SAMPLES];
            let mut frames_read = 0u32;
            while stream_active.load(Ordering::Relaxed) && running.load(Ordering::Relaxed) {
                if pa_read(
                    handle,
                    f32_buf.as_mut_ptr() as *mut u8,
                    SUBFRAME_STEREO_SAMPLES * 4,
                    &mut err,
                ) < 0
                {
                    break;
                }
                heartbeat_ms.store(start_epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
                ring.push_samples(&f32_buf);
                frames_read += 1;
                if frames_read >= 800 {
                    frames_read = 0;
                    if get_default_monitor() != monitor_name {
                        break;
                    }
                }
            }
            pa_free(handle);
            libc::dlclose(lib);
            true
        }
    }

    #[cfg(not(target_os = "android"))]
    fn spawn_loopback_capture(
        ring: Arc<BurstDecouplerRing>,
        heartbeat_ms: Arc<AtomicU64>,
        running: Arc<AtomicBool>,
        start_epoch: Instant,
    ) -> Result<HostCaptureStream, String> {
        #[cfg(target_os = "linux")]
        {
            // Spawn native PipeWire / PulseAudio 3ms desktop speaker monitor stream (`@DEFAULT_MONITOR@`)
            if let Ok(stream) = Self::spawn_linux_parec_monitor(
                Arc::clone(&ring),
                Arc::clone(&heartbeat_ms),
                Arc::clone(&running),
                start_epoch,
            ) {
                return Ok(stream);
            }
        }

        let host = cpal::default_host();

        #[cfg(target_os = "windows")]
        let device = host
            .default_output_device()
            .ok_or_else(|| "No Windows audio output endpoint found".to_string())?;

        #[cfg(not(target_os = "windows"))]
        let device = host
            .default_input_device()
            .ok_or_else(|| "No Linux audio capture endpoint found".to_string())?;

        let supported_cfg = if cfg!(target_os = "windows") {
            device
                .default_output_config()
                .map_err(|e| format!("Output config error: {}", e))?
        } else {
            device
                .default_input_config()
                .map_err(|e| format!("Input config error: {}", e))?
        };

        let channels = supported_cfg.channels() as usize;
        let src_rate = supported_cfg.sample_rate().0;
        let sample_format = supported_cfg.sample_format();

        let stream_cfg = cpal::StreamConfig {
            channels: channels as u16,
            sample_rate: cpal::SampleRate(src_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        // Use our 4-Point Hermite Cubic Fractional Interpolator for alias-free 44.1k/96k -> 48k conversion
        let mut host_resampler = audioshare_protocol::dsp::ClockDriftResampler::new();
        host_resampler.ratio = (src_rate as f64) / (SAMPLE_RATE as f64);

        let stream = match sample_format {
            cpal::SampleFormat::I16 => {
                let heartbeat = Arc::clone(&heartbeat_ms);
                let ring_buf = Arc::clone(&ring);
                device.build_input_stream(
                    &stream_cfg,
                    move |data: &[i16], _: &cpal::InputCallbackInfo| {
                        heartbeat
                            .store(start_epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
                        let in_frames = data.len() / channels.max(1);
                        let safe_frames = in_frames.min(1024);
                        let mut stereo_buf = [0.0f32; 2048];
                        let mut f32_tmp = [0.0f32; 2048];
                        let total_samples = (safe_frames * channels).min(2048);
                        for (dst, &s) in f32_tmp[..total_samples]
                            .iter_mut()
                            .zip(data[..total_samples].iter())
                        {
                            *dst = (s as f32) * (1.0 / 32768.0);
                        }

                        downmix_to_stereo(
                            &f32_tmp[..safe_frames * channels],
                            channels,
                            &mut stereo_buf[..safe_frames * 2],
                        );

                        if src_rate == SAMPLE_RATE {
                            ring_buf.push_samples(&stereo_buf[..safe_frames * 2]);
                        } else {
                            let mut resampled = [0.0f32; 4096];
                            let out_frames = host_resampler
                                .process(&stereo_buf[..safe_frames * 2], &mut resampled);
                            ring_buf.push_samples(&resampled[..out_frames * 2]);
                        }
                    },
                    |_| {},
                    None,
                )
            }
            cpal::SampleFormat::U16 => {
                let heartbeat = Arc::clone(&heartbeat_ms);
                let ring_buf = Arc::clone(&ring);
                device.build_input_stream(
                    &stream_cfg,
                    move |data: &[u16], _: &cpal::InputCallbackInfo| {
                        heartbeat
                            .store(start_epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
                        let in_frames = data.len() / channels.max(1);
                        let safe_frames = in_frames.min(1024);
                        let mut stereo_buf = [0.0f32; 2048];
                        let mut f32_tmp = [0.0f32; 2048];
                        let total_samples = (safe_frames * channels).min(2048);
                        for (dst, &s) in f32_tmp[..total_samples]
                            .iter_mut()
                            .zip(data[..total_samples].iter())
                        {
                            *dst = ((s as f32) - 32768.0) * (1.0 / 32768.0);
                        }

                        downmix_to_stereo(
                            &f32_tmp[..safe_frames * channels],
                            channels,
                            &mut stereo_buf[..safe_frames * 2],
                        );

                        if src_rate == SAMPLE_RATE {
                            ring_buf.push_samples(&stereo_buf[..safe_frames * 2]);
                        } else {
                            let mut resampled = [0.0f32; 4096];
                            let out_frames = host_resampler
                                .process(&stereo_buf[..safe_frames * 2], &mut resampled);
                            ring_buf.push_samples(&resampled[..out_frames * 2]);
                        }
                    },
                    |_| {},
                    None,
                )
            }
            _ => {
                let heartbeat = Arc::clone(&heartbeat_ms);
                let ring_buf = Arc::clone(&ring);
                device.build_input_stream(
                    &stream_cfg,
                    move |data: &[f32], _: &cpal::InputCallbackInfo| {
                        heartbeat
                            .store(start_epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
                        let in_frames = data.len() / channels.max(1);
                        let mut stereo_buf = [0.0f32; 2048];
                        let safe_frames = in_frames.min(1024);

                        downmix_to_stereo(
                            &data[..safe_frames * channels],
                            channels,
                            &mut stereo_buf[..safe_frames * 2],
                        );

                        if src_rate == SAMPLE_RATE {
                            ring_buf.push_samples(&stereo_buf[..safe_frames * 2]);
                        } else {
                            let mut resampled = [0.0f32; 4096];
                            let out_frames = host_resampler
                                .process(&stereo_buf[..safe_frames * 2], &mut resampled);
                            ring_buf.push_samples(&resampled[..out_frames * 2]);
                        }
                    },
                    |_| {},
                    None,
                )
            }
        }
        .map_err(|e| format!("Failed to build loopback stream: {}", e))?;

        stream
            .play()
            .map_err(|e| format!("Failed to start loopback stream: {}", e))?;
        Ok(HostCaptureStream::Cpal(stream))
    }

    #[allow(clippy::too_many_arguments)]
    fn realtime_tx_loop(
        socket: UdpSocket,
        config: HostConfig,
        ring: Arc<BurstDecouplerRing>,
        capture_heartbeat_ms: Arc<AtomicU64>,
        running: Arc<AtomicBool>,
        telemetry: Arc<HostTelemetry>,
        clients: Arc<Mutex<HashMap<SocketAddr, ConnectedClient>>>,
        session_key: Arc<Mutex<Option<SessionKey>>>,
    ) {
        #[cfg(target_os = "android")]
        let _ = &capture_heartbeat_ms;
        let start_instant = Instant::now();
        #[cfg(target_os = "windows")]
        let mut active_stream = if !config.use_test_tone {
            Self::spawn_loopback_capture(
                Arc::clone(&ring),
                Arc::clone(&capture_heartbeat_ms),
                Arc::clone(&running),
                start_instant,
            )
            .ok()
        } else {
            None
        };
        #[cfg(target_os = "windows")]
        let mut last_watchdog_check = Instant::now();

        #[cfg(target_os = "linux")]
        let _active_stream = if !config.use_test_tone {
            Self::spawn_loopback_capture(
                Arc::clone(&ring),
                Arc::clone(&capture_heartbeat_ms),
                Arc::clone(&running),
                start_instant,
            )
            .ok()
        } else {
            None
        };

        let mut subframes_per_pkt = config.subframes_per_packet.clamp(1, 4) as usize;
        let mut packet_interval = Duration::from_micros((2500 * subframes_per_pkt) as u64);
        let mut next_tick = start_instant + packet_interval;

        let mut seq: u16 = 0;
        let mut fec_n1 = [0u8; ADPCM_FEC_BYTES];
        let mut fec_n2 = [0u8; ADPCM_FEC_BYTES];
        let mut has_fec_n1 = false;
        let mut has_fec_n2 = false;

        let mut silent_subframes: u32 = 0;
        let mut tone_phase: f32 = 0.0;
        let mut ctrl_buf = [0u8; 64];
        let mut dynamic_codec_mode;
        let mut tx_bitrate_kbps;
        let mut worst_loss_permille: u16 = 0;
        let mut noise_shaper = audioshare_protocol::sync_and_eq::NoiseShapedQuantizer::new();
        let mut cached_targets: Vec<SocketAddr> = Vec::with_capacity(8);
        if let Ok(map) = clients.lock() {
            for c in map.values() {
                if !c.muted {
                    cached_targets.push(c.addr);
                }
            }
        }
        let mut last_client_sweep = Instant::now();

        while running.load(Ordering::Relaxed) {
            // 1. Hot-Plug Audio Device Watchdog (Solves Issues #173, #122, #103)
            #[cfg(target_os = "windows")]
            if !config.use_test_tone && last_watchdog_check.elapsed() >= Duration::from_millis(500)
            {
                last_watchdog_check = Instant::now();
                let now_ms = start_instant.elapsed().as_millis() as u64;
                let last_cb = capture_heartbeat_ms.load(Ordering::Relaxed);
                if active_stream.is_none() || now_ms.saturating_sub(last_cb) > 2500 {
                    // Audio endpoint changed (HDMI monitor sleep / USB headset unplugged) -> Seamless Rebind!
                    active_stream = Self::spawn_loopback_capture(
                        Arc::clone(&ring),
                        Arc::clone(&capture_heartbeat_ms),
                        Arc::clone(&running),
                        start_instant,
                    )
                    .ok();
                    if active_stream.is_some() {
                        capture_heartbeat_ms.store(now_ms, Ordering::Relaxed);
                        telemetry.hotplug_rebinds.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }

            // 2. Process incoming Client HELLO or ClientFeedbackPacket (Dynamic Link Governor & BT Sync)
            while let Ok((len, src_addr)) = socket.recv_from(&mut ctrl_buf) {
                let is_usb = src_addr.ip().to_string().starts_with("192.168.42.")
                    || src_addr.ip().to_string().starts_with("127.0.0.1");

                if len >= 8 && &ctrl_buf[..8] == CONTROL_MAGIC_HELLO {
                    let mut map = clients.lock().unwrap();
                    map.entry(src_addr)
                        .or_insert(ConnectedClient {
                            addr: src_addr,
                            last_seen: Instant::now(),
                            is_usb,
                            muted: false,
                            fec_rate_permille: 0,
                            bluetooth_delay_ms: 0,
                            client_volume: 100,
                        })
                        .last_seen = Instant::now();
                    telemetry
                        .active_clients_count
                        .store(map.len() as u8, Ordering::Relaxed);
                    cached_targets.clear();
                    for c in map.values() {
                        if !c.muted {
                            cached_targets.push(c.addr);
                        }
                    }
                } else if len >= 8 && &ctrl_buf[..8] == CONTROL_FEEDBACK_MAGIC {
                    if let Some(fb) = ClientFeedbackPacket::decode(&ctrl_buf[..len]) {
                        // Execute remote Bluetooth earbud media command (Play/Pause/Next/Prev) if present
                        if fb.requested_mode >= 10 {
                            let cmd = match fb.requested_mode {
                                10 => bt_optimization::RemoteMediaCommand::PlayPause,
                                11 => bt_optimization::RemoteMediaCommand::NextTrack,
                                12 => bt_optimization::RemoteMediaCommand::PrevTrack,
                                _ => bt_optimization::RemoteMediaCommand::None,
                            };
                            bt_optimization::execute_remote_media_command(cmd);
                        }

                        // Remote quality-mode selection from the phone UI:
                        // 1=Smart Auto, 2=Studio Lossless, 3=Low-Latency Compressed,
                        // 4=Max Stability (10 ms 2.4 GHz), 5=Studio Lossless 24-bit.
                        // Values >= 10 are Bluetooth media transport commands
                        // handled above.
                        if (1..=5).contains(&fb.requested_mode) {
                            telemetry
                                .quality_profile
                                .store(fb.requested_mode - 1, Ordering::Relaxed);
                        }

                        let mut map = clients.lock().unwrap();
                        let entry = map.entry(src_addr).or_insert(ConnectedClient {
                            addr: src_addr,
                            last_seen: Instant::now(),
                            is_usb,
                            muted: false,
                            fec_rate_permille: fb.fec_rate_permille,
                            bluetooth_delay_ms: fb.bluetooth_delay_ms,
                            client_volume: fb.client_volume,
                        });
                        entry.last_seen = Instant::now();
                        entry.fec_rate_permille = fb.fec_rate_permille;
                        entry.bluetooth_delay_ms = fb.bluetooth_delay_ms;
                        entry.client_volume = fb.client_volume;
                        telemetry
                            .active_clients_count
                            .store(map.len() as u8, Ordering::Relaxed);

                        worst_loss_permille =
                            map.values().map(|c| c.fec_rate_permille).max().unwrap_or(0);
                        let max_bt = map
                            .values()
                            .map(|c| c.bluetooth_delay_ms)
                            .max()
                            .unwrap_or(0);
                        telemetry
                            .max_client_bt_delay_ms
                            .store(max_bt, Ordering::Relaxed);

                        cached_targets.clear();
                        for c in map.values() {
                            if !c.muted {
                                cached_targets.push(c.addr);
                            }
                        }
                    }
                }
            }

            // 2b. Expire clients that stopped sending HELLO/feedback, and refresh lock-free cached_targets
            if last_client_sweep.elapsed() >= Duration::from_secs(1) {
                last_client_sweep = Instant::now();
                if let Ok(mut map) = clients.try_lock() {
                    let before = map.len();
                    map.retain(|_, client| client.last_seen.elapsed() < Duration::from_secs(10));
                    if map.len() != before {
                        telemetry
                            .active_clients_count
                            .store(map.len() as u8, Ordering::Relaxed);
                    }
                    worst_loss_permille =
                        map.values().map(|c| c.fec_rate_permille).max().unwrap_or(0);
                    cached_targets.clear();
                    for c in map.values() {
                        if !c.muted {
                            cached_targets.push(c.addr);
                        }
                    }
                }
            }

            // 2c. Evaluate the user-selected quality profile (re-evaluated every
            // packet so a UI change takes effect immediately, and so the
            // Smart Auto governor can adapt framing without restarting TX).
            let profile =
                QualityProfile::from_u8(telemetry.quality_profile.load(Ordering::Relaxed));
            let selected_kbps = telemetry
                .target_bitrate_kbps
                .load(Ordering::Relaxed)
                .clamp(96, 320);
            match profile {
                QualityProfile::StudioLossless => {
                    dynamic_codec_mode = CodecMode::StudioLosslessLpc;
                    subframes_per_pkt = config.subframes_per_packet.clamp(1, 2) as usize;
                    tx_bitrate_kbps = selected_kbps;
                }
                QualityProfile::LowLatencyCompressed => {
                    dynamic_codec_mode = CodecMode::CompressedLowDelay;
                    subframes_per_pkt = config.subframes_per_packet.clamp(1, 2) as usize;
                    tx_bitrate_kbps = selected_kbps;
                }
                QualityProfile::MaxStability24Ghz => {
                    // 10.0 ms framing (4 x 2.5 ms = 100 packets/sec) with the
                    // 160 kbps rung: long-range 2.4 GHz through walls.
                    dynamic_codec_mode = CodecMode::CompressedLowDelay;
                    subframes_per_pkt = 4;
                    tx_bitrate_kbps = 160;
                }
                QualityProfile::StudioLossless24Bit => {
                    // A worst-case 24-bit subframe is 722 bytes. Keeping one
                    // subframe per packet leaves ample room for both 124-byte
                    // ADPCM FEC backups inside the 1380-byte safe UDP MTU.
                    dynamic_codec_mode = CodecMode::StudioLosslessLpc;
                    subframes_per_pkt = 1;
                    tx_bitrate_kbps = selected_kbps;
                }
                QualityProfile::SmartAuto => {
                    if worst_loss_permille > 120 {
                        dynamic_codec_mode = CodecMode::CompressedLowDelay;
                        subframes_per_pkt = 4;
                        tx_bitrate_kbps = 160;
                    } else if worst_loss_permille > 45 {
                        dynamic_codec_mode = CodecMode::CompressedLowDelay;
                        subframes_per_pkt = 2;
                        tx_bitrate_kbps = selected_kbps;
                    } else {
                        dynamic_codec_mode = CodecMode::StudioLosslessLpc;
                        subframes_per_pkt = config.subframes_per_packet.clamp(1, 2) as usize;
                        tx_bitrate_kbps = selected_kbps;
                    }
                }
            }
            packet_interval = Duration::from_micros((2500 * subframes_per_pkt) as u64);
            telemetry
                .active_codec_mode
                .store(dynamic_codec_mode as u8, Ordering::Relaxed);

            // Wait until soundcard pushes a full packet of samples into `ring`,
            // or fall back to silence synthesis if capture endpoint is idle >15ms
            let needed_samples = subframes_per_pkt * SUBFRAME_STEREO_SAMPLES;
            if !config.use_test_tone && ring.available_samples() < needed_samples {
                let now_ms = start_instant.elapsed().as_millis() as u64;
                let last_cb = capture_heartbeat_ms.load(Ordering::Relaxed);
                if now_ms.saturating_sub(last_cb) <= 15 {
                    std::thread::sleep(Duration::from_micros(400));
                    continue;
                }
            }

            // 3. Encode Subframes
            let mut primary_bundle = [0u8; MAX_LOSSLESS_PCM24_BYTES * 4 + 12];
            let mut primary_cursor = 0usize;
            let base_seq = seq;
            let prev_n1_copy = fec_n1;
            let prev_n2_copy = fec_n2;
            let send_n1 = has_fec_n1;
            let send_n2 = has_fec_n2;
            let mut packet_is_silent = true;

            for _ in 0..subframes_per_pkt {
                let mut f32_subframe = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                let mut dummy_rng = 1u32;

                if config.use_test_tone {
                    for i in 0..SUBFRAME_FRAMES {
                        let l = (tone_phase * 2.0 * std::f32::consts::PI * 440.0).sin() * 0.25;
                        let r = (tone_phase * 2.0 * std::f32::consts::PI * 554.37).sin() * 0.25;
                        f32_subframe[i * 2] = l;
                        f32_subframe[i * 2 + 1] = r;
                        tone_phase += 1.0 / (SAMPLE_RATE as f32);
                        if tone_phase >= 1.0 {
                            tone_phase -= 1.0;
                        }
                    }
                } else if !ring.pop_exact_or_dither(&mut f32_subframe, &mut dummy_rng) {
                    f32_subframe.fill(0.0);
                }

                // Inject 2.5ms Acoustic Latency Sonar Chirp if requested by user
                if telemetry.trigger_sonar_ping.swap(false, Ordering::Relaxed) {
                    audioshare_protocol::sync_and_eq::LatencySonar::inject_chirp(&mut f32_subframe);
                    let now_us = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_micros() as u64;
                    telemetry
                        .sonar_tx_timestamp_us
                        .store(now_us, Ordering::Relaxed);
                }

                let mut i16_subframe = [0i16; SUBFRAME_STEREO_SAMPLES];
                let max_abs =
                    noise_shaper.quantize_stereo_subframe(&f32_subframe, &mut i16_subframe);
                let vu_pct = (((max_abs as f32) / 32767.0).sqrt() * 100.0).min(100.0) as u8;
                telemetry.host_vu_peak_pct.store(vu_pct, Ordering::Relaxed);

                if max_abs > 3 {
                    packet_is_silent = false;
                    silent_subframes = 0;
                } else {
                    silent_subframes = silent_subframes.saturating_add(1);
                }

                fec_n2 = fec_n1;
                has_fec_n2 = has_fec_n1;
                encode_fec_subframe(&i16_subframe, &mut fec_n1);
                has_fec_n1 = true;

                if dynamic_codec_mode == CodecMode::CompressedLowDelay {
                    // VBR primary payload targeted by the selected quality mode.
                    // A 2.5 ms frame is only 32-94 bytes (vs 124-byte ADPCM FEC),
                    // and the same payload is also valid as its own FEC backup.
                    let mut enc_vbr = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
                    let enc_len = encode_vbr_subframe(&i16_subframe, tx_bitrate_kbps, &mut enc_vbr);
                    let len_be = (enc_len as u16).to_be_bytes();
                    primary_bundle[primary_cursor] = len_be[0];
                    primary_bundle[primary_cursor + 1] = len_be[1];
                    primary_cursor += 2;
                    primary_bundle[primary_cursor..primary_cursor + enc_len]
                        .copy_from_slice(&enc_vbr[..enc_len]);
                    primary_cursor += enc_len;
                } else if profile == QualityProfile::StudioLossless24Bit {
                    // Encode from the original f32 samples (not the i16
                    // noise-shaper output) so the lower 8 bits carry genuine
                    // 24-bit resolution when the transport provides it.
                    let mut enc_24 = [0u8; MAX_LOSSLESS_PCM24_BYTES];
                    let enc_len = encode_lossless_pcm24_subframe(&f32_subframe, &mut enc_24);
                    let len_be = (enc_len as u16).to_be_bytes();
                    primary_bundle[primary_cursor] = len_be[0];
                    primary_bundle[primary_cursor + 1] = len_be[1];
                    primary_cursor += 2;
                    primary_bundle[primary_cursor..primary_cursor + enc_len]
                        .copy_from_slice(&enc_24[..enc_len]);
                    primary_cursor += enc_len;
                } else {
                    let mut enc_sub = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
                    let enc_len = encode_lossless_subframe(&i16_subframe, &mut enc_sub);
                    let len_be = (enc_len as u16).to_be_bytes();
                    primary_bundle[primary_cursor] = len_be[0];
                    primary_bundle[primary_cursor + 1] = len_be[1];
                    primary_cursor += 2;
                    primary_bundle[primary_cursor..primary_cursor + enc_len]
                        .copy_from_slice(&enc_sub[..enc_len]);
                    primary_cursor += enc_len;
                }

                seq = seq.wrapping_add(1);
            }

            let tx_timestamp_us = start_instant.elapsed().as_micros() as u32;
            let mut datagram = [0u8; MAX_SAFE_UDP_PAYLOAD];
            let active_key = *session_key.lock().unwrap();

            let is_dtx_active =
                config.enable_smart_dtx && packet_is_silent && silent_subframes > 200;
            telemetry
                .dtx_silent_mode
                .store(is_dtx_active, Ordering::Relaxed);

            let maybe_pkt_len = if is_dtx_active {
                if (base_seq % 80) == 0 {
                    let hdr = WireHeader {
                        mode: CodecMode::SilenceHeartbeat,
                        subframe_count: subframes_per_pkt as u8,
                        encrypted: active_key.is_some(),
                        sequence: base_seq,
                        tx_timestamp_us,
                        primary_len: 0,
                        fec_n1_len: 0,
                        fec_n2_len: 0,
                        host_volume: telemetry.host_volume.load(Ordering::Relaxed),
                        // Broadcast max group delay (in 2ms units) so fast phones align with slower phones!
                        stream_id: (telemetry.max_client_bt_delay_ms.load(Ordering::Relaxed) / 2)
                            .min(255) as u8,
                        header_crc16: 0,
                    };
                    pack_datagram_with_key(hdr, &[], &[], &[], active_key.as_ref(), &mut datagram)
                        .ok()
                } else {
                    None
                }
            } else {
                let hdr = WireHeader {
                    mode: dynamic_codec_mode,
                    subframe_count: subframes_per_pkt as u8,
                    encrypted: active_key.is_some(),
                    sequence: base_seq,
                    tx_timestamp_us,
                    primary_len: 0,
                    fec_n1_len: 0,
                    fec_n2_len: 0,
                    host_volume: telemetry.host_volume.load(Ordering::Relaxed),
                    // Broadcast max group delay (in 2ms units) so fast phones align with slower phones!
                    stream_id: (telemetry.max_client_bt_delay_ms.load(Ordering::Relaxed) / 2)
                        .min(255) as u8,
                    header_crc16: 0,
                };
                let f1_slice = if send_n1 { &prev_n1_copy[..] } else { &[] };
                let f2_slice = if send_n2 { &prev_n2_copy[..] } else { &[] };
                pack_datagram_with_key(
                    hdr,
                    &primary_bundle[..primary_cursor],
                    f1_slice,
                    f2_slice,
                    active_key.as_ref(),
                    &mut datagram,
                )
                .ok()
            };

            if let Some(pkt_len) = maybe_pkt_len {
                for &target_addr in &cached_targets {
                    if socket.send_to(&datagram[..pkt_len], target_addr).is_ok() {
                        telemetry.packets_sent.fetch_add(1, Ordering::Relaxed);
                        telemetry
                            .bytes_sent
                            .fetch_add(pkt_len as u64, Ordering::Relaxed);
                    }
                }
            }

            if config.use_test_tone || ring.available_samples() < needed_samples {
                let now = Instant::now();
                if next_tick > now {
                    std::thread::sleep(next_tick - now);
                }
                next_tick = Instant::now() + packet_interval;
            }
        }
    }
}

impl Drop for HostEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_i16_to_f32_conversion_and_downmix() {
        let raw_i16: [i16; 4] = [32767, -32768, 0, 16384];
        let mut f32_tmp = [0.0f32; 4];
        for (dst, &s) in f32_tmp.iter_mut().zip(raw_i16.iter()) {
            *dst = (s as f32) * (1.0 / 32768.0);
        }
        assert!((f32_tmp[0] - 0.9999695).abs() < 1e-4);
        assert!((f32_tmp[1] - (-1.0)).abs() < 1e-6);
        assert_eq!(f32_tmp[2], 0.0);
        assert!((f32_tmp[3] - 0.5).abs() < 1e-4);

        let mut stereo_buf = [0.0f32; 4];
        downmix_to_stereo(&f32_tmp, 2, &mut stereo_buf);
        assert_eq!(stereo_buf[0], f32_tmp[0]);
        assert_eq!(stereo_buf[1], f32_tmp[1]);
    }

    #[test]
    fn test_host_engine_start_stop() {
        let config = HostConfig {
            bind_port: 0, // OS assigned ephemeral port for test
            use_test_tone: true,
            subframes_per_packet: 1,
            enable_smart_dtx: false,
        };
        let dummy_target: SocketAddr = "127.0.0.1:65530".parse().unwrap();
        let engine = HostEngine::start(config, vec![dummy_target]);
        assert!(engine.is_ok());
        let mut engine = engine.unwrap();
        let clients = engine.connected_clients();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].addr, dummy_target);
        assert_eq!(
            engine
                .telemetry
                .active_clients_count
                .load(Ordering::Relaxed),
            1
        );
        engine.stop();
    }
}
