use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};
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
use audioshare_protocol::opus_codec::{OpusStreamEncoder, MAX_OPUS_SUBFRAME_BYTES};
use audioshare_protocol::wire::{
    pack_datagram_with_key, ClientFeedbackPacket, CodecMode, WireHeader, CONTROL_FEEDBACK_MAGIC,
    HEADER_LEN, MAC_TAG_LEN,
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

/// Smart Auto selects compressed audio above this reported loss.
const SMART_AUTO_COMPRESS_THRESHOLD_PERMILLE: u16 = 45;
/// Above this loss Smart Auto also drops to the fixed 160 kbps profile.
const SMART_AUTO_HIGH_LOSS_THRESHOLD_PERMILLE: u16 = 120;
/// Above this loss even the 2.4 GHz stability framing steps down to the
/// 96 kbps rung. 200 permille = 20% packet loss.
const CRITICAL_LOSS_THRESHOLD_PERMILLE: u16 = 200;
/// Keep compressed mode for this long after the last high-loss report. Without
/// a hold-down, a link alternating between 5% and 0% loss recreated the Opus
/// encoder every 500 ms.
const SMART_AUTO_LOSS_HOLD: Duration = Duration::from_secs(3);
/// Release pace after the hold expires. The estimate steps down by this much
/// at most once per interval, so a 130 permille report walks
/// Tier 2 -> Tier 1 -> Tier 0 instead of jumping back to lossless in one
/// feedback tick and immediately re-congesting the link.
const SMART_AUTO_RELEASE_STEP_PERMILLE: u16 = 25;
const SMART_AUTO_RELEASE_INTERVAL: Duration = Duration::from_millis(500);

/// Fast-attack / paced-release governor for Smart Auto.
///
/// A new loss report above the compression threshold immediately wins and
/// re-arms the hold window. While the window is open the highest reported
/// value is kept, so a brief good window cannot step back up to lossless.
/// After the hold expires the estimate decays by
/// [`SMART_AUTO_RELEASE_STEP_PERMILLE`] no more than once per
/// [`SMART_AUTO_RELEASE_INTERVAL`]. The wall-clock gate matters because every
/// connected client sends its own feedback packet: a per-call subtraction
/// would make a multi-client session release several times faster than a
/// single-client one.
struct LossGovernor {
    value: u16,
    hold_until: Instant,
    last_step: Instant,
}

impl LossGovernor {
    fn new(now: Instant) -> Self {
        Self {
            value: 0,
            hold_until: now,
            last_step: now,
        }
    }

    fn update(&mut self, raw: u16, now: Instant) -> u16 {
        if raw > SMART_AUTO_COMPRESS_THRESHOLD_PERMILLE {
            self.hold_until = now + SMART_AUTO_LOSS_HOLD;
            self.last_step = now;
            self.value = self.value.max(raw);
            return self.value;
        }
        if now < self.hold_until {
            self.value = self.value.max(raw);
            return self.value;
        }
        // Below the compression threshold the exact value no longer changes
        // the tier, so snap to the fresh report instead of stretching a
        // harmless 4% blip across the release interval.
        if self.value <= SMART_AUTO_COMPRESS_THRESHOLD_PERMILLE {
            self.value = raw;
            return self.value;
        }
        if now.saturating_duration_since(self.last_step) >= SMART_AUTO_RELEASE_INTERVAL {
            self.value = self
                .value
                .saturating_sub(SMART_AUTO_RELEASE_STEP_PERMILLE)
                .max(raw);
            self.last_step = now;
        } else {
            self.value = self.value.max(raw);
        }
        self.value
    }
}

/// Advance a warm Opus encoder with one live subframe while Smart Auto is in
/// its lossless tier. The encoded output is intentionally discarded: this
/// only keeps the encoder's 2.5 ms lookahead primed so the first compressed
/// packet carries the previous live subframe instead of codec warm-up silence.
#[inline]
fn warm_opus_lookahead(
    encoder: &mut Option<OpusStreamEncoder>,
    pcm_stereo: &[i16; SUBFRAME_STEREO_SAMPLES],
    kbps: u16,
) {
    if let Some(encoder) = encoder.as_mut() {
        let mut discard = [0u8; MAX_OPUS_SUBFRAME_BYTES];
        let _ = encoder.encode_subframe(pcm_stereo, kbps, &mut discard);
    }
}

/// Primary bitrate for the 2.4 GHz stability framing, used by both the
/// explicit `MaxStability24Ghz` profile and Smart Auto's high-loss tier.
///
/// The profile exists for long-range links through walls, so the primary
/// stream is capped at 160 kbps even when the user selected 256/320. A
/// user-selected 96 kbps is honored rather than silently raised to 160.
/// Above 20% loss the 96 kbps rung is the better tradeoff: its primary
/// payload is 36 B/subframe instead of 53 B, leaving more airtime for the
/// FEC backups that actually repair the loss.
#[inline]
fn stability_bitrate_kbps(selected_kbps: u16, worst_loss_permille: u16) -> u16 {
    if worst_loss_permille > CRITICAL_LOSS_THRESHOLD_PERMILLE {
        96
    } else {
        selected_kbps.clamp(96, 160)
    }
}

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

/// True for kernel gadget names (`usb0`, `rndis0`, `ncm0`), Windows-style
/// descriptions, and systemd predictable names for USB tethering NICs:
/// `enx<mac>` (CDC-ECM) and `enp<bus>s<slot>f<func>u<port>` (RNDIS/NCM).
fn is_usb_interface_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("rndis")
        || lower.contains("usb")
        || lower.contains("ncm")
        || lower.starts_with("enx")
        || (lower.starts_with("enp") && lower.contains('u'))
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
            let is_usb = is_usb_interface_name(&iface.name)
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

/// Returns `true` for loopback, the legacy Android RNDIS subnets, or any IPv4
/// address in the same `/24` as a local USB-tethering interface.
///
/// Android 11+ randomizes the RNDIS subnet for each tethering session, so a
/// fixed `192.168.42.x` / `192.168.137.x` prefix check is not sufficient. The
/// local interface list is queried on demand because USB tethering can be
/// enabled after the host engine starts.
pub fn is_usb_subnet_ip(addr_ip: IpAddr) -> bool {
    if addr_ip.is_loopback() {
        return true;
    }
    let IpAddr::V4(target) = addr_ip else {
        return false;
    };
    let t = target.octets();
    if t[0] == 192 && t[1] == 168 && (t[2] == 42 || t[2] == 137) {
        return true;
    }

    list_valid_host_interfaces().into_iter().any(|iface| {
        if !iface.is_usb_tethering {
            return false;
        }
        match iface.ip {
            IpAddr::V4(local) => {
                let l = local.octets();
                t[0] == l[0] && t[1] == l[1] && t[2] == l[2]
            }
            IpAddr::V6(_) => false,
        }
    })
}

pub fn configure_realtime_udp_socket(socket: &UdpSocket) -> std::io::Result<()> {
    let sock_ref = socket2::SockRef::from(socket);
    // DSCP CS5 (0xA0) -> 802.11e WMM AC_VI (High-Priority Media Queue with full 802.11n/ax MCS & Block-ACK on 2.4GHz!)
    let _ = sock_ref.set_tos(0xA0);
    // Asymmetric buffers: Tight 16KB SO_SNDBUF prevents kernel TX bufferbloat on 2.4GHz stalls;
    // 128KB SO_RCVBUF ensures receiver never drops incoming Wi-Fi aggregation bursts.
    let _ = sock_ref.set_send_buffer_size(16_384);
    let _ = sock_ref.set_recv_buffer_size(131_072);

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
        // Deliberately no process-wide `mlockall(MCL_CURRENT | MCL_FUTURE)`.
        // This function runs in the same process as the desktop GUI and GPU
        // driver, so MCL_FUTURE pins every later wgpu/egui allocation and can
        // fail with ENOMEM against the default RLIMIT_MEMLOCK. It also makes
        // every helper `fork()` (pactl/adb/playerctl) duplicate a much larger
        // set of locked VMAs. SCHED_FIFO + nice -19 + CPU affinity provide the
        // scheduling guarantees without those process-wide side effects.
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
        #[link(name = "winmm")]
        extern "system" {
            fn timeBeginPeriod(u_period: u32) -> u32;
        }
        // Force 1.0ms kernel timer resolution on Windows (default is 15.625ms!)
        timeBeginPeriod(1);

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

/// Smooth pacing gap for capture-fed packets: 85% of the nominal 2.5 ms
/// per-subframe interval. OS callbacks often deliver 10-20 ms of audio at
/// once; spacing packets slightly faster than real time spreads the burst
/// across the callback period while keeping the capture clock in charge.
const CAPTURE_SMOOTH_GAP_PER_SUBFRAME_US: u64 = 2_125;

/// Faster catch-up pacing when the capture ring holds more than 20 ms (8
/// subframes). The added drain rate prevents a callback backlog from becoming
/// permanent latency without dumping the whole burst in under 2 ms.
const CAPTURE_CATCHUP_GAP_PER_SUBFRAME_US: u64 = 1_100;
const CAPTURE_CATCHUP_BACKLOG_SUBFRAMES: usize = 8;

#[inline]
fn capture_min_gap_us(subframes_per_pkt: usize, remaining_subframes: usize) -> u64 {
    let count = subframes_per_pkt.max(1) as u64;
    if remaining_subframes > CAPTURE_CATCHUP_BACKLOG_SUBFRAMES {
        count * CAPTURE_CATCHUP_GAP_PER_SUBFRAME_US
    } else {
        count * CAPTURE_SMOOTH_GAP_PER_SUBFRAME_US
    }
}

/// Datagram layout decision for one encoded packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DatagramPlan {
    /// The primary payload fits; the flags say which FEC frames fit with it.
    Single { n1: bool, n2: bool },
    /// Even the primary alone exceeds the safe MTU: send one subframe per
    /// datagram instead of dropping audio.
    SplitPerSubframe,
}

/// True when `primary_len` plus the exact FEC payload bytes and the wire
/// overhead fit inside the safe UDP payload budget.
#[inline]
fn datagram_fits(primary_len: usize, encrypted: bool, fec_bytes: usize) -> bool {
    let overhead = HEADER_LEN + if encrypted { MAC_TAG_LEN } else { 0 } + fec_bytes;
    overhead + primary_len <= MAX_SAFE_UDP_PAYLOAD
}

/// Choose how to fit a packet into one datagram using the exact FEC payload
/// sizes.
///
/// Normal framing carries one 124-byte ADPCM frame per field, while 10 ms
/// 2.4 GHz framing carries a 248-byte pair per field. N-2 is dropped first
/// because it is older and less useful than N-1. If N-1 cannot fit either,
/// the bundle is split into one-subframe datagrams whenever FEC was requested:
/// a single 2.5 ms subframe is small enough that N-1 (and usually N-2) fits
/// even for true 24-bit PCM. Silently keeping the multi-subframe packet and
/// stripping all protection was the old behaviour, which left 24-bit Wi-Fi
/// streams with no loss recovery at all.
#[inline]
fn plan_datagram_with_fec(
    primary_len: usize,
    encrypted: bool,
    n1_len: usize,
    n2_len: usize,
) -> DatagramPlan {
    let want_n1 = n1_len > 0;
    let want_n2 = n2_len > 0;
    let first_fec = if want_n1 { n1_len } else { n2_len };
    if !datagram_fits(primary_len, encrypted, first_fec) {
        return DatagramPlan::SplitPerSubframe;
    }
    if want_n1 && want_n2 && datagram_fits(primary_len, encrypted, n1_len + n2_len) {
        DatagramPlan::Single { n1: true, n2: true }
    } else if want_n1 && datagram_fits(primary_len, encrypted, n1_len) {
        DatagramPlan::Single {
            n1: true,
            n2: false,
        }
    } else if want_n2 && datagram_fits(primary_len, encrypted, n2_len) {
        // Defensive: no current caller sends N-2 without N-1, but a planner
        // should still carry the frame it was given instead of silently
        // dropping protection.
        DatagramPlan::Single {
            n1: false,
            n2: true,
        }
    } else {
        DatagramPlan::Single {
            n1: false,
            n2: false,
        }
    }
}

/// Boolean convenience wrapper for callers that only track FEC presence.
#[inline]
fn plan_datagram(
    primary_len: usize,
    encrypted: bool,
    want_n1: bool,
    want_n2: bool,
) -> DatagramPlan {
    plan_datagram_with_fec(
        primary_len,
        encrypted,
        if want_n1 { ADPCM_FEC_BYTES } else { 0 },
        if want_n2 { ADPCM_FEC_BYTES } else { 0 },
    )
}

/// Build the two FEC fields for one outgoing packet from the sliding history.
///
/// `hist[0]` is N-1 (newest). Normal framing carries one frame per field.
/// Four-subframe 10 ms framing interleaves two frames per field:
/// `fec_n1 = [N-1, N-3]`, `fec_n2 = [N-2, N-4]`. Keeping N-1/N-2 as the
/// *first* frame of each field is deliberate: older clients decode exactly
/// those 124 bytes and stay bit-compatible, while newer clients read the
/// second frame as the older half. A contiguous `[N-1,N-2]` + `[N-3,N-4]`
/// layout would make an old client heal slot N-2 with N-3's audio.
///
/// Returns `(n1_len, n2_len)` into `pair_1`/`pair_2`.
fn pack_fec_history(
    pair_fec: bool,
    hist: &[[u8; ADPCM_FEC_BYTES]; 4],
    hist_len: usize,
    pair_1: &mut [u8; ADPCM_FEC_BYTES * 2],
    pair_2: &mut [u8; ADPCM_FEC_BYTES * 2],
) -> (usize, usize) {
    if pair_fec && hist_len >= 3 {
        pair_1[..ADPCM_FEC_BYTES].copy_from_slice(&hist[0]);
        pair_1[ADPCM_FEC_BYTES..].copy_from_slice(&hist[2]);
        pair_2[..ADPCM_FEC_BYTES].copy_from_slice(&hist[1]);
        if hist_len >= 4 {
            pair_2[ADPCM_FEC_BYTES..].copy_from_slice(&hist[3]);
            (ADPCM_FEC_BYTES * 2, ADPCM_FEC_BYTES * 2)
        } else {
            // Ramp-up: N-2 alone is still the legacy field for one packet.
            (ADPCM_FEC_BYTES * 2, ADPCM_FEC_BYTES)
        }
    } else if hist_len >= 2 {
        pair_1[..ADPCM_FEC_BYTES].copy_from_slice(&hist[0]);
        pair_2[..ADPCM_FEC_BYTES].copy_from_slice(&hist[1]);
        (ADPCM_FEC_BYTES, ADPCM_FEC_BYTES)
    } else if hist_len == 1 {
        pair_1[..ADPCM_FEC_BYTES].copy_from_slice(&hist[0]);
        (ADPCM_FEC_BYTES, 0)
    } else {
        (0, 0)
    }
}

/// FEC candidates for one datagram of a split primary bundle.
#[derive(Debug, Clone, Copy)]
struct SplitFec<'a> {
    want_n1: bool,
    n1: &'a [u8],
    want_n2: bool,
    n2: &'a [u8],
}

/// Re-base the packet-level FEC history for a one-subframe split datagram.
///
/// For packet `i` (sequence `base + i`), N-1 is the subframe encoded at
/// `i - 1` and N-2 is the subframe at `i - 2`. Index 0 therefore uses the
/// history carried into the bundle, index 1 uses subframe 0 plus the old N-1,
/// and later indexes use the in-packet copies.
#[inline]
fn split_packet_fec<'a>(
    split_idx: usize,
    send_n1: bool,
    send_n2: bool,
    prev_n1: &'a [u8],
    prev_n2: &'a [u8],
    packet_fec: &'a [[u8; ADPCM_FEC_BYTES]; 4],
) -> SplitFec<'a> {
    let (want_n1, n1) = if split_idx == 0 {
        (send_n1, prev_n1)
    } else {
        (true, &packet_fec[split_idx - 1][..])
    };
    let (want_n2, n2) = if split_idx == 0 {
        (send_n2, prev_n2)
    } else if split_idx == 1 {
        (send_n1, prev_n1)
    } else {
        (true, &packet_fec[split_idx - 2][..])
    };
    SplitFec {
        want_n1,
        n1,
        want_n2,
        n2,
    }
}

#[allow(clippy::too_many_arguments)]
fn pack_and_send(
    socket: &UdpSocket,
    datagram: &mut [u8; MAX_SAFE_UDP_PAYLOAD],
    header: WireHeader,
    primary: &[u8],
    fec_n1: &[u8],
    fec_n2: &[u8],
    session_key: Option<&SessionKey>,
    targets: &[SocketAddr],
    telemetry: &HostTelemetry,
) -> bool {
    let Ok(pkt_len) =
        pack_datagram_with_key(header, primary, fec_n1, fec_n2, session_key, datagram)
    else {
        return false;
    };
    for &target_addr in targets {
        if socket.send_to(&datagram[..pkt_len], target_addr).is_ok() {
            telemetry.packets_sent.fetch_add(1, Ordering::Relaxed);
            telemetry
                .bytes_sent
                .fetch_add(pkt_len as u64, Ordering::Relaxed);
        }
    }
    true
}

#[derive(Debug, Clone)]
pub struct ConnectedClient {
    pub addr: SocketAddr,
    pub last_seen: Instant,
    pub is_usb: bool,
    pub muted: bool,
    pub fec_rate_permille: u16,
    /// Genuine Bluetooth A2DP delay reported by the client (0 when not on BT).
    pub bluetooth_delay_ms: u16,
    /// Measured DAC/hardware presentation delay reported by the client.
    pub hardware_delay_ms: u16,
    pub client_volume: u8,
    /// True when this client has engaged 5 ms Gaming Hard-Lock. At least one
    /// such client makes the host tighten packetization for the session.
    pub gaming_lock: bool,
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
    /// Opt-in compressed-codec backend switch. `false` (default) keeps the
    /// pure-Rust VBR codec; `true` routes compressed profiles through the
    /// bundled pure-Rust Opus encoder. Lossless profiles ignore this flag.
    pub prefer_opus: AtomicBool,
    /// True while any connected client has engaged 5 ms Gaming Hard-Lock.
    /// The governor then drops to 1-subframe (2.5 ms) framing for normal
    /// profiles (2 subframes for lossy 2.4 GHz stability profiles).
    pub client_gaming_lock: AtomicBool,
    pub enable_group_sync: AtomicBool,
    pub manual_group_sync_trim_ms: AtomicU16,
    pub target_bitrate_kbps: AtomicU16,
    pub host_volume: AtomicU8,
    pub host_vu_peak_pct: AtomicU8,
    pub active_clients_count: AtomicU8,
    /// Max genuine Bluetooth A2DP delay (for the BT banner and video lip-sync).
    pub max_client_bt_delay_ms: AtomicU16,
    /// Max fixed downstream delay (BT or measured DAC) for multi-device sync.
    pub max_client_downstream_delay_ms: AtomicU16,
    pub hotplug_rebinds: AtomicU16,
    pub trigger_sonar_ping: AtomicBool,
    pub sonar_tx_timestamp_us: AtomicU64,
    /// Actual subframes bundled into each outgoing packet after the quality
    /// governor runs.
    pub tx_subframes_per_packet: AtomicU8,
    /// Actual sender pacing/bundling interval in microseconds.
    pub tx_packet_interval_us: AtomicU32,
    /// Captured audio waiting in the host ring at send time, in microseconds.
    pub capture_buffer_us: AtomicU32,
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
            prefer_opus: AtomicBool::new(false),
            client_gaming_lock: AtomicBool::new(false),
            enable_group_sync: AtomicBool::new(true),
            manual_group_sync_trim_ms: AtomicU16::new(0),
            target_bitrate_kbps: AtomicU16::new(256),
            host_volume: AtomicU8::new(100),
            host_vu_peak_pct: AtomicU8::new(0),
            active_clients_count: AtomicU8::new(initial_targets.len() as u8),
            max_client_bt_delay_ms: AtomicU16::new(0),
            max_client_downstream_delay_ms: AtomicU16::new(0),
            hotplug_rebinds: AtomicU16::new(0),
            trigger_sonar_ping: AtomicBool::new(false),
            sonar_tx_timestamp_us: AtomicU64::new(0),
            tx_subframes_per_packet: AtomicU8::new(config.subframes_per_packet.clamp(1, 4)),
            tx_packet_interval_us: AtomicU32::new(
                (config.subframes_per_packet.clamp(1, 4) as u32) * 2500,
            ),
            capture_buffer_us: AtomicU32::new(0),
        });

        let mut client_map = HashMap::new();
        for addr in initial_targets {
            let is_usb = is_usb_subnet_ip(addr.ip());
            client_map.insert(
                addr,
                ConnectedClient {
                    addr,
                    last_seen: Instant::now(),
                    is_usb,
                    muted: false,
                    fec_rate_permille: 0,
                    bluetooth_delay_ms: 0,
                    hardware_delay_ms: 0,
                    client_volume: 100,
                    gaming_lock: false,
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

    /// Recompute both delay maxima from the live client map.
    ///
    /// `max_client_bt_delay_ms` drives the Bluetooth banner / video lip-sync,
    /// while `max_client_downstream_delay_ms` drives multi-device acoustic
    /// alignment. Keeping them separate prevents a measured DAC latency (or a
    /// stale disconnected client) from being mistaken for Bluetooth. Muted
    /// clients are excluded because they receive no audio and therefore
    /// cannot contribute to either delay.
    /// Drop older sockets from the same non-loopback IP when a client
    /// reconnects with a new source port. Without this, the stale entry keeps
    /// receiving audio for the 10-second sweep window and makes
    /// `cached_targets.len() >= 2` enable group sync for a single phone.
    ///
    /// Loopback is exempt so local multi-client tests can share 127.0.0.1.
    fn evict_same_ip_stale_ports(
        map: &mut HashMap<SocketAddr, ConnectedClient>,
        src_addr: SocketAddr,
    ) {
        if src_addr.ip().is_loopback() {
            return;
        }
        map.retain(|addr, _| addr.ip() != src_addr.ip() || addr.port() == src_addr.port());
    }

    fn refresh_client_delay_telemetry(
        map: &HashMap<SocketAddr, ConnectedClient>,
        telemetry: &HostTelemetry,
    ) {
        let (max_bt, max_downstream) = Self::client_delay_maxima(map);
        telemetry
            .max_client_bt_delay_ms
            .store(max_bt, Ordering::Relaxed);
        telemetry
            .max_client_downstream_delay_ms
            .store(max_downstream, Ordering::Relaxed);
    }

    /// Recompute the session-wide 5 ms hard-lock flag. At least one unmuted
    /// gaming client tightens packetization; when that client mutes, leaves,
    /// or disables the lock, the session returns to the configured framing.
    fn refresh_gaming_lock_telemetry(
        map: &HashMap<SocketAddr, ConnectedClient>,
        telemetry: &HostTelemetry,
    ) {
        telemetry
            .client_gaming_lock
            .store(Self::any_client_gaming_lock(map), Ordering::Relaxed);
    }

    /// Pure computation behind [`Self::refresh_gaming_lock_telemetry`].
    /// Muted clients receive no audio, so they cannot request framing.
    fn any_client_gaming_lock(map: &HashMap<SocketAddr, ConnectedClient>) -> bool {
        map.values().any(|c| !c.muted && c.gaming_lock)
    }

    /// Pure computation behind [`Self::refresh_client_delay_telemetry`].
    ///
    /// Returns `(max_bt, max_downstream)`. Muted clients are excluded from
    /// both because the host no longer sends them audio.
    fn client_delay_maxima(map: &HashMap<SocketAddr, ConnectedClient>) -> (u16, u16) {
        let max_bt = map
            .values()
            .filter(|c| !c.muted)
            .map(|c| c.bluetooth_delay_ms)
            .max()
            .unwrap_or(0);
        let max_downstream = map
            .values()
            .filter(|c| !c.muted)
            .map(|c| c.bluetooth_delay_ms.max(c.hardware_delay_ms))
            .max()
            .unwrap_or(0);
        (max_bt, max_downstream)
    }

    pub fn set_client_muted(&self, addr: SocketAddr, muted: bool) {
        let mut map = self.clients.lock().unwrap();
        if let Some(client) = map.get_mut(&addr) {
            client.muted = muted;
        }
        Self::refresh_client_delay_telemetry(&map, &self.telemetry);
        Self::refresh_gaming_lock_telemetry(&map, &self.telemetry);
    }

    pub fn kick_client(&self, addr: SocketAddr) {
        let mut map = self.clients.lock().unwrap();
        map.remove(&addr);
        self.telemetry
            .active_clients_count
            .store(map.len() as u8, Ordering::Relaxed);
        Self::refresh_client_delay_telemetry(&map, &self.telemetry);
        Self::refresh_gaming_lock_telemetry(&map, &self.telemetry);
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

                fn query_default_monitor() -> Option<String> {
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
                }

                // The default sink is polled by a dedicated, non-real-time
                // watcher thread. Never spawn `pactl` from this capture
                // thread: a 10-100ms subprocess stall would drop audio.
                let latest_monitor = Arc::new(Mutex::new(
                    query_default_monitor().unwrap_or_else(|| "@DEFAULT_MONITOR@".to_string()),
                ));
                let sink_generation = Arc::new(AtomicU64::new(0));
                {
                    let watcher_running = Arc::clone(&running);
                    let watcher_active = Arc::clone(&stream_active_clone);
                    let watcher_latest = Arc::clone(&latest_monitor);
                    let watcher_generation = Arc::clone(&sink_generation);
                    let _ = std::thread::Builder::new()
                        .name("as-ultra-sink-watch".into())
                        .spawn(move || {
                            let mut last = watcher_latest.lock().unwrap().clone();
                            while watcher_running.load(Ordering::Relaxed)
                                && watcher_active.load(Ordering::Relaxed)
                            {
                                std::thread::sleep(Duration::from_secs(2));
                                if let Some(current) = query_default_monitor() {
                                    if current != last {
                                        last = current.clone();
                                        *watcher_latest.lock().unwrap() = current;
                                        watcher_generation.fetch_add(1, Ordering::Release);
                                    }
                                }
                            }
                        });
                }

                while stream_active_clone.load(Ordering::Relaxed) && running.load(Ordering::Relaxed)
                {
                    let generation = sink_generation.load(Ordering::Acquire);
                    let current_monitor = latest_monitor.lock().unwrap().clone();

                    // 1. Try direct in-process libpulse-simple.so.0 (2.5ms hardware fragsize, zero pipe buffering!)
                    if Self::try_inprocess_pulse_simple_capture(
                        &current_monitor,
                        &ring,
                        &heartbeat_ms,
                        &running,
                        &stream_active_clone,
                        start_epoch,
                        &sink_generation,
                        generation,
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
                            "--latency-msec=3",
                            "--process-time-msec=2",
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

                                    // The watcher thread already resolved the
                                    // new sink; just reopen without blocking
                                    // this real-time loop on `pactl`.
                                    if sink_generation.load(Ordering::Acquire) != generation {
                                        break;
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
    fn try_inprocess_pulse_simple_capture(
        monitor_name: &str,
        ring: &Arc<BurstDecouplerRing>,
        heartbeat_ms: &Arc<AtomicU64>,
        running: &Arc<AtomicBool>,
        stream_active: &Arc<AtomicBool>,
        start_epoch: Instant,
        sink_generation: &Arc<AtomicU64>,
        generation: u64,
    ) -> bool {
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
                // CRITICAL: Must be >= 64KB so PipeWire/PulseAudio 10-20ms app quantums
                // never overflow the server ringbuffer while we drain 960B (2.5ms) fragments!
                maxlength: 65_536,
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
                // The watcher thread resolves sink changes off this
                // real-time path; reopening is cheap once it signals.
                if sink_generation.load(Ordering::Acquire) != generation {
                    break;
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
                        let ch = channels.max(1);
                        let in_frames = data.len() / ch;
                        let mut stereo_buf = [0.0f32; 2048];
                        let mut f32_tmp = [0.0f32; 2048];
                        let max_chunk_frames = (2048 / ch).min(1024).max(1);
                        let mut frame_offset = 0usize;
                        while frame_offset < in_frames {
                            let chunk_frames = (in_frames - frame_offset).min(max_chunk_frames);
                            let src_slice =
                                &data[frame_offset * ch..(frame_offset + chunk_frames) * ch];
                            for (dst, &s) in f32_tmp[..src_slice.len()].iter_mut().zip(src_slice) {
                                *dst = (s as f32) * (1.0 / 32768.0);
                            }
                            downmix_to_stereo(
                                &f32_tmp[..chunk_frames * ch],
                                ch,
                                &mut stereo_buf[..chunk_frames * 2],
                            );
                            if src_rate == SAMPLE_RATE {
                                ring_buf.push_samples(&stereo_buf[..chunk_frames * 2]);
                            } else {
                                let mut resampled = [0.0f32; 4096];
                                let out_frames = host_resampler
                                    .process(&stereo_buf[..chunk_frames * 2], &mut resampled);
                                ring_buf.push_samples(&resampled[..out_frames * 2]);
                            }
                            frame_offset += chunk_frames;
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
                        let ch = channels.max(1);
                        let in_frames = data.len() / ch;
                        let mut stereo_buf = [0.0f32; 2048];
                        let mut f32_tmp = [0.0f32; 2048];
                        let max_chunk_frames = (2048 / ch).min(1024).max(1);
                        let mut frame_offset = 0usize;
                        while frame_offset < in_frames {
                            let chunk_frames = (in_frames - frame_offset).min(max_chunk_frames);
                            let src_slice =
                                &data[frame_offset * ch..(frame_offset + chunk_frames) * ch];
                            for (dst, &s) in f32_tmp[..src_slice.len()].iter_mut().zip(src_slice) {
                                *dst = ((s as f32) - 32768.0) * (1.0 / 32768.0);
                            }
                            downmix_to_stereo(
                                &f32_tmp[..chunk_frames * ch],
                                ch,
                                &mut stereo_buf[..chunk_frames * 2],
                            );
                            if src_rate == SAMPLE_RATE {
                                ring_buf.push_samples(&stereo_buf[..chunk_frames * 2]);
                            } else {
                                let mut resampled = [0.0f32; 4096];
                                let out_frames = host_resampler
                                    .process(&stereo_buf[..chunk_frames * 2], &mut resampled);
                                ring_buf.push_samples(&resampled[..out_frames * 2]);
                            }
                            frame_offset += chunk_frames;
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
                        let ch = channels.max(1);
                        let in_frames = data.len() / ch;
                        let mut stereo_buf = [0.0f32; 2048];
                        let max_chunk_frames = (2048 / ch).min(1024).max(1);
                        let mut frame_offset = 0usize;
                        while frame_offset < in_frames {
                            let chunk_frames = (in_frames - frame_offset).min(max_chunk_frames);
                            let src_slice =
                                &data[frame_offset * ch..(frame_offset + chunk_frames) * ch];
                            downmix_to_stereo(src_slice, ch, &mut stereo_buf[..chunk_frames * 2]);
                            if src_rate == SAMPLE_RATE {
                                ring_buf.push_samples(&stereo_buf[..chunk_frames * 2]);
                            } else {
                                let mut resampled = [0.0f32; 4096];
                                let out_frames = host_resampler
                                    .process(&stereo_buf[..chunk_frames * 2], &mut resampled);
                                ring_buf.push_samples(&resampled[..out_frames * 2]);
                            }
                            frame_offset += chunk_frames;
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
        let mut last_tx_instant = start_instant;

        let mut seq: u16 = 0;
        // Sliding FEC history, newest first. Normal framing sends history[0]
        // as N-1 and history[1] as N-2. Four-subframe 10 ms framing packs
        // [N-1, N-2] into the first FEC field and [N-3, N-4] into the second
        // so one lost 10 ms packet is fully healed by the next packet.
        let mut fec_hist = [[0u8; ADPCM_FEC_BYTES]; 4];
        let mut fec_hist_len: usize = 0;

        let mut silent_subframes: u32 = 0;
        let mut tone_phase: f32 = 0.0;
        let mut ctrl_buf = [0u8; 64];
        let mut dynamic_codec_mode;
        let mut tx_bitrate_kbps;
        let mut loss_governor = LossGovernor::new(Instant::now());
        let mut noise_shaper = audioshare_protocol::sync_and_eq::NoiseShapedQuantizer::new();
        // Lazily created on the first compressed packet after the user enables
        // the Opus backend. Smart Auto also keeps it warm through Tier 0 so
        // entering the compressed tier does not emit lookahead silence.
        // Other lossless profiles drop it so a later switch starts clean.
        let mut opus_encoder: Option<OpusStreamEncoder> = None;
        let mut opus_encoder_failed = false;
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
                let is_usb = is_usb_subnet_ip(src_addr.ip());

                if len >= 8 && &ctrl_buf[..8] == CONTROL_MAGIC_HELLO {
                    let mut map = clients.lock().unwrap();
                    Self::evict_same_ip_stale_ports(&mut map, src_addr);
                    map.entry(src_addr)
                        .or_insert(ConnectedClient {
                            addr: src_addr,
                            last_seen: Instant::now(),
                            is_usb,
                            muted: false,
                            fec_rate_permille: 0,
                            bluetooth_delay_ms: 0,
                            hardware_delay_ms: 0,
                            client_volume: 100,
                            gaming_lock: false,
                        })
                        .last_seen = Instant::now();
                    telemetry
                        .active_clients_count
                        .store(map.len() as u8, Ordering::Relaxed);
                    Self::refresh_client_delay_telemetry(&map, &telemetry);
                    Self::refresh_gaming_lock_telemetry(&map, &telemetry);
                    cached_targets.clear();
                    for c in map.values() {
                        if !c.muted {
                            cached_targets.push(c.addr);
                        }
                    }
                } else if len >= 8 && &ctrl_buf[..8] == CONTROL_FEEDBACK_MAGIC {
                    // Feedback carries remote-control commands (quality profile,
                    // codec backend, media transport) and group-sync delays.
                    // When a PIN is configured, require the keyed MAC over the
                    // complete body before acting on any of it. Unencrypted LAN
                    // and party mode have no key and stay backward compatible.
                    let active_key = *session_key.lock().unwrap();
                    if let Some(fb) =
                        ClientFeedbackPacket::decode_verified(&ctrl_buf[..len], active_key.as_ref())
                    {
                        let mut gaming_lock_update: Option<bool> = None;

                        // Compressed-codec backend selection from the phone UI:
                        // 20 = pure-Rust VBR (default), 21 = bundled Opus RLD.
                        // This is separate from the quality profile and only
                        // affects compressed modes.
                        // 40/41 carry the client's 5 ms hard-lock state so the
                        // host can tighten packetization for the session.
                        match fb.requested_mode {
                            20 => telemetry.prefer_opus.store(false, Ordering::Relaxed),
                            21 => telemetry.prefer_opus.store(true, Ordering::Relaxed),
                            40 => gaming_lock_update = Some(false),
                            41 => gaming_lock_update = Some(true),
                            _ => {}
                        }

                        // Execute remote Bluetooth earbud media command (Play/Pause/Next/Prev) if present.
                        // Keep this range below 20 so codec-backend commands are
                        // not mistaken for transport commands.
                        if (10..20).contains(&fb.requested_mode) {
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
                        // Bitrate selections are 30=96, 31=160, 32=256, 33=320 kbps.
                        // Other values are media transport or codec-backend
                        // commands handled above.
                        if (1..=5).contains(&fb.requested_mode) {
                            telemetry
                                .quality_profile
                                .store(fb.requested_mode - 1, Ordering::Relaxed);
                        } else {
                            let kbps = match fb.requested_mode {
                                30 => Some(96u16),
                                31 => Some(160),
                                32 => Some(256),
                                33 => Some(320),
                                _ => None,
                            };
                            if let Some(kbps) = kbps {
                                telemetry.target_bitrate_kbps.store(kbps, Ordering::Relaxed);
                            }
                        }

                        let mut map = clients.lock().unwrap();
                        Self::evict_same_ip_stale_ports(&mut map, src_addr);
                        let entry = map.entry(src_addr).or_insert(ConnectedClient {
                            addr: src_addr,
                            last_seen: Instant::now(),
                            is_usb,
                            muted: false,
                            fec_rate_permille: fb.fec_rate_permille,
                            bluetooth_delay_ms: fb.bluetooth_delay_ms,
                            hardware_delay_ms: fb.hardware_delay_ms,
                            client_volume: fb.client_volume,
                            gaming_lock: false,
                        });
                        entry.last_seen = Instant::now();
                        entry.fec_rate_permille = fb.fec_rate_permille;
                        entry.bluetooth_delay_ms = fb.bluetooth_delay_ms;
                        entry.hardware_delay_ms = fb.hardware_delay_ms;
                        entry.client_volume = fb.client_volume;
                        if let Some(gaming_lock) = gaming_lock_update {
                            entry.gaming_lock = gaming_lock;
                        }
                        telemetry
                            .active_clients_count
                            .store(map.len() as u8, Ordering::Relaxed);

                        // Fast attack / paced release. A borderline link
                        // that alternates between 5% and 0% loss must not
                        // recreate the Opus encoder every 500 ms. Muted
                        // clients do not receive audio, so they cannot drive
                        // the governor.
                        let raw_worst = map
                            .values()
                            .filter(|c| !c.muted)
                            .map(|c| c.fec_rate_permille)
                            .max()
                            .unwrap_or(0);
                        loss_governor.update(raw_worst, Instant::now());
                        Self::refresh_client_delay_telemetry(&map, &telemetry);
                        Self::refresh_gaming_lock_telemetry(&map, &telemetry);

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
                    let raw_worst = map
                        .values()
                        .filter(|c| !c.muted)
                        .map(|c| c.fec_rate_permille)
                        .max()
                        .unwrap_or(0);
                    loss_governor.update(raw_worst, Instant::now());
                    Self::refresh_client_delay_telemetry(&map, &telemetry);
                    Self::refresh_gaming_lock_telemetry(&map, &telemetry);
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
            // A connected 5 ms hard-lock client requests the tightest framing
            // the profile can sustain. Normal profiles drop to one subframe
            // (2.5 ms packets); lossy 2.4 GHz stability profiles stay at two
            // subframes so the doubled packet rate does not worsen contention.
            let gaming_lock = telemetry.client_gaming_lock.load(Ordering::Relaxed);
            let base_subframes = if gaming_lock {
                1
            } else {
                config.subframes_per_packet.clamp(1, 2) as usize
            };
            // Tier selection uses the governor's paced estimate, not the raw
            // last report, so a post-hold release walks through Tier 1 instead
            // of jumping straight from Tier 2 back to lossless.
            let worst_loss_permille = loss_governor.value;
            match profile {
                QualityProfile::StudioLossless => {
                    dynamic_codec_mode = CodecMode::StudioLosslessLpc;
                    subframes_per_pkt = base_subframes;
                    tx_bitrate_kbps = selected_kbps;
                }
                QualityProfile::LowLatencyCompressed => {
                    dynamic_codec_mode = CodecMode::CompressedLowDelay;
                    subframes_per_pkt = base_subframes;
                    tx_bitrate_kbps = selected_kbps;
                }
                QualityProfile::MaxStability24Ghz => {
                    // 4 x 2.5 ms = 10.0 ms framing (100 pps) as advertised by
                    // the profile name. Longer packets cut MAC contention and
                    // retransmit bursts on congested 2.4 GHz links; the client
                    // WSOLA PLC covers subframes not carried by [N-1]/[N-2].
                    // Gaming lock halves that to 5 ms instead of doubling the
                    // packet rate to 400 pps on an already lossy link.
                    dynamic_codec_mode = CodecMode::CompressedLowDelay;
                    subframes_per_pkt = if gaming_lock { 2 } else { 4 };
                    tx_bitrate_kbps = stability_bitrate_kbps(selected_kbps, worst_loss_permille);
                }
                QualityProfile::StudioLossless24Bit => {
                    // Two subframes halve MAC contention (200 packets/s).
                    // A worst-case 24-bit subframe is huge, so the sender
                    // drops FEC first and splits into per-subframe datagrams
                    // only when the primary itself cannot fit the safe MTU.
                    dynamic_codec_mode = CodecMode::StudioLosslessLpc;
                    subframes_per_pkt = base_subframes;
                    tx_bitrate_kbps = selected_kbps;
                }
                QualityProfile::SmartAuto => {
                    if worst_loss_permille > SMART_AUTO_HIGH_LOSS_THRESHOLD_PERMILLE {
                        dynamic_codec_mode = CodecMode::CompressedLowDelay;
                        subframes_per_pkt = if gaming_lock { 2 } else { 4 };
                        tx_bitrate_kbps =
                            stability_bitrate_kbps(selected_kbps, worst_loss_permille);
                    } else if worst_loss_permille > SMART_AUTO_COMPRESS_THRESHOLD_PERMILLE {
                        dynamic_codec_mode = CodecMode::CompressedLowDelay;
                        subframes_per_pkt = if gaming_lock { 1 } else { 2 };
                        tx_bitrate_kbps = selected_kbps;
                    } else {
                        dynamic_codec_mode = CodecMode::StudioLosslessLpc;
                        subframes_per_pkt = base_subframes;
                        tx_bitrate_kbps = selected_kbps;
                    }
                }
            }
            // Compressed-codec backend selection. Opus is opt-in and only
            // replaces the payload of compressed profiles; lossless and
            // 24-bit modes are untouched. If encoder creation fails we stay
            // on the pure-Rust VBR path instead of dropping audio. Smart Auto
            // also keeps the encoder warm during Tier 0 so the first
            // compressed packet contains live lookahead audio rather than a
            // silent pre-roll; other lossless profiles still drop it.
            let prefer_opus = telemetry.prefer_opus.load(Ordering::Relaxed);
            if !prefer_opus {
                opus_encoder = None;
                opus_encoder_failed = false;
            } else if dynamic_codec_mode == CodecMode::CompressedLowDelay
                || profile == QualityProfile::SmartAuto
            {
                if opus_encoder.is_none() && !opus_encoder_failed {
                    opus_encoder = OpusStreamEncoder::new(tx_bitrate_kbps);
                    if opus_encoder.is_none() {
                        opus_encoder_failed = true;
                    }
                }
                if dynamic_codec_mode == CodecMode::CompressedLowDelay && opus_encoder.is_some() {
                    dynamic_codec_mode = CodecMode::OpusRld;
                }
            } else {
                // Leaving compressed mode: drop the codec state so a later
                // compressed period starts clean.
                opus_encoder = None;
            }

            packet_interval = Duration::from_micros((2500 * subframes_per_pkt) as u64);
            telemetry
                .active_codec_mode
                .store(dynamic_codec_mode as u8, Ordering::Relaxed);
            telemetry
                .tx_subframes_per_packet
                .store(subframes_per_pkt as u8, Ordering::Relaxed);
            telemetry
                .tx_packet_interval_us
                .store((2500 * subframes_per_pkt) as u32, Ordering::Relaxed);

            // Wait until soundcard pushes a full packet of samples into `ring`,
            // or fall back to silence synthesis only if capture endpoint is truly idle >80ms.
            let needed_samples = subframes_per_pkt * SUBFRAME_STEREO_SAMPLES;
            if !config.use_test_tone {
                let avail = ring.available_samples();
                // Allow up to 24 subframes (60ms = 5760 samples) so normal 10-22ms OS callbacks
                // on Windows WASAPI and Linux PipeWire are NEVER discarded mid-callback!
                if avail > SUBFRAME_STEREO_SAMPLES * 24 {
                    let mut discard = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                    let mut dummy_rng = 1u32;
                    while ring.available_samples() > needed_samples + SUBFRAME_STEREO_SAMPLES * 4 {
                        ring.pop_exact_or_dither(&mut discard, &mut dummy_rng);
                    }
                } else if avail < needed_samples {
                    let now_ms = start_instant.elapsed().as_millis() as u64;
                    let last_cb = capture_heartbeat_ms.load(Ordering::Relaxed);
                    if now_ms.saturating_sub(last_cb) <= 80 {
                        std::thread::sleep(Duration::from_micros(500));
                        continue;
                    }
                }
            }

            // Capture backlog at send time (0 in synthetic test-tone mode).
            let captured_subframes = ring.available_samples() / SUBFRAME_STEREO_SAMPLES;
            telemetry.capture_buffer_us.store(
                (captured_subframes as u32).saturating_mul(2500),
                Ordering::Relaxed,
            );

            // 3. Encode Subframes
            let mut primary_bundle = [0u8; MAX_LOSSLESS_PCM24_BYTES * 4 + 12];
            let mut primary_cursor = 0usize;
            // Byte range of each length-prefixed subframe inside
            // `primary_bundle`, used by the MTU split fallback.
            let mut subframe_ranges = [(0usize, 0usize); 4];
            // FEC ADPCM copy of each subframe encoded in this packet. The
            // per-subframe split path uses [i-1] and [i-2] as its N-1/N-2.
            let mut packet_fec = [[0u8; ADPCM_FEC_BYTES]; 4];
            let base_seq = seq;
            let prev_hist = fec_hist;
            let prev_hist_len = fec_hist_len;
            let send_n1 = prev_hist_len >= 1;
            let send_n2 = prev_hist_len >= 2;
            let mut packet_is_silent = true;

            for sub_idx in 0..subframes_per_pkt {
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

                fec_hist.copy_within(0..3, 1);
                encode_fec_subframe(&i16_subframe, &mut fec_hist[0]);
                fec_hist_len = (fec_hist_len + 1).min(fec_hist.len());
                packet_fec[sub_idx] = fec_hist[0];

                let entry_start = primary_cursor;
                if matches!(
                    dynamic_codec_mode,
                    CodecMode::CompressedLowDelay | CodecMode::OpusRld
                ) {
                    // Compressed primary payload targeted by the selected
                    // quality mode. The tag inside the payload tells the
                    // receiver whether this is pure-Rust VBR or Opus, so a
                    // live backend switch never requires a client restart.
                    let mut enc_primary = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
                    let enc_len = if dynamic_codec_mode == CodecMode::OpusRld {
                        let opus_len = opus_encoder.as_mut().and_then(|encoder| {
                            encoder.encode_subframe(
                                &i16_subframe,
                                tx_bitrate_kbps,
                                &mut enc_primary,
                            )
                        });
                        match opus_len {
                            Some(len) => len,
                            None => {
                                // Never drop audio on a codec failure. Fall
                                // back to VBR for this packet, mark the Opus
                                // backend unavailable, and let the next
                                // toggle retry it.
                                opus_encoder = None;
                                opus_encoder_failed = true;
                                dynamic_codec_mode = CodecMode::CompressedLowDelay;
                                encode_vbr_subframe(
                                    &i16_subframe,
                                    tx_bitrate_kbps,
                                    &mut enc_primary,
                                )
                            }
                        }
                    } else {
                        encode_vbr_subframe(&i16_subframe, tx_bitrate_kbps, &mut enc_primary)
                    };
                    let len_be = (enc_len as u16).to_be_bytes();
                    primary_bundle[primary_cursor] = len_be[0];
                    primary_bundle[primary_cursor + 1] = len_be[1];
                    primary_cursor += 2;
                    primary_bundle[primary_cursor..primary_cursor + enc_len]
                        .copy_from_slice(&enc_primary[..enc_len]);
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
                    // Tier 0 of Smart Auto is lossless, but the Opus encoder
                    // is kept primed so a sudden loss spike does not start
                    // with 2.5 ms of codec warm-up silence.
                    if profile == QualityProfile::SmartAuto && prefer_opus {
                        warm_opus_lookahead(&mut opus_encoder, &i16_subframe, tx_bitrate_kbps);
                    }
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
                subframe_ranges[sub_idx] = (entry_start, primary_cursor);

                seq = seq.wrapping_add(1);
            }

            // If an Opus encode failed mid-packet we fell back to VBR above;
            // publish the mode that matches the payload actually built.
            telemetry
                .active_codec_mode
                .store(dynamic_codec_mode as u8, Ordering::Relaxed);

            let tx_timestamp_us = start_instant.elapsed().as_micros() as u32;
            let mut datagram = [0u8; MAX_SAFE_UDP_PAYLOAD];
            let active_key = *session_key.lock().unwrap();

            let is_dtx_active =
                config.enable_smart_dtx && packet_is_silent && silent_subframes > 200;
            telemetry
                .dtx_silent_mode
                .store(is_dtx_active, Ordering::Relaxed);

            let mut sent_any = false;
            if is_dtx_active {
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
                        // Broadcast max group delay (in 2ms units) when Multi-Device Acoustic Sync is enabled,
                        // or 0 when Independent Lowest-Latency Mode is selected!
                        stream_id: if telemetry.enable_group_sync.load(Ordering::Relaxed)
                            && (cached_targets.len() >= 2
                                || telemetry.manual_group_sync_trim_ms.load(Ordering::Relaxed) > 0)
                        {
                            let total_sync_ms = telemetry
                                .max_client_downstream_delay_ms
                                .load(Ordering::Relaxed)
                                .saturating_add(
                                    telemetry.manual_group_sync_trim_ms.load(Ordering::Relaxed),
                                );
                            (total_sync_ms / 2).min(255) as u8
                        } else {
                            0
                        },
                        header_crc16: 0,
                    };
                    sent_any = pack_and_send(
                        &socket,
                        &mut datagram,
                        hdr,
                        &[],
                        &[],
                        &[],
                        active_key.as_ref(),
                        &cached_targets,
                        &telemetry,
                    );
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
                    // Broadcast max group delay (in 2ms units) when Multi-Device Acoustic Sync is enabled,
                    // or 0 when Independent Lowest-Latency Mode is selected!
                    stream_id: if telemetry.enable_group_sync.load(Ordering::Relaxed)
                        && (cached_targets.len() >= 2
                            || telemetry.manual_group_sync_trim_ms.load(Ordering::Relaxed) > 0)
                    {
                        let total_sync_ms = telemetry
                            .max_client_downstream_delay_ms
                            .load(Ordering::Relaxed)
                            .saturating_add(
                                telemetry.manual_group_sync_trim_ms.load(Ordering::Relaxed),
                            );
                        (total_sync_ms / 2).min(255) as u8
                    } else {
                        0
                    },
                    header_crc16: 0,
                };
                // 2.4 GHz 10 ms framing carries the previous four subframes
                // across the two FEC fields: [N-1,N-3] and [N-2,N-4]. Each
                // field stays <= 255 bytes, so the wire header is unchanged;
                // old clients decode the first 124-byte frame of each field
                // (N-1/N-2) and ignore the rest. Opus skips ADPCM FEC on the
                // client, so the pair is only packed for the pure-Rust VBR
                // backend.
                let pair_fec = subframes_per_pkt == 4 && dynamic_codec_mode != CodecMode::OpusRld;
                let mut fec_pair_1 = [0u8; ADPCM_FEC_BYTES * 2];
                let mut fec_pair_2 = [0u8; ADPCM_FEC_BYTES * 2];
                let (n1_len, n2_len) = pack_fec_history(
                    pair_fec,
                    &prev_hist,
                    prev_hist_len,
                    &mut fec_pair_1,
                    &mut fec_pair_2,
                );
                let full_n1 = &fec_pair_1[..n1_len];
                let full_n2 = &fec_pair_2[..n2_len];
                match plan_datagram_with_fec(
                    primary_cursor,
                    active_key.is_some(),
                    full_n1.len(),
                    full_n2.len(),
                ) {
                    DatagramPlan::Single { n1, n2 } => {
                        let f1_slice = if n1 { full_n1 } else { &[] };
                        let f2_slice = if n2 { full_n2 } else { &[] };
                        sent_any = pack_and_send(
                            &socket,
                            &mut datagram,
                            hdr,
                            &primary_bundle[..primary_cursor],
                            f1_slice,
                            f2_slice,
                            active_key.as_ref(),
                            &cached_targets,
                            &telemetry,
                        );
                    }
                    DatagramPlan::SplitPerSubframe => {
                        // Two incompressible 24-bit subframes can exceed the
                        // MTU even with both FEC frames removed. Keep the
                        // audio by sending one subframe per datagram, with the
                        // FEC history re-based to each new packet sequence.
                        for split_idx in 0..subframes_per_pkt {
                            let (start, end) = subframe_ranges[split_idx];
                            let mut split_hdr = hdr;
                            split_hdr.subframe_count = 1;
                            split_hdr.sequence = base_seq.wrapping_add(split_idx as u16);

                            let fec = split_packet_fec(
                                split_idx,
                                send_n1,
                                send_n2,
                                &prev_hist[0],
                                &prev_hist[1],
                                &packet_fec,
                            );
                            let (n1, n2) = match plan_datagram(
                                end - start,
                                active_key.is_some(),
                                fec.want_n1,
                                fec.want_n2,
                            ) {
                                DatagramPlan::Single { n1, n2 } => (n1, n2),
                                DatagramPlan::SplitPerSubframe => {
                                    // A single 24-bit subframe is at most
                                    // 722 bytes and always fits.
                                    debug_assert!(false, "single subframe exceeds safe MTU");
                                    (false, false)
                                }
                            };
                            let f1_slice = if n1 { fec.n1 } else { &[] };
                            let f2_slice = if n2 { fec.n2 } else { &[] };
                            sent_any |= pack_and_send(
                                &socket,
                                &mut datagram,
                                split_hdr,
                                &primary_bundle[start..end],
                                f1_slice,
                                f2_slice,
                                active_key.as_ref(),
                                &cached_targets,
                                &telemetry,
                            );
                        }
                    }
                }
            }

            // Capture-Clock-Master Packet Pacer:
            //
            // The capture endpoint (PipeWire/WASAPI/AAudio) is already a
            // real-time clock: it delivers samples at exactly the playback
            // rate. Waiting for a full packet at the top of the loop and then
            // also sleeping until an independent `next_tick` deadline double-
            // paces the loop and starves the socket between callbacks.
            //
            // When capture is live, send as soon as a full packet is ready.
            // Only enforce a small anti-burst gap while a backlog remains,
            // because OS callbacks may hand us 10-20ms of audio at once.
            // The original isochronous pacer is retained for the synthetic
            // test tone (no capture clock) and for a stalled capture endpoint,
            // where silence must still be synthesized at real-time cadence.
            let now = Instant::now();
            let capture_active = !config.use_test_tone
                && (start_instant.elapsed().as_millis() as u64)
                    .saturating_sub(capture_heartbeat_ms.load(Ordering::Relaxed))
                    <= 80;
            if capture_active {
                if ring.available_samples() >= needed_samples {
                    // Spread a 10-20 ms OS capture callback across its
                    // callback period instead of dumping every packet in a
                    // 1.5 ms burst and then starving the network. A backlog
                    // above 20 ms drains faster so it cannot become latency.
                    let remaining_subframes = ring.available_samples() / SUBFRAME_STEREO_SAMPLES;
                    let min_gap = Duration::from_micros(capture_min_gap_us(
                        subframes_per_pkt,
                        remaining_subframes,
                    ));
                    let since_tx = now.saturating_duration_since(last_tx_instant);
                    if since_tx < min_gap {
                        std::thread::sleep(min_gap - since_tx);
                    }
                }
                if sent_any {
                    last_tx_instant = Instant::now();
                }
                // Keep the isochronous deadline anchored so a later capture
                // stall does not trigger a catch-up burst.
                next_tick = last_tx_instant + packet_interval;
            } else {
                let step_interval =
                    if !config.use_test_tone && ring.available_samples() > needed_samples * 3 {
                        Duration::from_micros(((2300 * subframes_per_pkt) as u64).max(1000))
                    } else {
                        packet_interval
                    };
                let now = Instant::now();
                if next_tick > now {
                    std::thread::sleep(next_tick - now);
                    next_tick += step_interval;
                } else if now.duration_since(next_tick) > packet_interval * 4 {
                    next_tick = now + step_interval;
                } else {
                    next_tick += step_interval;
                }
                if sent_any {
                    last_tx_instant = Instant::now();
                }
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
    fn test_client_delay_maxima_separates_bt_and_hardware() {
        let mut map = HashMap::new();
        let wired: SocketAddr = "127.0.0.1:1001".parse().unwrap();
        let bluetooth: SocketAddr = "127.0.0.1:1002".parse().unwrap();
        let muted_bluetooth: SocketAddr = "127.0.0.1:1003".parse().unwrap();
        map.insert(
            wired,
            ConnectedClient {
                addr: wired,
                last_seen: Instant::now(),
                is_usb: true,
                muted: false,
                fec_rate_permille: 0,
                bluetooth_delay_ms: 0,
                hardware_delay_ms: 17,
                client_volume: 100,
                gaming_lock: false,
            },
        );
        map.insert(
            bluetooth,
            ConnectedClient {
                addr: bluetooth,
                last_seen: Instant::now(),
                is_usb: false,
                muted: false,
                fec_rate_permille: 0,
                bluetooth_delay_ms: 145,
                hardware_delay_ms: 30,
                client_volume: 100,
                gaming_lock: false,
            },
        );
        map.insert(
            muted_bluetooth,
            ConnectedClient {
                addr: muted_bluetooth,
                last_seen: Instant::now(),
                is_usb: false,
                muted: true,
                fec_rate_permille: 0,
                bluetooth_delay_ms: 200,
                hardware_delay_ms: 200,
                client_volume: 100,
                gaming_lock: false,
            },
        );

        // BT remains a genuine A2DP-only metric; downstream alignment takes
        // the larger of A2DP and measured DAC latency. The muted client must
        // not influence either value.
        assert_eq!(HostEngine::client_delay_maxima(&map), (145, 145));

        // Without any BT client, a wired DAC delay must not be reported as BT.
        map.remove(&bluetooth);
        assert_eq!(HostEngine::client_delay_maxima(&map), (0, 17));
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

    #[test]
    fn test_opus_backend_selected_for_compressed_profile() {
        let config = HostConfig {
            bind_port: 0,
            use_test_tone: true,
            subframes_per_packet: 1,
            enable_smart_dtx: false,
        };
        let mut engine = HostEngine::start(config, vec![]).expect("start host engine");
        engine.telemetry.quality_profile.store(
            QualityProfile::LowLatencyCompressed as u8,
            Ordering::Relaxed,
        );
        engine.telemetry.prefer_opus.store(true, Ordering::Relaxed);

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut observed = 0u8;
        while Instant::now() < deadline {
            observed = engine.telemetry.active_codec_mode.load(Ordering::Relaxed);
            if observed == CodecMode::OpusRld as u8 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            observed,
            CodecMode::OpusRld as u8,
            "host did not switch the compressed profile to Opus RLD"
        );
        engine.stop();
    }

    #[test]
    fn test_usb_interface_name_detection() {
        for name in [
            "usb0",
            "rndis0",
            "ncm0",
            "enx001122334455",
            "enp0s20f0u1",
            "ENX001122334455",
        ] {
            assert!(is_usb_interface_name(name), "{} should be USB", name);
        }
        for name in ["eth0", "wlp2s0", "wlo1", "enp3s0", "lo", "docker0"] {
            assert!(!is_usb_interface_name(name), "{} should not be USB", name);
        }
    }

    #[test]
    fn test_is_usb_subnet_ip_recognizes_loopback_and_legacy_rndis() {
        assert!(is_usb_subnet_ip("127.0.0.1".parse().unwrap()));
        assert!(is_usb_subnet_ip("192.168.42.7".parse().unwrap()));
        assert!(is_usb_subnet_ip("192.168.137.7".parse().unwrap()));
        // TEST-NET-3 is never a local tethering subnet.
        assert!(!is_usb_subnet_ip("203.0.113.7".parse().unwrap()));
        assert!(!is_usb_subnet_ip("2001:db8::1".parse().unwrap()));
    }

    #[test]
    fn test_datagram_plan_splits_bundles_to_preserve_fec() {
        // Worst-case two 24-bit subframes = 2 * (2-byte length + 722 bytes).
        let worst_two = 2 * (2 + MAX_LOSSLESS_PCM24_BYTES);
        assert_eq!(worst_two, 1448);
        assert_eq!(
            plan_datagram(worst_two, true, true, true),
            DatagramPlan::SplitPerSubframe
        );
        assert_eq!(
            plan_datagram(worst_two, false, true, true),
            DatagramPlan::SplitPerSubframe
        );

        // One worst-case subframe always fits with both FEC frames.
        let worst_one = 2 + MAX_LOSSLESS_PCM24_BYTES;
        assert_eq!(
            plan_datagram(worst_one, true, true, true),
            DatagramPlan::Single { n1: true, n2: true }
        );

        // This is the regression from report 1.5: a typical two-subframe
        // 24-bit bundle fits the MTU without FEC but not with N-1. It must
        // split so each 724-byte subframe carries both FEC backups, rather
        // than silently disabling FEC for the whole stream.
        let typical_24bit_two = 1280;
        assert_eq!(
            plan_datagram(typical_24bit_two, true, true, true),
            DatagramPlan::SplitPerSubframe
        );
        // With no FEC requested (stream start) there is nothing to preserve.
        assert_eq!(
            plan_datagram(typical_24bit_two, true, false, false),
            DatagramPlan::Single {
                n1: false,
                n2: false
            }
        );

        let two_fec_budget = MAX_SAFE_UDP_PAYLOAD - HEADER_LEN - MAC_TAG_LEN - 2 * ADPCM_FEC_BYTES;
        assert_eq!(
            plan_datagram(two_fec_budget, true, true, true),
            DatagramPlan::Single { n1: true, n2: true }
        );
        // One byte over the two-FEC budget: drop the older N-2 first.
        assert_eq!(
            plan_datagram(two_fec_budget + 1, true, true, true),
            DatagramPlan::Single {
                n1: true,
                n2: false
            }
        );
        // One byte over the one-FEC budget: split to preserve N-1.
        let one_fec_budget = two_fec_budget + ADPCM_FEC_BYTES;
        assert_eq!(
            plan_datagram(one_fec_budget + 1, true, true, true),
            DatagramPlan::SplitPerSubframe
        );
        // The same packet is fine when the caller does not want FEC.
        assert_eq!(
            plan_datagram(one_fec_budget + 1, true, false, false),
            DatagramPlan::Single {
                n1: false,
                n2: false
            }
        );
        // Encrypted packets reserve a 4-byte MAC tag; unencrypted ones do not.
        let primary_only = MAX_SAFE_UDP_PAYLOAD - HEADER_LEN - MAC_TAG_LEN;
        assert_eq!(
            plan_datagram(primary_only, true, true, true),
            DatagramPlan::SplitPerSubframe
        );
        assert_eq!(
            plan_datagram(primary_only, true, false, false),
            DatagramPlan::Single {
                n1: false,
                n2: false
            }
        );
        assert_eq!(
            plan_datagram(primary_only + 1, true, true, true),
            DatagramPlan::SplitPerSubframe
        );
        assert_eq!(
            plan_datagram(primary_only + 1, false, true, true),
            DatagramPlan::SplitPerSubframe
        );
        assert_eq!(
            plan_datagram(primary_only + 1 + MAC_TAG_LEN, false, true, true),
            DatagramPlan::SplitPerSubframe
        );
    }

    #[test]
    fn test_split_fec_history_rebases_per_subframe() {
        let prev_n1 = [0x11u8; ADPCM_FEC_BYTES];
        let prev_n2 = [0x22u8; ADPCM_FEC_BYTES];
        let mut packet_fec = [[0u8; ADPCM_FEC_BYTES]; 4];
        packet_fec[0] = [0xAA; ADPCM_FEC_BYTES];
        packet_fec[1] = [0xBB; ADPCM_FEC_BYTES];

        // Packet 0 keeps the history that preceded the bundle.
        let f0 = split_packet_fec(0, true, true, &prev_n1, &prev_n2, &packet_fec);
        assert!(f0.want_n1 && f0.want_n2);
        assert_eq!(f0.n1, &prev_n1[..]);
        assert_eq!(f0.n2, &prev_n2[..]);

        // Packet 1: N-1 is subframe 0; N-2 is the old N-1.
        let f1 = split_packet_fec(1, true, true, &prev_n1, &prev_n2, &packet_fec);
        assert!(f1.want_n1 && f1.want_n2);
        assert_eq!(f1.n1, &packet_fec[0][..]);
        assert_eq!(f1.n2, &prev_n1[..]);

        // Packet 2: N-1 is subframe 1; N-2 is subframe 0.
        let f2 = split_packet_fec(2, true, true, &prev_n1, &prev_n2, &packet_fec);
        assert!(f2.want_n1 && f2.want_n2);
        assert_eq!(f2.n1, &packet_fec[1][..]);
        assert_eq!(f2.n2, &packet_fec[0][..]);

        // At stream start there is no old N-1, so packet 1 has no N-2.
        let f1_start = split_packet_fec(1, false, false, &prev_n1, &prev_n2, &packet_fec);
        assert!(f1_start.want_n1);
        assert!(!f1_start.want_n2);
    }

    #[test]
    fn test_plan_datagram_with_paired_fec_fields() {
        let pair = ADPCM_FEC_BYTES * 2;

        // 10 ms 2.4 GHz framing: one paired field per FEC slot.
        let both_pairs = MAX_SAFE_UDP_PAYLOAD - HEADER_LEN - MAC_TAG_LEN - 2 * pair;
        assert_eq!(
            plan_datagram_with_fec(both_pairs, true, pair, pair),
            DatagramPlan::Single { n1: true, n2: true }
        );
        // One byte over: drop the older N-3/N-4 pair first.
        assert_eq!(
            plan_datagram_with_fec(both_pairs + 1, true, pair, pair),
            DatagramPlan::Single {
                n1: true,
                n2: false
            }
        );

        // A paired N-1 field plus a single N-2 frame (history ramp-up).
        let pair_plus_one =
            MAX_SAFE_UDP_PAYLOAD - HEADER_LEN - MAC_TAG_LEN - pair - ADPCM_FEC_BYTES;
        assert_eq!(
            plan_datagram_with_fec(pair_plus_one, true, pair, ADPCM_FEC_BYTES),
            DatagramPlan::Single { n1: true, n2: true }
        );
        assert_eq!(
            plan_datagram_with_fec(pair_plus_one + 1, true, pair, ADPCM_FEC_BYTES),
            DatagramPlan::Single {
                n1: true,
                n2: false
            }
        );

        // Two paired fields no longer fit: split so both survive per subframe.
        let pair_only = MAX_SAFE_UDP_PAYLOAD - HEADER_LEN - MAC_TAG_LEN - pair;
        assert_eq!(
            plan_datagram_with_fec(pair_only + 1, true, pair, pair),
            DatagramPlan::SplitPerSubframe
        );
        assert_eq!(
            plan_datagram_with_fec(pair_only + 1, true, pair, 0),
            DatagramPlan::SplitPerSubframe
        );

        // The boolean wrapper keeps the legacy 124-byte accounting.
        let legacy_both = MAX_SAFE_UDP_PAYLOAD - HEADER_LEN - MAC_TAG_LEN - 2 * ADPCM_FEC_BYTES;
        assert_eq!(
            plan_datagram(legacy_both, true, true, true),
            DatagramPlan::Single { n1: true, n2: true }
        );
        assert_eq!(
            plan_datagram(legacy_both + 1, true, true, true),
            DatagramPlan::Single {
                n1: true,
                n2: false
            }
        );

        // A lone N-2 field is carried rather than silently dropped.
        assert_eq!(
            plan_datagram_with_fec(pair_only, true, 0, ADPCM_FEC_BYTES),
            DatagramPlan::Single {
                n1: false,
                n2: true
            }
        );
    }

    #[test]
    fn test_pack_fec_history_keeps_legacy_slots_stable() {
        let mut hist = [[0u8; ADPCM_FEC_BYTES]; 4];
        for (i, frame) in hist.iter_mut().enumerate() {
            frame.fill(0x10 + i as u8);
        }
        let mut p1 = [0u8; ADPCM_FEC_BYTES * 2];
        let mut p2 = [0u8; ADPCM_FEC_BYTES * 2];

        // Legacy 1/2-subframe framing keeps one frame per field.
        let (n1, n2) = pack_fec_history(false, &hist, 4, &mut p1, &mut p2);
        assert_eq!((n1, n2), (ADPCM_FEC_BYTES, ADPCM_FEC_BYTES));
        assert_eq!(&p1[..ADPCM_FEC_BYTES], &hist[0][..]);
        assert_eq!(&p2[..ADPCM_FEC_BYTES], &hist[1][..]);

        // 10 ms paired framing interleaves the older half into the second
        // frame so the first frame of each field stays N-1/N-2.
        let (n1, n2) = pack_fec_history(true, &hist, 4, &mut p1, &mut p2);
        assert_eq!((n1, n2), (ADPCM_FEC_BYTES * 2, ADPCM_FEC_BYTES * 2));
        assert_eq!(&p1[..ADPCM_FEC_BYTES], &hist[0][..]);
        assert_eq!(&p1[ADPCM_FEC_BYTES..], &hist[2][..]);
        assert_eq!(&p2[..ADPCM_FEC_BYTES], &hist[1][..]);
        assert_eq!(&p2[ADPCM_FEC_BYTES..], &hist[3][..]);

        // Ramp-up at length 3: paired N-1 plus the legacy single N-2.
        let (n1, n2) = pack_fec_history(true, &hist, 3, &mut p1, &mut p2);
        assert_eq!((n1, n2), (ADPCM_FEC_BYTES * 2, ADPCM_FEC_BYTES));
        assert_eq!(&p1[ADPCM_FEC_BYTES..], &hist[2][..]);
        assert_eq!(&p2[..ADPCM_FEC_BYTES], &hist[1][..]);

        // Short histories (mode switch / stream start) stay legacy.
        assert_eq!(
            pack_fec_history(true, &hist, 2, &mut p1, &mut p2),
            (ADPCM_FEC_BYTES, ADPCM_FEC_BYTES)
        );
        assert_eq!(
            pack_fec_history(true, &hist, 1, &mut p1, &mut p2),
            (ADPCM_FEC_BYTES, 0)
        );
        assert_eq!(pack_fec_history(true, &hist, 0, &mut p1, &mut p2), (0, 0));
    }

    #[test]
    fn test_stability_bitrate_stepdown_at_critical_loss() {
        // The 2.4 GHz profile caps at 160 kbps without raising a user's
        // explicit 96 kbps choice.
        assert_eq!(stability_bitrate_kbps(96, 0), 96);
        assert_eq!(stability_bitrate_kbps(128, 0), 128);
        assert_eq!(stability_bitrate_kbps(160, 0), 160);
        assert_eq!(stability_bitrate_kbps(320, 0), 160);
        // Exactly 20% loss keeps the normal stability rung; above it, 96.
        assert_eq!(stability_bitrate_kbps(320, 200), 160);
        assert_eq!(stability_bitrate_kbps(320, 201), 96);
        assert_eq!(stability_bitrate_kbps(96, 900), 96);
    }

    #[test]
    fn test_capture_pacer_spreads_bursts_below_nominal_cadence() {
        // Smooth pacing is 85% of the nominal 2.5 ms per-subframe interval.
        assert_eq!(capture_min_gap_us(1, 0), 2_125);
        assert_eq!(capture_min_gap_us(2, 0), 4_250);
        assert_eq!(capture_min_gap_us(4, 0), 8_500);
        assert!(capture_min_gap_us(1, 0) < 2_500);
        assert!(capture_min_gap_us(2, 0) < 5_000);
        assert!(capture_min_gap_us(4, 0) < 10_000);

        // A normal 10-20 ms callback leaves at most 8 subframes queued and
        // stays on smooth pacing; only a genuine backlog (>20 ms) accelerates.
        assert_eq!(capture_min_gap_us(2, 8), 4_250);
        assert_eq!(capture_min_gap_us(2, 9), 2_200);
        assert_eq!(capture_min_gap_us(4, 9), 4_400);
        assert_eq!(capture_min_gap_us(1, 9), 1_100);
    }

    #[test]
    fn test_gaming_lock_requires_an_unmuted_client() {
        fn client(addr: SocketAddr, muted: bool, gaming_lock: bool) -> ConnectedClient {
            ConnectedClient {
                addr,
                last_seen: Instant::now(),
                is_usb: false,
                muted,
                fec_rate_permille: 0,
                bluetooth_delay_ms: 0,
                hardware_delay_ms: 0,
                client_volume: 100,
                gaming_lock,
            }
        }

        let mut map = HashMap::new();
        let a: SocketAddr = "127.0.0.1:50001".parse().unwrap();
        let b: SocketAddr = "127.0.0.1:50002".parse().unwrap();
        map.insert(a, client(a, false, false));
        map.insert(b, client(b, false, true));
        assert!(HostEngine::any_client_gaming_lock(&map));

        // Muting the only gaming client returns the session to normal framing.
        map.get_mut(&b).unwrap().muted = true;
        assert!(!HostEngine::any_client_gaming_lock(&map));

        // A gaming lock on any unmuted client enables tight framing again.
        map.get_mut(&b).unwrap().muted = false;
        map.get_mut(&b).unwrap().gaming_lock = false;
        map.get_mut(&a).unwrap().gaming_lock = true;
        assert!(HostEngine::any_client_gaming_lock(&map));
        map.get_mut(&a).unwrap().gaming_lock = false;
        assert!(!HostEngine::any_client_gaming_lock(&map));
    }

    #[test]
    fn test_same_ip_reconnect_evicts_stale_port() {
        fn client(addr: SocketAddr) -> ConnectedClient {
            ConnectedClient {
                addr,
                last_seen: Instant::now(),
                is_usb: false,
                muted: false,
                fec_rate_permille: 0,
                bluetooth_delay_ms: 0,
                hardware_delay_ms: 0,
                client_volume: 100,
                gaming_lock: false,
            }
        }

        let mut map = HashMap::new();
        let old: SocketAddr = "192.168.1.50:41000".parse().unwrap();
        let new: SocketAddr = "192.168.1.50:41001".parse().unwrap();
        let other: SocketAddr = "192.168.1.51:41000".parse().unwrap();
        map.insert(old, client(old));
        map.insert(other, client(other));

        HostEngine::evict_same_ip_stale_ports(&mut map, new);
        assert!(!map.contains_key(&old), "stale port must be evicted");
        assert!(map.contains_key(&other), "other devices must survive");

        // Loopback clients are independent test instances and must coexist.
        let lo1: SocketAddr = "127.0.0.1:50000".parse().unwrap();
        let lo2: SocketAddr = "127.0.0.1:50001".parse().unwrap();
        map.insert(lo1, client(lo1));
        map.insert(lo2, client(lo2));
        HostEngine::evict_same_ip_stale_ports(&mut map, lo2);
        assert!(map.contains_key(&lo1));
        assert!(map.contains_key(&lo2));
    }

    #[test]
    fn test_smart_auto_loss_governor_holds_then_steps_down() {
        let base = Instant::now();
        let mut governor = LossGovernor::new(base);

        // 5% loss arms compressed mode.
        assert_eq!(governor.update(50, base), 50);
        // The next 0% report cannot step back up while the hold is open.
        assert_eq!(governor.update(0, base + Duration::from_millis(500)), 50);
        assert_eq!(governor.update(0, base + Duration::from_millis(2_999)), 50);
        // The hold expires: first 25-permille step instead of a cliff to 0.
        assert_eq!(governor.update(0, base + Duration::from_millis(3_000)), 25);
        // Once below the 4.5% tier boundary the residual estimate no longer
        // changes the profile, so the next report snaps to the live value.
        assert_eq!(governor.update(0, base + Duration::from_millis(3_001)), 0);

        // A high-loss burst re-arms the window and walks Tier 2 -> Tier 1 ->
        // Tier 0 in 500 ms steps instead of a single 3.9x bitrate jump.
        let t = base + Duration::from_millis(3_500);
        assert_eq!(governor.update(130, t), 130);
        assert_eq!(governor.update(0, t + Duration::from_millis(2_900)), 130);
        assert_eq!(governor.update(0, t + Duration::from_millis(3_000)), 105);
        // Several clients may report inside one 500 ms interval; the decay
        // must not run once per call.
        assert_eq!(governor.update(0, t + Duration::from_millis(3_001)), 105);
        assert_eq!(governor.update(0, t + Duration::from_millis(3_400)), 105);
        assert_eq!(governor.update(0, t + Duration::from_millis(3_500)), 80);
        assert_eq!(governor.update(0, t + Duration::from_millis(4_000)), 55);
        assert_eq!(governor.update(0, t + Duration::from_millis(4_500)), 30);
        assert_eq!(governor.update(0, t + Duration::from_millis(4_501)), 0);

        // Sub-threshold noise never arms the hold and does not linger.
        let mut governor = LossGovernor::new(base);
        assert_eq!(governor.update(40, base), 40);
        assert_eq!(governor.update(0, base + Duration::from_millis(1)), 0);
    }

    #[test]
    fn test_warm_opus_lookahead_primes_first_packet() {
        use audioshare_protocol::opus_codec::{OpusStreamDecoder, TAG_OPUS_RLD};

        fn tone(freq: f32, start_sample: usize) -> [i16; SUBFRAME_STEREO_SAMPLES] {
            let mut pcm = [0i16; SUBFRAME_STEREO_SAMPLES];
            for i in 0..SUBFRAME_FRAMES {
                let t = ((start_sample + i) as f32) / (SAMPLE_RATE as f32);
                let sample = (t * freq * std::f32::consts::TAU).sin() * 0.25 * 32767.0;
                pcm[i * 2] = sample as i16;
                pcm[i * 2 + 1] = sample as i16;
            }
            pcm
        }

        let previous = tone(440.0, 0);
        let current = tone(1200.0, SUBFRAME_FRAMES);
        let mut encoder = Some(OpusStreamEncoder::new(128).expect("create Opus encoder"));

        // Tier 0 keeps the encoder primed with live lossless audio.
        warm_opus_lookahead(&mut encoder, &previous, 128);
        let mut payload = [0u8; MAX_OPUS_SUBFRAME_BYTES];
        let len = encoder
            .as_mut()
            .expect("warm encoder")
            .encode_subframe(&current, 128, &mut payload)
            .expect("encode first transmitted frame");
        assert_eq!(payload[0], TAG_OPUS_RLD);

        let mut decoder = OpusStreamDecoder::new().expect("create Opus decoder");
        let mut decoded = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        assert!(decoder.decode_subframe(&payload[..len], &mut decoded));

        let dot = |lhs: &[f32; SUBFRAME_STEREO_SAMPLES], rhs: &[i16; SUBFRAME_STEREO_SAMPLES]| {
            lhs.iter()
                .zip(rhs.iter())
                .map(|(a, b)| *a * (*b as f32 / 32768.0))
                .sum::<f32>()
        };
        let dot_previous = dot(&decoded, &previous);
        let dot_current = dot(&decoded, &current);
        assert!(
            dot_previous > 1.0,
            "warm packet was silent: dot_previous={dot_previous}"
        );
        assert!(
            dot_previous > dot_current * 2.0,
            "warm packet did not carry the previous frame: \
             dot_previous={dot_previous}, dot_current={dot_current}"
        );
    }

    #[test]
    fn test_smart_auto_keeps_opus_warm_without_compressing_healthy_link() {
        let config = HostConfig {
            bind_port: 0,
            use_test_tone: true,
            subframes_per_packet: 1,
            enable_smart_dtx: false,
        };
        let mut engine = HostEngine::start(config, vec![]).expect("start host engine");
        engine
            .telemetry
            .quality_profile
            .store(QualityProfile::SmartAuto as u8, Ordering::Relaxed);
        engine.telemetry.prefer_opus.store(true, Ordering::Relaxed);

        // Wait until the TX loop has encoded at least one test-tone subframe,
        // then verify that the warm backend did not leave the lossless tier.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut vu = 0u8;
        while Instant::now() < deadline {
            vu = engine.telemetry.host_vu_peak_pct.load(Ordering::Relaxed);
            if vu > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(vu > 0, "host TX loop did not encode a test-tone subframe");
        assert_eq!(
            engine.telemetry.active_codec_mode.load(Ordering::Relaxed),
            CodecMode::StudioLosslessLpc as u8,
            "Smart Auto must stay lossless on a healthy link even with Opus warm"
        );
        engine.stop();
    }

    #[test]
    fn test_24bit_profile_uses_up_to_two_subframes_per_packet() {
        let config = HostConfig {
            bind_port: 0,
            use_test_tone: true,
            subframes_per_packet: 2,
            enable_smart_dtx: false,
        };
        let mut engine = HostEngine::start(config, vec![]).expect("start host engine");
        engine
            .telemetry
            .quality_profile
            .store(QualityProfile::StudioLossless24Bit as u8, Ordering::Relaxed);

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut observed = 0u8;
        while Instant::now() < deadline {
            observed = engine
                .telemetry
                .tx_subframes_per_packet
                .load(Ordering::Relaxed);
            if observed == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            observed, 2,
            "24-bit profile did not bundle two subframes per packet"
        );
        engine.stop();
    }

    #[test]
    fn test_max_stability_uses_four_subframe_10ms_frames() {
        let config = HostConfig {
            bind_port: 0,
            use_test_tone: true,
            subframes_per_packet: 1,
            enable_smart_dtx: false,
        };
        let mut engine = HostEngine::start(config, vec![]).expect("start host engine");
        engine
            .telemetry
            .quality_profile
            .store(QualityProfile::MaxStability24Ghz as u8, Ordering::Relaxed);

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut observed = 0u8;
        let mut interval_us = 0u32;
        while Instant::now() < deadline {
            observed = engine
                .telemetry
                .tx_subframes_per_packet
                .load(Ordering::Relaxed);
            interval_us = engine
                .telemetry
                .tx_packet_interval_us
                .load(Ordering::Relaxed);
            if observed == 4 && interval_us == 10_000 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            observed, 4,
            "Max Stability must bundle 4 subframes (10 ms) per packet"
        );
        assert_eq!(
            interval_us, 10_000,
            "Max Stability packet cadence must be 100 pps"
        );
        engine.stop();
    }
}
