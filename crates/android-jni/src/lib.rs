use std::collections::HashMap;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU16, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

#[cfg(target_os = "android")]
use audioshare_client_engine::android_aaudio::AAudioInputGuard;
use audioshare_client_engine::{ClientConfig, ClientEngine};
use audioshare_host_engine::discovery_and_os::{DiscoveryBeacon, DiscoveryScanner};
use audioshare_host_engine::virtual_mic::PHONE_MIC_PORT;
use audioshare_host_engine::{
    configure_realtime_udp_socket, list_valid_host_interfaces, promote_thread_realtime,
    DEFAULT_PORT,
};
use audioshare_protocol::crypto::SessionKey;
use audioshare_protocol::diagnostics;
use audioshare_protocol::dsp::BurstDecouplerRing;
use audioshare_protocol::latency::codec_mode_name;
use audioshare_protocol::lpc_lossless::{
    encode_fec_subframe, encode_lossless_subframe, encode_vbr_subframe, ADPCM_FEC_BYTES,
    MAX_LOSSLESS_SUBFRAME_BYTES,
};
use audioshare_protocol::opus_codec::{OpusRedHistory, OpusStreamEncoder};
use audioshare_protocol::wire::{
    pack_datagram_with_key, ClientFeedbackPacket, CodecMode, FeedbackReplayWindow, WireHeader,
    CONTROL_FEEDBACK_MAGIC,
};
use audioshare_protocol::{MAX_SAFE_UDP_PAYLOAD, SUBFRAME_STEREO_SAMPLES};

static GLOBAL_ANDROID_CLIENT: RwLock<Option<ClientEngine>> = RwLock::new(None);
static GLOBAL_PHONE_MIC_TX: RwLock<Option<PhoneMicTransmitter>> = RwLock::new(None);
static GLOBAL_SCANNER: OnceLock<DiscoveryScanner> = OnceLock::new();
/// User-selected Party codec profile (persisted by Android UI):
/// 0 = Opus RLD (5 ms, default stable), 1 = Studio Lossless 16-bit,
/// 2 = Max Stability Opus (10 ms), 3 = Pure-Rust VBR (0 ms).
static PARTY_DESIRED_CODEC_PROFILE: AtomicU8 = AtomicU8::new(0);
/// User-selected Party compressed bitrate in kbps (64..=320, default 256).
static PARTY_DESIRED_BITRATE_KBPS: AtomicU16 = AtomicU16::new(256);
/// Last Bluetooth downstream delay reported by Android AudioManager. Stored
/// globally so a route report that runs before `android_start_stream` still
/// seeds the new `ClientEngine` immediately.
static LAST_REPORTED_BT_DELAY_MS: AtomicU16 = AtomicU16::new(0);

pub fn get_or_start_scanner() -> &'static DiscoveryScanner {
    GLOBAL_SCANNER.get_or_init(DiscoveryScanner::start)
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartyCodecProfile {
    OpusRld = 0,
    StudioLossless = 1,
    MaxStabilityOpus10ms = 2,
    PureRustVbr = 3,
}

impl PartyCodecProfile {
    #[inline]
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::StudioLossless,
            2 => Self::MaxStabilityOpus10ms,
            3 => Self::PureRustVbr,
            _ => Self::OpusRld,
        }
    }
}

pub struct PartyHostTelemetry {
    pub listener_count: AtomicU8,
    pub active_codec_mode: AtomicU8,
    /// User-locked Party codec profile (`PartyCodecProfile as u8`).
    pub desired_codec_profile: AtomicU8,
    /// User-locked Party compressed bitrate in kbps.
    pub desired_bitrate_kbps: AtomicU16,
    /// Live capture peak (0..100) so the UI can warn about DRM/OEM silence.
    pub capture_vu_peak_pct: AtomicU8,
    /// Mute the cast stream at the capture source (service-owned Party mute).
    pub cast_muted: AtomicBool,
    pub max_group_delay_ms: AtomicU16,
    pub pending_remote_media_cmd: AtomicU8,
    pub bound_port: u16,
    pub rt_thread_tid: AtomicI32,
}

pub struct PhoneMicTransmitter {
    running: Arc<AtomicBool>,
    pub mic_input_ring: Arc<BurstDecouplerRing>,
    pub party_telemetry: Arc<PartyHostTelemetry>,
    _party_beacon: Option<DiscoveryBeacon>,
    handle: Option<std::thread::JoinHandle<()>>,
}

/// Anti-burst gaps for the phone-mic transmitter, mirroring the host capture
/// pacer. OS record callbacks often deliver 10-20 ms at once; transmitting that
/// backlog back-to-back overflows the phone's 2.4 GHz TX queue. Staying below
/// the 2.5 ms packet cadence keeps the record clock in control.
const PHONE_MIC_MIN_GAP_SINGLE_SUBFRAME_US: u64 = 800;
const PHONE_MIC_MIN_GAP_MULTI_SUBFRAME_US: u64 = 1_500;

#[inline]
fn phone_mic_min_gap_us(subframes_per_pkt: usize) -> u64 {
    if subframes_per_pkt >= 2 {
        PHONE_MIC_MIN_GAP_MULTI_SUBFRAME_US
    } else {
        PHONE_MIC_MIN_GAP_SINGLE_SUBFRAME_US
    }
}

/// Phone-mic backlog cap before old audio is discarded (50 ms).
const PHONE_MIC_MAX_BACKLOG_SUBFRAMES: usize = 20;
/// Headroom kept above one packet after an overflow trim.
const PHONE_MIC_TRIM_HEADROOM_SUBFRAMES: usize = 2;

/// Drop the oldest phone-mic audio when an OS scheduling stall has piled up
/// more than 50 ms. Without this the backlog becomes permanent mic latency.
fn trim_phone_mic_backlog(
    ring: &BurstDecouplerRing,
    subframes_per_pkt: usize,
    dither_rng: &mut u32,
) {
    let max_samples = SUBFRAME_STEREO_SAMPLES * PHONE_MIC_MAX_BACKLOG_SUBFRAMES;
    if ring.available_samples() <= max_samples {
        return;
    }
    let keep_samples =
        SUBFRAME_STEREO_SAMPLES * (subframes_per_pkt + PHONE_MIC_TRIM_HEADROOM_SUBFRAMES);
    let mut discard = [0.0f32; SUBFRAME_STEREO_SAMPLES];
    while ring.available_samples() > keep_samples {
        if !ring.pop_exact_or_dither(&mut discard, dither_rng) {
            break;
        }
    }
}

/// The legacy Android RNDIS host address. It is only meaningful while a USB
/// tethering interface actually exists; otherwise it is a dead unicast target
/// that would otherwise receive 400 packets per second on Wi-Fi.
const LEGACY_RNDIS_HOST: Ipv4Addr = Ipv4Addr::new(192, 168, 42, 129);

const PARTY_PEER_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
const PARTY_VBR_BITRATE_KBPS: u16 = 256;
const PARTY_DTX_SILENT_SUBFRAMES: u32 = 200;
const PARTY_DTX_HEARTBEAT_INTERVAL: u16 = 80;
/// Periodic AEAD session salt rotation interval so >71-minute encrypted Party
/// casts never wrap the 32-bit `tx_timestamp_us` nonce on the same salt.
const PARTY_SALT_ROTATION_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// Hard cap on distinct Party listener IPs. An unbounded unicast fan-out from a
/// phone host saturates its 2.4 GHz TX queue; 16 is the tested ceiling.
const MAX_PARTY_PEERS: usize = 16;

/// Resolve the user-locked Party transmission parameters:
/// `(wire_codec, prefer_opus, subframes_per_packet, bitrate_kbps)`.
///
/// Party Mode NEVER switches codecs or framing automatically on network loss
/// or peer count changes — doing so caused continuous audible jitter/crackles.
/// The codec, framing, and bitrate change ONLY when the user explicitly changes
/// them in the UI.
#[inline]
fn resolve_party_tx_params(
    profile: PartyCodecProfile,
    user_bitrate_kbps: u16,
) -> (CodecMode, bool, usize, u16) {
    match profile {
        PartyCodecProfile::OpusRld => (
            CodecMode::CompressedLowDelay,
            true,
            2,
            user_bitrate_kbps.clamp(96, 320),
        ),
        PartyCodecProfile::StudioLossless => (CodecMode::StudioLosslessLpc, false, 2, 256),
        PartyCodecProfile::MaxStabilityOpus10ms => (
            CodecMode::CompressedLowDelay,
            true,
            4,
            user_bitrate_kbps.clamp(64, 160),
        ),
        PartyCodecProfile::PureRustVbr => (
            CodecMode::CompressedLowDelay,
            false,
            2,
            user_bitrate_kbps.clamp(96, 320),
        ),
    }
}

/// Build the two FEC fields for one outgoing packet from the sliding history.
///
/// `hist[0]` is N-1 (newest). Normal framing carries one frame per field.
/// Four-subframe 10 ms framing interleaves two frames per field:
/// `fec_n1 = [N-1, N-3]`, `fec_n2 = [N-2, N-4]`. Keeping N-1/N-2 as the first
/// frame of each field is the compatibility contract with
/// `client-engine::heal_packet_fec`: older clients decode only those 124 bytes,
/// while newer clients read the second frame as the older half. A contiguous
/// `[N-1,N-2] + [N-3,N-4]` layout would heal the wrong slot on old clients.
///
/// `audioshare_protocol` does not export a history packer (host-engine keeps
/// its copy private), so mirror it here instead of adding a cross-crate API.
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
            // Ramp-up: N-2 alone stays the legacy field for one packet.
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

/// Shift the newest-first FEC history right and prepend `frame` as the new N-1.
#[inline]
fn push_fec_history(
    hist: &mut [[u8; ADPCM_FEC_BYTES]; 4],
    hist_len: &mut usize,
    frame: &[i16; SUBFRAME_STEREO_SAMPLES],
) {
    hist.copy_within(0..3, 1);
    encode_fec_subframe(frame, &mut hist[0]);
    *hist_len = (*hist_len + 1).min(hist.len());
}

/// Convert a dB value to the integer tenths stored in telemetry atomics.
/// A non-finite value from Java must never poison DSP state, and `.round()`
/// avoids the downward bias of a plain float-to-int cast.
#[inline]
fn db_to_tenths(db: f32, min: f32, max: f32) -> i32 {
    if !db.is_finite() {
        return 0;
    }
    (db.clamp(min, max) * 10.0).round() as i32
}

#[derive(Clone, Copy)]
struct PartyPeerState {
    last_seen: Instant,
    delay_ms: u16,
    #[cfg_attr(not(test), allow(dead_code))]
    fec_permille: u16,
    /// Anti-replay sliding window for authenticated Party feedback (AEAD v2).
    /// Unencrypted LAN feedback has no sequence guarantee, so the gate is a
    /// no-op there; with a session key, out-of-order packets inside the window
    /// are accepted exactly once.
    feedback_replay: FeedbackReplayWindow,
}

fn parse_target_endpoints_for_mode(
    target_hosts: &str,
    use_hardware_mic: bool,
    local_ips: &[IpAddr],
    has_usb_iface: bool,
) -> Vec<SocketAddr> {
    let mut targets = Vec::new();
    for item in target_hosts.split([',', ';', ' ']) {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        // Always deliver phone-mic audio to `PHONE_MIC_PORT` (65532). The UI
        // and discovery layer store `IP:65530` because 65530 is the control /
        // client-stream port; the PC's `PhoneMicReceiver` listens on 65532,
        // so preserving the user-visible port would send every hardware-mic
        // and party-cast stream to a port nobody reads. Parse a bare address
        // first so an IPv6 literal whose final hextet looks like a port is
        // never mistaken for an explicit port.
        let ip = if let Ok(ip) = item.parse::<IpAddr>() {
            ip
        } else if let Ok(explicit) = item.parse::<SocketAddr>() {
            explicit.ip()
        } else {
            continue;
        };

        // A unicast transmitter must never send to a broadcast, multicast, or
        // unspecified address. Loopback (`127.0.0.1` / `::1`) is only allowed
        // in hardware-mic mode, where it tunnels over `adb reverse tcp:65532`
        // to the PC's `PhoneMicReceiver` over USB.
        if ip.is_unspecified() || ip.is_multicast() {
            continue;
        }
        if ip.is_loopback() {
            if use_hardware_mic {
                let adb_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), PHONE_MIC_PORT);
                if !targets.contains(&adb_addr) {
                    targets.push(adb_addr);
                }
            }
            continue;
        }
        let addr = SocketAddr::new(ip, PHONE_MIC_PORT);
        if let IpAddr::V4(ipv4) = addr.ip() {
            if ipv4.is_broadcast() {
                continue;
            }
            if ipv4 == LEGACY_RNDIS_HOST && !has_usb_iface {
                continue;
            }
        }
        // The Party host must never register its own beacon or send audio back
        // to its own capture path.
        if local_ips.contains(&addr.ip()) {
            continue;
        }
        if !targets.contains(&addr) {
            targets.push(addr);
        }
    }
    targets
}

#[cfg_attr(not(test), allow(dead_code))]
fn parse_target_endpoints_with_locals(
    target_hosts: &str,
    local_ips: &[IpAddr],
    has_usb_iface: bool,
) -> Vec<SocketAddr> {
    parse_target_endpoints_for_mode(target_hosts, false, local_ips, has_usb_iface)
}

pub fn parse_target_endpoints(target_hosts: &str) -> Vec<SocketAddr> {
    let ifaces = list_valid_host_interfaces();
    let local_ips: Vec<IpAddr> = ifaces.iter().map(|iface| iface.ip).collect();
    let has_usb_iface = ifaces.iter().any(|iface| iface.is_usb_tethering);
    parse_target_endpoints_for_mode(target_hosts, false, &local_ips, has_usb_iface)
}

fn initial_party_peers(
    static_targets: &[SocketAddr],
    now: Instant,
) -> HashMap<SocketAddr, PartyPeerState> {
    let fresh = PartyPeerState {
        last_seen: now,
        delay_ms: 0,
        fec_permille: 0,
        feedback_replay: FeedbackReplayWindow::default(),
    };
    let mut peers = HashMap::new();
    for &addr in static_targets {
        peers.insert(addr, fresh);
    }
    // Peer phones and PC clients register themselves on their real source
    // port through HELLO/feedback. Blindly adding `IP:65530` for every
    // configured PC receiver counted one listener twice, forced the phone
    // into lossy VBR (`peer_count >= 2`) even for a single-PC call, and sent
    // a second unicast copy to a port that often had nobody listening.
    peers
}

/// Count distinct destination IPs rather than raw socket entries. One listener
/// can appear as both a configured endpoint (for example `IP:65532`) and a
/// dynamic HELLO/feedback source (`IP:65530`); that is still one listener for
/// codec-policy purposes, not two.
fn unique_peer_ip_count(peers: &HashMap<SocketAddr, PartyPeerState>) -> usize {
    let mut ips: Vec<IpAddr> = Vec::with_capacity(peers.len());
    for addr in peers.keys() {
        if !ips.contains(&addr.ip()) {
            ips.push(addr.ip());
        }
    }
    ips.len()
}

/// True when this IP is already registered on any port. At the peer cap an
/// existing IP may still reconnect on a new port or receive an update.
#[inline]
fn peer_ip_known(peers: &HashMap<SocketAddr, PartyPeerState>, ip: IpAddr) -> bool {
    peers.keys().any(|addr| addr.ip() == ip)
}

/// Replace any old dynamic port for this IP, but keep the configured PC
/// receiver endpoints. A peer that reconnects on a new ephemeral port must not
/// leave the old port receiving packets forever.
fn retain_static_and_current(
    peers: &mut HashMap<SocketAddr, PartyPeerState>,
    src: SocketAddr,
    static_targets: &[SocketAddr],
) {
    peers.retain(|addr, _| addr.ip() != src.ip() || *addr == src || static_targets.contains(addr));
}

fn register_party_peer(
    peers: &mut HashMap<SocketAddr, PartyPeerState>,
    src: SocketAddr,
    static_targets: &[SocketAddr],
    now: Instant,
) {
    // A HELLO from a new IP at the cap is dropped. An existing IP (including
    // this exact socket reconnecting on a new port) is always allowed through,
    // and a same-socket re-registration keeps its anti-replay watermark.
    if !peers.contains_key(&src)
        && !peer_ip_known(peers, src.ip())
        && unique_peer_ip_count(peers) >= MAX_PARTY_PEERS
    {
        return;
    }
    let replay = peers
        .get(&src)
        .map(|p| p.feedback_replay)
        .unwrap_or_default();
    retain_static_and_current(peers, src, static_targets);
    peers.insert(
        src,
        PartyPeerState {
            last_seen: now,
            delay_ms: 0,
            fec_permille: 0,
            feedback_replay: replay,
        },
    );
}

/// Apply one authenticated feedback update. Returns `false` when the packet is
/// a replay (or a new IP arrives at the peer cap), leaving state unchanged.
/// The replay window is checked and advanced in place here because a plain
/// `insert` would otherwise reset the watermark on every 500 ms feedback.
fn update_party_peer_feedback(
    peers: &mut HashMap<SocketAddr, PartyPeerState>,
    src: SocketAddr,
    delay_ms: u16,
    fec_permille: u16,
    feedback_seq: u32,
    authenticated: bool,
    static_targets: &[SocketAddr],
    now: Instant,
) -> bool {
    let mut replay = peers
        .get(&src)
        .map(|p| p.feedback_replay)
        .unwrap_or_default();
    if !replay.check_and_accept(authenticated, feedback_seq) {
        return false;
    }
    if !peers.contains_key(&src)
        && !peer_ip_known(peers, src.ip())
        && unique_peer_ip_count(peers) >= MAX_PARTY_PEERS
    {
        return false;
    }
    retain_static_and_current(peers, src, static_targets);
    peers.insert(
        src,
        PartyPeerState {
            last_seen: now,
            delay_ms,
            fec_permille,
            feedback_replay: replay,
        },
    );
    true
}

fn expire_party_peers(
    peers: &mut HashMap<SocketAddr, PartyPeerState>,
    static_targets: &[SocketAddr],
    now: Instant,
) {
    peers.retain(|addr, state| {
        static_targets.contains(addr)
            || now.saturating_duration_since(state.last_seen) < PARTY_PEER_TIMEOUT
    });
}

fn party_dtx_active(packet_is_silent: bool, silent_subframes: u32) -> bool {
    packet_is_silent && silent_subframes > PARTY_DTX_SILENT_SUBFRAMES
}

fn party_dtx_heartbeat_due(last: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last)
        >= Duration::from_micros(PARTY_DTX_HEARTBEAT_INTERVAL as u64 * 2_500)
}

fn party_beacon_port(use_hardware_mic: bool, bound_port: u16) -> Option<u16> {
    (!use_hardware_mic).then_some(bound_port)
}

/// Bind the transmitter socket.
///
/// Hardware-mic mode streams to the PC on `PHONE_MIC_PORT` (65532), so it has
/// no reason to own port 65530. That port belongs to `ClientEngine`, which
/// receives PC speaker audio there. Sharing it (especially with
/// `SO_REUSEPORT`) lets this transmitter drain or steal speaker packets and
/// forces the client onto a fallback port.
///
/// Party Broadcast mode is a host: peers send HELLO/feedback to the
/// well-known port 65530, so keep that binding when it is available.
fn bind_phone_mic_socket(use_hardware_mic: bool) -> std::io::Result<UdpSocket> {
    if use_hardware_mic {
        UdpSocket::bind("0.0.0.0:0")
    } else {
        UdpSocket::bind(("0.0.0.0", 65530)).or_else(|_| UdpSocket::bind("0.0.0.0:0"))
    }
}

impl PhoneMicTransmitter {
    pub fn start(target_hosts: &str, use_hardware_mic: bool) -> Result<Self, String> {
        Self::start_with_pin(target_hosts, use_hardware_mic, None)
    }

    pub fn start_with_pin(
        target_hosts: &str,
        use_hardware_mic: bool,
        pin: Option<&str>,
    ) -> Result<Self, String> {
        let ifaces = list_valid_host_interfaces();
        let local_ips: Vec<IpAddr> = ifaces.iter().map(|iface| iface.ip).collect();
        let has_usb_iface = ifaces.iter().any(|iface| iface.is_usb_tethering);
        let static_targets =
            parse_target_endpoints_for_mode(target_hosts, use_hardware_mic, &local_ips, has_usb_iface);
        // Hardware-mic mode streams to explicitly configured PC receivers. A
        // Party host is allowed to start with only its beacon: peer phones
        // register themselves by HELLO/feedback and never rely on broadcast.
        if use_hardware_mic && static_targets.is_empty() {
            return Err("No valid target endpoints found for transmitter".into());
        }

        let socket = bind_phone_mic_socket(use_hardware_mic).map_err(|e| e.to_string())?;
        let bound_port = socket
            .local_addr()
            .map_err(|e| format!("Failed to read party socket port: {}", e))?
            .port();
        let _ = socket.set_broadcast(true);
        let _ = socket.set_nonblocking(true);
        configure_realtime_udp_socket(&socket).map_err(|e| e.to_string())?;

        let running = Arc::new(AtomicBool::new(true));
        let mic_input_ring = Arc::new(BurstDecouplerRing::new());
        let initial_peers = initial_party_peers(&static_targets, Instant::now());
        let initial_profile = PARTY_DESIRED_CODEC_PROFILE.load(Ordering::Relaxed);
        let initial_bitrate = PARTY_DESIRED_BITRATE_KBPS.load(Ordering::Relaxed);
        let initial_mode = if use_hardware_mic {
            CodecMode::StudioLosslessLpc
        } else {
            let (codec, prefer_opus, _, _) =
                resolve_party_tx_params(PartyCodecProfile::from_u8(initial_profile), initial_bitrate);
            if prefer_opus {
                CodecMode::OpusRld
            } else {
                codec
            }
        };
        let party_telemetry = Arc::new(PartyHostTelemetry {
            listener_count: AtomicU8::new(unique_peer_ip_count(&initial_peers) as u8),
            active_codec_mode: AtomicU8::new(initial_mode as u8),
            desired_codec_profile: AtomicU8::new(initial_profile),
            desired_bitrate_kbps: AtomicU16::new(initial_bitrate),
            capture_vu_peak_pct: AtomicU8::new(0),
            cast_muted: AtomicBool::new(false),
            max_group_delay_ms: AtomicU16::new(0),
            pending_remote_media_cmd: AtomicU8::new(0),
            bound_port,
            rt_thread_tid: AtomicI32::new(0),
        });
        let mut session_key = pin
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(SessionKey::from_pin);
        // The PIN is applied at transmitter start on Android, so seed the
        // shared flag from the session key before launching the Party beacon.
        // There is no live PIN-apply path in this crate: a PIN change requires
        // restarting the transmitter (and therefore the beacon), so the shared
        // atomic is seeded once here and never flipped afterwards.
        let beacon_encrypted = Arc::new(AtomicBool::new(session_key.is_some()));

        // Party peers discover this phone through the beacon on the port the
        // transmitter actually bound. Advertising a hardcoded 65530 after a
        // fallback bind leaves every listener sending HELLO to a port nobody
        // reads.
        let _party_beacon = party_beacon_port(use_hardware_mic, bound_port).map(|bound_port| {
            DiscoveryBeacon::start_with_encryption_flag(
                bound_port,
                "Party: Android".into(),
                Arc::clone(&beacon_encrypted),
            )
        });

        let run_clone = Arc::clone(&running);
        let ring_clone = Arc::clone(&mic_input_ring);
        let party_telem_clone = Arc::clone(&party_telemetry);
        let mut local_ips: Vec<IpAddr> = list_valid_host_interfaces()
            .into_iter()
            .map(|iface| iface.ip)
            .collect();

        let handle = std::thread::Builder::new()
            .name("as-ultra-phone-mic-tx".into())
            .spawn(move || {
                promote_thread_realtime();
                #[cfg(target_os = "android")]
                unsafe {
                    party_telem_clone.rt_thread_tid.store(libc::gettid(), Ordering::Relaxed);
                }
                #[cfg(target_os = "android")]
                let mut mic_input_stream = if use_hardware_mic {
                    AAudioInputGuard::open_low_latency_mic(Arc::clone(&ring_clone))
                } else {
                    None
                };
                #[cfg(target_os = "android")]
                let mut last_mic_open_attempt = Instant::now();
                let start = Instant::now();
                let mut seq: u16 = 0;
                // Newest-first ADPCM FEC history: hist[0] = N-1 ... hist[3] = N-4.
                let mut fec_hist = [[0u8; ADPCM_FEC_BYTES]; 4];
                let mut fec_hist_len = 0usize;
                // Exact Opus RED ring ([P-1] / [P-2]) for `OpusRld` packets.
                // Self-describing blobs from `OpusRedHistory` ride in
                // `WireHeader::fec_n1` / `fec_n2`, so two consecutive dropped
                // 10 ms Opus packets heal with zero CELT PLC frames.
                let mut opus_red = OpusRedHistory::new();
                let mut dither_rng = 1u32;
                let mut noise_shaper =
                    audioshare_protocol::sync_and_eq::NoiseShapedQuantizer::new();
                // Party mode upgrades its compressed fallback to Opus RLD when
                // the encoder is available. Hardware-mic mode never compresses,
                // so skip the encoder allocation there. `opus_available` goes
                // false permanently if creation or encoding fails.
                let mut opus_available = false;
                let mut opus_encoder = if use_hardware_mic {
                    None
                } else {
                    let encoder = OpusStreamEncoder::new(
                        party_telem_clone
                            .desired_bitrate_kbps
                            .load(Ordering::Relaxed)
                            .clamp(64, 320),
                    );
                    opus_available = encoder.is_some();
                    encoder
                };
                // Retain the previous live subframe so a manual user switch
                // into Opus RLD mid-stream primes the 2.5 ms lookahead cleanly.
                let mut prev_f32_subframe = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                let mut has_prev_subframe = false;
                let mut ctrl_buf = [0u8; 64];
                let mut peers = initial_peers;
                // Per-peer authenticated feedback history lives in
                // `PartyPeerState::feedback_replay` so a feedback update
                // cannot reset the anti-replay watermark.
                let mut last_sweep = Instant::now();
                let mut last_iface_refresh = Instant::now();
                let mut last_salt_rotation = Instant::now();
                let mut silent_subframes: u32 = 0;
                // Keep 2.5 ms (400 pps) single-subframe mic pacing only on USB;
                // bundle 5.0 ms (200 pps) over Wi-Fi to avoid 802.11 TX saturation.
                // A Wi-Fi target at 400 pps burns airtime and starves the
                // client's receive path, so default to 200 pps unless every
                // configured endpoint is a known USB/RNDIS/ADB-loopback subnet.
                let all_targets_usb = !static_targets.is_empty()
                    && static_targets
                        .iter()
                        .all(|addr| audioshare_host_engine::is_usb_subnet_ip(addr.ip()));
                // USB hardware mic keeps the 2.5 ms (1 subframe) cadence;
                // Wi-Fi hardware mic and Party mode start at 5 ms (2 subframes)
                // and Party may grow to 10 ms dynamically below.
                let hw_mic_subframes: usize = if use_hardware_mic && all_targets_usb {
                    1
                } else {
                    2
                };
                let mut last_tx: Option<Instant> = None;
                let mut last_party_dtx_heartbeat = Instant::now();
                // Persistent TCP_NODELAY stream for USB ADB reverse (`127.0.0.1:65532`)
                // hardware-mic mode, using the exact 2-byte big-endian length-prefixed
                // framing expected by `AdbUsbWatcher::spawn_host_adb_tcp_session`.
                let mut adb_mic_tcp: Option<TcpStream> = None;
                let mut last_adb_tcp_attempt: Option<Instant> = None;
                let mut tcp_frame = [0u8; 2 + MAX_SAFE_UDP_PAYLOAD];

                while run_clone.load(Ordering::Relaxed) {
                    #[cfg(target_os = "android")]
                    if use_hardware_mic {
                        let disconnected = mic_input_stream
                            .as_ref()
                            .map(|s| s.needs_reopen())
                            .unwrap_or(false);
                        let retry_due = mic_input_stream.is_none()
                            && last_mic_open_attempt.elapsed() >= Duration::from_millis(500);
                        if disconnected || retry_due {
                            last_mic_open_attempt = Instant::now();
                            drop(mic_input_stream.take());
                            mic_input_stream =
                                AAudioInputGuard::open_low_latency_mic(Arc::clone(&ring_clone));
                        }
                    }

                    if !use_hardware_mic {
                        // Party Mode only: accept incoming HELLO and
                        // ClientFeedbackPacket from peer phones for
                        // Multi-Phone Group Sync. In hardware-mic mode this
                        // socket is ephemeral and port 65530 belongs to
                        // ClientEngine, so there is nothing to listen for.
                        while let Ok((len, src_addr)) = socket.recv_from(&mut ctrl_buf) {
                            // A phone that receives its own beacon/feedback
                            // must never register itself as a listener.
                            if local_ips.contains(&src_addr.ip()) || src_addr.ip().is_loopback() {
                                continue;
                            }
                            if len >= 8 && &ctrl_buf[..8] == b"ASULTRA1" {
                                // A bare HELLO carries no PIN/MAC. With a PIN
                                // active, only authenticated feedback may add a
                                // peer; configured static targets still work.
                                if session_key.is_some() {
                                    continue;
                                }
                                register_party_peer(
                                    &mut peers,
                                    src_addr,
                                    &static_targets,
                                    Instant::now(),
                                );
                            } else if len >= 8 && &ctrl_buf[..8] == CONTROL_FEEDBACK_MAGIC {
                                if let Some(fb) = ClientFeedbackPacket::decode_verified(
                                    &ctrl_buf[..len],
                                    session_key.as_ref(),
                                ) {
                                    // Align the fixed downstream delay (A2DP or
                                    // measured DAC), never the adaptive network
                                    // jitter buffer. `update_party_peer_feedback`
                                    // runs the replay gate and advances the
                                    // per-peer window in one atomic step.
                                    let accepted = update_party_peer_feedback(
                                        &mut peers,
                                        src_addr,
                                        fb.bluetooth_delay_ms.max(fb.hardware_delay_ms),
                                        fb.fec_rate_permille,
                                        fb.feedback_seq,
                                        session_key.is_some(),
                                        &static_targets,
                                        Instant::now(),
                                    );
                                    if accepted && (10..=12).contains(&fb.requested_mode) {
                                        party_telem_clone
                                            .pending_remote_media_cmd
                                            .store(fb.requested_mode, Ordering::Relaxed);
                                    }
                                }
                            }
                        }
                        // Peers send feedback every 500 ms. A peer that stopped
                        // sending is removed after 10 s so its old port and
                        // Bluetooth delay cannot pin the group-sync alignment
                        // forever. Configured PC endpoints stay.
                        if last_sweep.elapsed() >= Duration::from_secs(1) {
                            last_sweep = Instant::now();
                            expire_party_peers(&mut peers, &static_targets, last_sweep);
                        }
                        // Enumerating host interfaces walks the OS network
                        // stack. Keep the 1 s peer-expiry sweep but refresh the
                        // local-IP allowlist only every 10 s.
                        if last_iface_refresh.elapsed() >= Duration::from_secs(10) {
                            last_iface_refresh = Instant::now();
                            local_ips = list_valid_host_interfaces()
                                .into_iter()
                                .map(|iface| iface.ip)
                                .collect();
                        }
                        if last_salt_rotation.elapsed() >= PARTY_SALT_ROTATION_INTERVAL {
                            last_salt_rotation = Instant::now();
                            if let Some(ref mut key) = session_key {
                                key.rotate_session_salt();
                            }
                        }
                    }

                    // A single listener may be registered on two ports (a
                    // configured PC endpoint plus its dynamic HELLO source),
                    // so peer telemetry counts distinct listener IPs.
                    let listener_ips = unique_peer_ip_count(&peers);
                    party_telem_clone
                        .listener_count
                        .store(listener_ips.min(255) as u8, Ordering::Relaxed);
                    // User-locked Party codec/framing/bitrate: never auto-flip
                    // mid-stream on loss or peer fluctuations.
                    let desired_profile = PartyCodecProfile::from_u8(
                        party_telem_clone
                            .desired_codec_profile
                            .load(Ordering::Relaxed),
                    );
                    let desired_kbps = party_telem_clone
                        .desired_bitrate_kbps
                        .load(Ordering::Relaxed);
                    let (party_packet_codec, party_prefer_opus, party_subframes, party_kbps) =
                        resolve_party_tx_params(desired_profile, desired_kbps);
                    let subframes_per_pkt = if use_hardware_mic {
                        hw_mic_subframes
                    } else {
                        party_subframes
                    };
                    let min_tx_gap = Duration::from_micros(phone_mic_min_gap_us(subframes_per_pkt));

                    trim_phone_mic_backlog(&ring_clone, subframes_per_pkt, &mut dither_rng);

                    // Zero-peer fast path: keep the capture ring drained and
                    // publish a live VU meter for the DRM/OEM silence warning,
                    // but skip noise shaping, LPC/Opus, FEC and AEAD packing
                    // while nobody is listening.
                    if peers.is_empty() {
                        // The last listener may have just disconnected: clear
                        // the stale group-sync badge and reset DTX history so
                        // the next peer starts from a clean silence count.
                        party_telem_clone
                            .max_group_delay_ms
                            .store(0, Ordering::Relaxed);
                        let idle_mode = if use_hardware_mic {
                            CodecMode::StudioLosslessLpc
                        } else if party_prefer_opus {
                            CodecMode::OpusRld
                        } else {
                            party_packet_codec
                        };
                        party_telem_clone
                            .active_codec_mode
                            .store(idle_mode as u8, Ordering::Relaxed);
                        silent_subframes = 0;
                        has_prev_subframe = false;
                        prev_f32_subframe.fill(0.0);
                        if ring_clone.available_samples() >= SUBFRAME_STEREO_SAMPLES {
                            let mut f32_buf = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                            ring_clone.pop_exact_or_dither(&mut f32_buf, &mut dither_rng);
                            let peak = if party_telem_clone.cast_muted.load(Ordering::Relaxed) {
                                0.0
                            } else {
                                f32_buf.iter().fold(0.0f32, |m, s| m.max(s.abs()))
                            };
                            party_telem_clone.capture_vu_peak_pct.store(
                                (peak.clamp(0.0, 1.0).sqrt() * 100.0).min(100.0) as u8,
                                Ordering::Relaxed,
                            );
                            opus_encoder = None;
                            fec_hist_len = 0;
                            // No listener means no Opus chain to continue.
                            opus_red.clear();
                        } else {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        continue;
                    }

                    if ring_clone.available_samples() >= SUBFRAME_STEREO_SAMPLES * subframes_per_pkt
                    {
                        // Space catch-up packets. A 10-20 ms record burst
                        // otherwise leaves in one 2.4 GHz TXOP and overflows
                        // the phone's Wi-Fi queue.
                        if let Some(last) = last_tx {
                            let since_tx = last.elapsed();
                            if since_tx < min_tx_gap {
                                std::thread::sleep(min_tx_gap - since_tx);
                            }
                        }

                        let base_seq = seq;
                        let prev_hist = fec_hist;
                        let prev_hist_len = fec_hist_len;
                        let mut primary_bundle = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES * 4 + 8];
                        let mut cursor = 0usize;
                        let mut packet_is_silent = true;
                        // Collect the whole packet before choosing a codec. If
                        // Opus fails on the second subframe, rebuilding the
                        // first one as VBR keeps every datagram single-codec;
                        // the receiver never has to guess per subframe.
                        // Keep native f32 for Opus; quantize only for the VBR /
                        // lossless fallback. `cast_muted` zeroes the capture at
                        // the source so peers hear silence, not a faded stream.
                        let muted = party_telem_clone.cast_muted.load(Ordering::Relaxed);
                        let mut f32_frames = [[0.0f32; SUBFRAME_STEREO_SAMPLES]; 4];
                        let mut pcm_frames = [[0i16; SUBFRAME_STEREO_SAMPLES]; 4];
                        let mut pkt_peak = 0.0f32;
                        for idx in 0..subframes_per_pkt {
                            ring_clone.pop_exact_or_dither(&mut f32_frames[idx], &mut dither_rng);
                            if muted {
                                f32_frames[idx].fill(0.0);
                            }
                            for &s in &f32_frames[idx] {
                                pkt_peak = pkt_peak.max(s.abs());
                            }
                            // Quantize with high-pass TPDF dither and 0.85
                            // error feedback instead of plain rounding. The
                            // shaped noise sits below the microphone's own
                            // noise floor, while the old rounding correlated
                            // the error with the signal and added odd-harmonic
                            // distortion to quiet captures.
                            let max_abs = noise_shaper
                                .quantize_stereo_subframe(&f32_frames[idx], &mut pcm_frames[idx]);
                            if max_abs > 3 {
                                packet_is_silent = false;
                                silent_subframes = 0;
                            } else {
                                silent_subframes = silent_subframes.saturating_add(1);
                            }
                        }
                        party_telem_clone.capture_vu_peak_pct.store(
                            (pkt_peak.clamp(0.0, 1.0).sqrt() * 100.0).min(100.0) as u8,
                            Ordering::Relaxed,
                        );

                        seq = seq.wrapping_add(subframes_per_pkt as u16);

                        // After 500 ms of silence, keep only a 200 ms
                        // heartbeat. Evaluate DTX before touching the encoder
                        // or encoding any payload, so silence cycles never
                        // allocate an Opus encoder or run LPC/VBR + ADPCM FEC.
                        // Every heartbeat advances the sequence and wakes the
                        // receiver's jitter buffer; audio resumes on the next
                        // non-silent packet.
                        let dtx = !use_hardware_mic
                            && party_dtx_active(packet_is_silent, silent_subframes);
                        if dtx {
                            opus_encoder = None;
                            fec_hist_len = 0;
                            has_prev_subframe = false;
                            prev_f32_subframe.fill(0.0);
                            // Silence breaks the exact-Opus chain: the next
                            // real Opus packet starts with no prior blobs.
                            opus_red.clear();
                            let now = Instant::now();
                            if !party_dtx_heartbeat_due(last_party_dtx_heartbeat, now) {
                                // Nothing goes out: leave last_tx at the last
                                // real transmission so anti-burst pacing tracks
                                // actual sends, not silent skips.
                                continue;
                            }
                            last_party_dtx_heartbeat = now;
                        }

                        // Strictly honor the user-locked Party codec/bitrate so
                        // Party Mode never flips codecs mid-stream unless the
                        // user explicitly selects another codec. Hardware-mic
                        // mode always stays lossless for PC Virtual Mic.
                        let packet_codec = if use_hardware_mic {
                            CodecMode::StudioLosslessLpc
                        } else {
                            party_packet_codec
                        };
                        let target_kbps = party_kbps;
                        let use_compressed =
                            !use_hardware_mic && packet_codec == CodecMode::CompressedLowDelay;
                        let want_opus = use_compressed && party_prefer_opus;

                        // Opus is stateful and the receiver drops its decoder
                        // on every non-Opus frame. Mirror that here: drop the
                        // encoder when leaving Opus so a later user switch
                        // starts cleanly, and prime its 2.5 ms lookahead with
                        // `prev_f32_subframe` so manual switches don't click.
                        if !dtx {
                            if !want_opus {
                                opus_encoder = None;
                                // Leaving Opus invalidates the exact-Opus RED
                                // history; ADPCM FEC takes over again.
                                opus_red.clear();
                            } else if opus_encoder.is_none() && opus_available {
                                opus_encoder = OpusStreamEncoder::new(target_kbps);
                                opus_available = opus_encoder.is_some();
                                if let Some(enc_opus) = opus_encoder.as_mut() {
                                    if has_prev_subframe {
                                        let mut discard = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
                                        let _ = enc_opus.encode_subframe_f32(
                                            &prev_f32_subframe,
                                            target_kbps,
                                            &mut discard,
                                        );
                                    }
                                }
                            }
                        }

                        let mut codec_mode = packet_codec;
                        // Byte ranges of this packet's tagged Opus subframes
                        // inside `primary_bundle`, captured for the RED ring.
                        let mut opus_sf_ranges: [(usize, usize); 4] = [(0, 0); 4];
                        let mut opus_sf_count = 0usize;
                        if !dtx {
                            let mut enc = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
                            let mut opus_primary = want_opus && opus_encoder.is_some();
                            if opus_primary {
                                for f32_frame in f32_frames.iter().take(subframes_per_pkt) {
                                    let encoded = opus_encoder.as_mut().and_then(|enc_opus| {
                                        enc_opus.encode_subframe_f32(
                                            f32_frame,
                                            target_kbps,
                                            &mut enc,
                                        )
                                    });
                                    let Some(enc_len) = encoded else {
                                        // Opus failed: drop it for the rest of the
                                        // stream and rebuild this packet as VBR.
                                        opus_encoder = None;
                                        opus_available = false;
                                        opus_primary = false;
                                        opus_sf_count = 0;
                                        opus_red.clear();
                                        break;
                                    };
                                    let len_be = (enc_len as u16).to_be_bytes();
                                    primary_bundle[cursor] = len_be[0];
                                    primary_bundle[cursor + 1] = len_be[1];
                                    cursor += 2;
                                    primary_bundle[cursor..cursor + enc_len]
                                        .copy_from_slice(&enc[..enc_len]);
                                    opus_sf_ranges[opus_sf_count] = (cursor, enc_len);
                                    opus_sf_count += 1;
                                    cursor += enc_len;
                                }
                            }
                            if !opus_primary {
                                cursor = 0;
                                for frame in pcm_frames.iter().take(subframes_per_pkt) {
                                    let enc_len = if use_compressed {
                                        encode_vbr_subframe(frame, target_kbps, &mut enc)
                                    } else {
                                        encode_lossless_subframe(frame, &mut enc)
                                    };
                                    let len_be = (enc_len as u16).to_be_bytes();
                                    primary_bundle[cursor] = len_be[0];
                                    primary_bundle[cursor + 1] = len_be[1];
                                    cursor += 2;
                                    primary_bundle[cursor..cursor + enc_len]
                                        .copy_from_slice(&enc[..enc_len]);
                                    cursor += enc_len;
                                }
                            }
                            prev_f32_subframe = f32_frames[subframes_per_pkt - 1];
                            has_prev_subframe = true;
                            codec_mode = if opus_primary {
                                CodecMode::OpusRld
                            } else {
                                packet_codec
                            };

                            if opus_primary {
                                // A zero-lookahead ADPCM backup would jump the
                                // waveform by one subframe, so suppress it.
                                // The FEC slots instead carry the exact prior
                                // Opus packets (see `f1` / `f2` below).
                                fec_hist_len = 0;
                            } else {
                                for frame in pcm_frames.iter().take(subframes_per_pkt) {
                                    push_fec_history(&mut fec_hist, &mut fec_hist_len, frame);
                                }
                            }
                        }
                        // All `primary_bundle` writes are complete: borrow the
                        // tagged Opus subframes for the RED packer.
                        let mut opus_slices: [&[u8]; 4] = [&[]; 4];
                        for i in 0..opus_sf_count {
                            let (start, len) = opus_sf_ranges[i];
                            opus_slices[i] = &primary_bundle[start..start + len];
                        }
                        // Group-sync alignment only makes sense with at least
                        // two distinct listeners; a single listener's two
                        // ports must not enable it.
                        let max_peer_delay_ms = if listener_ips >= 2 {
                            peers.values().map(|p| p.delay_ms).max().unwrap_or(0)
                        } else {
                            0
                        };
                        party_telem_clone
                            .max_group_delay_ms
                            .store(max_peer_delay_ms, Ordering::Relaxed);
                        party_telem_clone.active_codec_mode.store(
                            if dtx {
                                CodecMode::SilenceHeartbeat as u8
                            } else {
                                codec_mode as u8
                            },
                            Ordering::Relaxed,
                        );
                        let hdr = WireHeader {
                            mode: if dtx {
                                CodecMode::SilenceHeartbeat
                            } else {
                                codec_mode
                            },
                            subframe_count: if dtx { 0 } else { subframes_per_pkt as u8 },
                            encrypted: session_key.is_some(),
                            sequence: base_seq,
                            tx_timestamp_us: start.elapsed().as_micros() as u32,
                            primary_len: 0,
                            fec_n1_len: 0,
                            fec_n2_len: 0,
                            host_volume: 100,
                            stream_id: (max_peer_delay_ms / 2).min(255) as u8,
                            header_crc16: 0,
                        };

                        let mut dgram = [0u8; MAX_SAFE_UDP_PAYLOAD];
                        let primary = if dtx {
                            &[][..]
                        } else {
                            &primary_bundle[..cursor]
                        };
                        // 10 ms / 4-subframe VBR framing carries the previous
                        // four subframes interleaved across the two fields:
                        // [N-1,N-3] / [N-2,N-4]. The first frame of each field
                        // keeps its legacy meaning for old clients.
                        let mut fec_pair_1 = [0u8; ADPCM_FEC_BYTES * 2];
                        let mut fec_pair_2 = [0u8; ADPCM_FEC_BYTES * 2];
                        let (n1_len, n2_len) = if dtx {
                            (0, 0)
                        } else {
                            let pair_fec =
                                subframes_per_pkt == 4 && codec_mode != CodecMode::OpusRld;
                            pack_fec_history(
                                pair_fec,
                                &prev_hist,
                                prev_hist_len,
                                &mut fec_pair_1,
                                &mut fec_pair_2,
                            )
                        };
                        // Opus RLD piggybacks the exact prior packets in the
                        // FEC slots; a zero-lookahead ADPCM backup would jump
                        // the waveform by one subframe. Non-Opus mode keeps
                        // the existing 4-slot ADPCM FEC history layout.
                        let (f1, f2): (&[u8], &[u8]) = if dtx {
                            (&[], &[])
                        } else if codec_mode == CodecMode::OpusRld {
                            opus_red.fec_slices()
                        } else {
                            (&fec_pair_1[..n1_len], &fec_pair_2[..n2_len])
                        };
                        let packed = pack_datagram_with_key(
                            hdr,
                            primary,
                            f1,
                            f2,
                            session_key.as_ref(),
                            &mut dgram,
                        )
                        .or_else(|_| {
                            // An oversized lossless burst + FEC would otherwise
                            // drop the whole datagram. Retry without any FEC
                            // backups (ADPCM or Opus RED) so loud audio still
                            // reaches listeners.
                            pack_datagram_with_key(
                                hdr,
                                primary,
                                &[],
                                &[],
                                session_key.as_ref(),
                                &mut dgram,
                            )
                        });
                        if let Ok(pkt_len) = packed {
                            for target in peers.keys() {
                                if use_hardware_mic && target.ip().is_loopback() {
                                    if adb_mic_tcp.is_none()
                                        && last_adb_tcp_attempt
                                            .map(|t| t.elapsed() >= Duration::from_secs(1))
                                            .unwrap_or(true)
                                    {
                                        last_adb_tcp_attempt = Some(Instant::now());
                                        if let Ok(stream) = TcpStream::connect_timeout(
                                            target,
                                            Duration::from_millis(250),
                                        ) {
                                            let _ = stream.set_nodelay(true);
                                            let _ = stream.set_write_timeout(Some(
                                                Duration::from_millis(250),
                                            ));
                                            adb_mic_tcp = Some(stream);
                                        }
                                    }
                                    if let Some(ref mut stream) = adb_mic_tcp {
                                        let len_be = (pkt_len as u16).to_be_bytes();
                                        tcp_frame[0] = len_be[0];
                                        tcp_frame[1] = len_be[1];
                                        tcp_frame[2..2 + pkt_len]
                                            .copy_from_slice(&dgram[..pkt_len]);
                                        if stream.write_all(&tcp_frame[..2 + pkt_len]).is_err() {
                                            adb_mic_tcp = None;
                                        }
                                    }
                                } else {
                                    let _ = socket.send_to(&dgram[..pkt_len], *target);
                                }
                            }
                            last_tx = Some(Instant::now());

                            // P becomes [P-1] for the next Opus packet and the
                            // old [P-1] becomes [P-2]. A bitrate spike that
                            // cannot fit in 255 B leaves [P-1] empty for one
                            // packet instead of dropping the whole datagram.
                            if !dtx && codec_mode == CodecMode::OpusRld {
                                opus_red.push_packet(base_seq, &opus_slices[..opus_sf_count]);
                            }
                        }
                    } else {
                        std::thread::sleep(Duration::from_micros(800));
                    }
                }
            })
            .map_err(|e| e.to_string())?;

        Ok(Self {
            running,
            mic_input_ring,
            party_telemetry,
            _party_beacon,
            handle: Some(handle),
        })
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        drop(self._party_beacon.take());
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for PhoneMicTransmitter {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn android_start_stream(
    host_ip_port: &str,
    is_usb: bool,
    gain_db: f32,
    pin: Option<&str>,
    dolby_effects: bool,
) -> Result<(), String> {
    let trimmed = host_ip_port.trim();
    let addr: SocketAddr = trimmed
        .parse::<SocketAddr>()
        .or_else(|_| {
            trimmed
                .parse::<IpAddr>()
                .map(|ip| SocketAddr::new(ip, DEFAULT_PORT))
        })
        .map_err(|e| {
            diagnostics::warn("session-start", format!("invalid host address: {e}"));
            format!("Invalid SocketAddr {}: {}", host_ip_port, e)
        })?;

    let existing = GLOBAL_ANDROID_CLIENT.write().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(mut existing) = existing {
        existing.stop();
    }

    // Kotlin can only report USB when the discovery row or caller knows it.
    // Android 11+ randomizes the RNDIS subnet, so also ask the host-engine
    // helper, which compares the address against local USB interfaces.
    let detected_usb = is_usb || audioshare_host_engine::is_usb_subnet_ip(addr.ip());
    let cfg = ClientConfig {
        server_addr: addr,
        target_buffer_subframes: if detected_usb { 2.0 } else { 3.5 },
        preamp_gain_db: gain_db,
        dolby_effects,
    };

    let client = ClientEngine::connect_with_pin(cfg, pin).map_err(|e| {
        diagnostics::error(
            "session-start",
            format!("connect failed to {}:{} ({})", addr.ip(), addr.port(), e),
        );
        e
    })?;
    let initial_bt_delay = LAST_REPORTED_BT_DELAY_MS.load(Ordering::Relaxed);
    if initial_bt_delay > 0 {
        client
            .telemetry
            .downstream_bt_delay_ms
            .store(initial_bt_delay, Ordering::Relaxed);
    }
    // Log the parsed host/port, never the raw input or PIN: the PIN is a
    // separate argument and must not end up in a pasteable diagnostics bundle.
    diagnostics::info(
        "session-start",
        format!(
            "host={} port={} codec={} usb={}",
            addr.ip(),
            addr.port(),
            codec_mode_name(client.telemetry.codec_mode.load(Ordering::Relaxed)),
            detected_usb,
        ),
    );
    *GLOBAL_ANDROID_CLIENT.write().unwrap_or_else(|e| e.into_inner()) = Some(client);
    Ok(())
}

pub fn android_stop_client_only() {
    let client = GLOBAL_ANDROID_CLIENT.write().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(mut client) = client {
        client.stop();
        // `stop()` consumes the running session but leaves `telemetry` intact.
        // Flatten the multi-line summary so the final counters survive in a
        // single event-log entry after the client has disconnected.
        diagnostics::info(
            "session-end",
            client
                .telemetry
                .diagnostics_summary(0.0)
                .replace('\n', " | "),
        );
    }
}

pub fn android_stop_stream() {
    android_stop_client_only();
    android_stop_phone_mic();
}

pub fn android_stop_phone_mic() {
    let mic = GLOBAL_PHONE_MIC_TX.write().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(mut mic) = mic {
        mic.stop();
    }
}

/// Mute/unmute the Party cast at the capture source. The service owns this so
/// a notification toggle keeps working after the app goes to the background.
pub fn android_set_party_cast_muted(muted: bool) {
    if let Some(ref tx) = *GLOBAL_PHONE_MIC_TX.read().unwrap_or_else(|e| e.into_inner()) {
        tx.party_telemetry.cast_muted.store(muted, Ordering::Relaxed);
    }
}

/// Set the user-locked Party codec profile and target bitrate. Updates both
/// global defaults (for future Party sessions) and any live Party transmitter.
pub fn android_set_party_codec_config(profile: u8, bitrate_kbps: u16) {
    let clamped_profile = PartyCodecProfile::from_u8(profile) as u8;
    let clamped_kbps = bitrate_kbps.clamp(64, 320);
    PARTY_DESIRED_CODEC_PROFILE.store(clamped_profile, Ordering::Relaxed);
    PARTY_DESIRED_BITRATE_KBPS.store(clamped_kbps, Ordering::Relaxed);
    if let Some(ref tx) = *GLOBAL_PHONE_MIC_TX.read().unwrap_or_else(|e| e.into_inner()) {
        tx.party_telemetry
            .desired_codec_profile
            .store(clamped_profile, Ordering::Relaxed);
        tx.party_telemetry
            .desired_bitrate_kbps
            .store(clamped_kbps, Ordering::Relaxed);
    }
}

pub fn android_set_gain_db(gain_db: f32) {
    if let Some(ref client) = *GLOBAL_ANDROID_CLIENT.read().unwrap_or_else(|e| e.into_inner()) {
        client
            .telemetry
            .preamp_gain_db_tenths
            .store(db_to_tenths(gain_db, -12.0, 24.0), Ordering::Relaxed);
    }
}

pub fn android_report_bt_delay_ms(delay_ms: u16) {
    LAST_REPORTED_BT_DELAY_MS.store(delay_ms, Ordering::Relaxed);
    if let Some(ref client) = *GLOBAL_ANDROID_CLIENT.read().unwrap_or_else(|e| e.into_inner()) {
        client
            .telemetry
            .downstream_bt_delay_ms
            .store(delay_ms, Ordering::Relaxed);
    }
}

pub fn android_send_media_command(cmd_code: u8) {
    if let Some(ref client) = *GLOBAL_ANDROID_CLIENT.read().unwrap_or_else(|e| e.into_inner()) {
        // Quality/media, compressed bitrate and codec-backend commands each
        // have their own slot so a startup restore cannot overwrite a command
        // that has not reached the host yet.
        if cmd_code == 20 || cmd_code == 21 {
            client
                .telemetry
                .pending_codec_backend_command
                .store(cmd_code, Ordering::Relaxed);
        } else if (30..=34).contains(&cmd_code) {
            client
                .telemetry
                .pending_bitrate_command
                .store(cmd_code, Ordering::Relaxed);
        } else {
            client
                .telemetry
                .pending_media_command
                .store(cmd_code, Ordering::Relaxed);
        }
    }
}

pub fn android_configure_dsp(
    bass_db: f32,
    pres_db: f32,
    treb_db: f32,
    role: u8,
    crossfeed: bool,
    night_mode: bool,
    gaming_lock: bool,
    dolby_effects: bool,
) {
    if let Some(ref client) = *GLOBAL_ANDROID_CLIENT.read().unwrap_or_else(|e| e.into_inner()) {
        client
            .telemetry
            .eq_bass_db_tenths
            .store(db_to_tenths(bass_db, -12.0, 12.0), Ordering::Relaxed);
        client
            .telemetry
            .eq_presence_db_tenths
            .store(db_to_tenths(pres_db, -12.0, 12.0), Ordering::Relaxed);
        client
            .telemetry
            .eq_treble_db_tenths
            .store(db_to_tenths(treb_db, -12.0, 12.0), Ordering::Relaxed);
        client.telemetry.channel_role.store(role, Ordering::Relaxed);
        client
            .telemetry
            .enable_binaural_crossfeed
            .store(crossfeed, Ordering::Relaxed);
        client
            .telemetry
            .enable_night_mode
            .store(night_mode, Ordering::Relaxed);
        client
            .telemetry
            .gaming_mode_hard_lock
            .store(gaming_lock, Ordering::Relaxed);
        client
            .telemetry
            .enable_dolby_effects
            .store(dolby_effects, Ordering::Relaxed);
    }
}

/// Returns newline-delimited list of auto-discovered hosts: "Hostname|IP:Port|USB_or_WiFi"
pub fn android_get_discovered_hosts_csv() -> String {
    let local_ips: Vec<IpAddr> = list_valid_host_interfaces()
        .into_iter()
        .map(|iface| iface.ip)
        .collect();
    let hosts = get_or_start_scanner().discovered_hosts();
    let mut out = String::new();
    for h in hosts {
        // Party hosts beacon their own label. Showing it in the listener list
        // would let the phone connect to itself and feed its own speaker
        // output back into the capture path.
        if local_ips.contains(&h.addr.ip()) {
            continue;
        }
        let mode = if h.is_usb { "USB" } else { "WiFi" };
        let enc = u8::from(h.encrypted);
        out.push_str(&format!("{}|{}|{}|{}\n", h.hostname, h.addr, mode, enc));
    }
    out
}

#[cfg(target_os = "android")]
pub mod android_native {
    use super::*;
    use jni::objects::{JClass, JString};
    use jni::sys::{jboolean, jfloat, jint, jintArray, jlong, jlongArray, jstring};
    use jni::JNIEnv;

    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeGetDiscoveredHosts(
        env: JNIEnv,
        _class: JClass,
    ) -> jstring {
        let csv = android_get_discovered_hosts_csv();
        match env.new_string(csv) {
            Ok(value) => value.into_raw(),
            Err(_) => std::ptr::null_mut(),
        }
    }

    /// Pause or resume UDP discovery scanning (e.g. while switching networks).
    /// Mirrors `nativeSetScannerPaused(paused: Boolean)` on the Kotlin side.
    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeSetScannerPaused(
        _env: JNIEnv,
        _class: JClass,
        paused: jboolean,
    ) {
        get_or_start_scanner().set_paused(paused != 0);
    }

    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeStartClient(
        mut env: JNIEnv,
        _class: JClass,
        host_addr: JString,
        is_usb: jboolean,
        gain_db: jfloat,
        pin: JString,
        dolby_effects: jboolean,
    ) -> jboolean {
        let Ok(addr_str) = env.get_string(&host_addr) else {
            return 0;
        };
        let addr_rust: String = addr_str.into();
        let pin_rust: String = env
            .get_string(&pin)
            .map(|value| value.into())
            .unwrap_or_default();
        let pin_trimmed = pin_rust.trim();
        let pin_opt = if pin_trimmed.is_empty() {
            None
        } else {
            Some(pin_trimmed)
        };
        match android_start_stream(
            &addr_rust,
            is_usb != 0,
            gain_db,
            pin_opt,
            dolby_effects != 0,
        ) {
            Ok(()) => 1,
            Err(_) => 0,
        }
    }

    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeStopClient(
        _env: JNIEnv,
        _class: JClass,
    ) {
        android_stop_client_only();
    }

    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeSetGainDb(
        _env: JNIEnv,
        _class: JClass,
        gain_db: jfloat,
    ) {
        android_set_gain_db(gain_db);
    }

    /// Service-owned "Mute Cast" action for the Party Host notification.
    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeSetPartyCastMuted(
        _env: JNIEnv,
        _class: JClass,
        muted: jboolean,
    ) {
        android_set_party_cast_muted(muted != 0);
    }

    /// User-locked Party codec profile and bitrate configuration.
    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeSetPartyCodecConfig(
        _env: JNIEnv,
        _class: JClass,
        codec_profile: jint,
        bitrate_kbps: jint,
    ) {
        android_set_party_codec_config(
            codec_profile.clamp(0, 3) as u8,
            bitrate_kbps.clamp(64, 320) as u16,
        );
    }

    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeReportBluetoothDelayMs(
        _env: JNIEnv,
        _class: JClass,
        delay_ms: jint,
    ) {
        android_report_bt_delay_ms(delay_ms.max(0) as u16);
    }

    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeSendMediaCommand(
        _env: JNIEnv,
        _class: JClass,
        cmd_code: jint,
    ) {
        android_send_media_command(cmd_code as u8);
    }

    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeConfigureDsp(
        _env: JNIEnv,
        _class: JClass,
        bass_db: jfloat,
        pres_db: jfloat,
        treb_db: jfloat,
        role: jint,
        crossfeed: jboolean,
        night_mode: jboolean,
        gaming_lock: jboolean,
        dolby_effects: jboolean,
    ) {
        android_configure_dsp(
            bass_db,
            pres_db,
            treb_db,
            role as u8,
            crossfeed != 0,
            night_mode != 0,
            gaming_lock != 0,
            dolby_effects != 0,
        );
    }

    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativePushCapturedAudio(
        env: JNIEnv,
        _class: JClass,
        pcm_float: jni::objects::JFloatArray,
    ) {
        // A null Java array makes get_float_array_region fail and leaves a
        // pending exception. Bail out before the copy and clear it so the next
        // JNI call on this thread starts with a clean exception state.
        if pcm_float.is_null() {
            return;
        }
        let mut buf = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        if env.get_float_array_region(&pcm_float, 0, &mut buf).is_err() {
            let _ = env.exception_clear();
            return;
        }
        if let Some(ref tx) = *GLOBAL_PHONE_MIC_TX.read().unwrap_or_else(|e| e.into_inner()) {
            tx.mic_input_ring.push_samples(&buf);
        }
    }

    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeStartPhoneMic(
        mut env: JNIEnv,
        _class: JClass,
        host_ip: JString,
        use_hardware_mic: jboolean,
        pin: JString,
    ) -> jboolean {
        let Ok(ip_str) = env.get_string(&host_ip) else {
            return 0;
        };
        let ip_rust: String = ip_str.into();
        let pin_rust: String = env
            .get_string(&pin)
            .map(|v| v.into())
            .unwrap_or_default();
        let pin_trimmed = pin_rust.trim();
        let pin_opt = if pin_trimmed.is_empty() {
            None
        } else {
            Some(pin_trimmed)
        };
        android_stop_phone_mic();
        if let Ok(tx) = PhoneMicTransmitter::start_with_pin(&ip_rust, use_hardware_mic != 0, pin_opt) {
            *GLOBAL_PHONE_MIC_TX.write().unwrap_or_else(|e| e.into_inner()) = Some(tx);
            1
        } else {
            0
        }
    }

    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeStopPhoneMic(
        _env: JNIEnv,
        _class: JClass,
    ) {
        android_stop_phone_mic();
    }

    /// Return the kernel TIDs of the native real-time threads that are
    /// currently running (client RX/DSP first, then the phone-mic/Party
    /// transmitter). ADPF hint sessions must cover those threads, not the
    /// Android main thread that happens to call the JNI function.
    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeGetRealtimeTids(
        env: JNIEnv,
        _class: JClass,
    ) -> jintArray {
        let mut tids: Vec<jint> = Vec::with_capacity(2);
        if let Some(ref client) = *GLOBAL_ANDROID_CLIENT.read().unwrap_or_else(|e| e.into_inner()) {
            let tid = client.telemetry.rt_thread_tid.load(Ordering::Relaxed);
            if tid > 0 && !tids.contains(&tid) {
                tids.push(tid);
            }
        }
        if let Some(ref tx) = *GLOBAL_PHONE_MIC_TX.read().unwrap_or_else(|e| e.into_inner()) {
            let tid = tx.party_telemetry.rt_thread_tid.load(Ordering::Relaxed);
            if tid > 0 && !tids.contains(&tid) {
                tids.push(tid);
            }
        }
        let Ok(arr) = env.new_int_array(tids.len() as i32) else {
            return std::ptr::null_mut();
        };
        if !tids.is_empty() {
            let _ = env.set_int_array_region(&arr, 0, &tids);
        }
        arr.into_raw()
    }

    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeGetTelemetry(
        env: JNIEnv,
        _class: JClass,
    ) -> jlongArray {
        // Keep this array length and index order in sync with the Kotlin
        // `LongArray(25)` telemetry buffer: 0..10 legacy counters, 11..21 live
        // latency breakdown, 22..23 party host, 24 client output VU peak.
        let mut stats = [0i64; 25];
        if let Some(ref c) = *GLOBAL_ANDROID_CLIENT.read().unwrap_or_else(|e| e.into_inner()) {
            let latency = c.telemetry.latency_snapshot(0.0);
            stats[0] = c.telemetry.packets_received.load(Ordering::Relaxed) as i64;
            stats[1] = c.telemetry.primary_frames_played.load(Ordering::Relaxed) as i64;
            stats[2] = c.telemetry.fec_healed_frames.load(Ordering::Relaxed) as i64;
            stats[3] = c.telemetry.plc_extrapolated_frames.load(Ordering::Relaxed) as i64;
            stats[4] = c.telemetry.clock_drift_ppm.load(Ordering::Relaxed) as i64;
            stats[5] = c.telemetry.auth_failures.load(Ordering::Relaxed) as i64;
            stats[6] = c
                .telemetry
                .seconds_since_last_packet
                .load(Ordering::Relaxed) as i64;
            stats[7] = i64::from(c.telemetry.server_dtx_silent.load(Ordering::Relaxed));
            stats[8] = c.telemetry.host_volume.load(Ordering::Relaxed) as i64;
            stats[9] = c.telemetry.max_group_delay_ms.load(Ordering::Relaxed) as i64;
            stats[10] = c.telemetry.downstream_hw_delay_ms.load(Ordering::Relaxed) as i64;
            stats[11] = c.telemetry.packet_interval_us.load(Ordering::Relaxed) as i64;
            stats[12] = c.telemetry.network_jitter_us.load(Ordering::Relaxed) as i64;
            stats[13] = c.telemetry.adaptive_buffer_us.load(Ordering::Relaxed) as i64;
            stats[14] = c.telemetry.target_buffer_us.load(Ordering::Relaxed) as i64;
            stats[15] = c.telemetry.buffered_audio_us.load(Ordering::Relaxed) as i64;
            stats[16] = c.telemetry.group_sync_extra_us.load(Ordering::Relaxed) as i64;
            stats[17] = c.telemetry.downstream_bt_delay_ms.load(Ordering::Relaxed) as i64;
            stats[18] = i64::from(c.telemetry.prebuffer_ready.load(Ordering::Relaxed));
            stats[19] = c.telemetry.codec_mode.load(Ordering::Relaxed) as i64;
            stats[20] = (latency.estimated_one_way_ms() * 10.0).round() as i64;
            stats[21] = (latency.codec_lookahead_ms * 10.0).round() as i64;
            stats[24] = c.telemetry.client_vu_peak_pct.load(Ordering::Relaxed) as i64;
        }
        if let Some(ref tx) = *GLOBAL_PHONE_MIC_TX.read().unwrap_or_else(|e| e.into_inner()) {
            // Pack Party Host telemetry into slots 22 and 23:
            // stats[22]: (capture_vu << 40) | (remote_cmd << 32) | (bound_port << 16)
            //            | (active_codec_mode << 8) | listener_count
            // stats[23]: party max_group_delay_ms
            let listeners = tx.party_telemetry.listener_count.load(Ordering::Relaxed) as i64;
            let mode = tx.party_telemetry.active_codec_mode.load(Ordering::Relaxed) as i64;
            let port = tx.party_telemetry.bound_port as i64;
            // Telemetry polling must be read-only: the UI calls this every
            // 60-250 ms for the VU meter, so consuming the pending command here
            // would steal peer media keys from partyMediaPollRunnable. The
            // dedicated nativeTakePartyMediaCommand() owns consumption.
            let remote_cmd = tx
                .party_telemetry
                .pending_remote_media_cmd
                .load(Ordering::Relaxed) as i64;
            let party_vu = tx
                .party_telemetry
                .capture_vu_peak_pct
                .load(Ordering::Relaxed) as i64;
            stats[22] =
                (party_vu << 40) | (remote_cmd << 32) | (port << 16) | (mode << 8) | listeners;
            stats[23] = tx
                .party_telemetry
                .max_group_delay_ms
                .load(Ordering::Relaxed) as i64;
        }
        let Ok(arr) = env.new_long_array(25) else {
            return std::ptr::null_mut();
        };
        let _ = env.set_long_array_region(&arr, 0, &stats);
        arr.into_raw()
    }

    /// Atomically consume the next pending remote media command (10..=12) for
    /// `AudioEngineService.partyMediaPollRunnable` without racing UI polling.
    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeTakePartyMediaCommand(
        _env: JNIEnv,
        _class: JClass,
    ) -> jint {
        if let Some(ref tx) = *GLOBAL_PHONE_MIC_TX.read().unwrap_or_else(|e| e.into_inner()) {
            tx.party_telemetry
                .pending_remote_media_cmd
                .swap(0, Ordering::Relaxed) as jint
        } else {
            0
        }
    }

    /// Return a pasteable diagnostics bundle (platform info + live telemetry +
    /// event log) from the Rust side.
    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeGetDiagnostics(
        env: JNIEnv,
        _class: JClass,
    ) -> jstring {
        let mut summary = String::from("Platform: Android JNI");
        if let Some(ref client) = *GLOBAL_ANDROID_CLIENT.read().unwrap_or_else(|e| e.into_inner()) {
            summary.push('\n');
            summary.push_str(&client.telemetry.diagnostics_summary(0.0));
        }
        if let Some(ref tx) = *GLOBAL_PHONE_MIC_TX.read().unwrap_or_else(|e| e.into_inner()) {
            let party = &tx.party_telemetry;
            summary.push('\n');
            summary.push_str(&format!(
                "Party Host: peers={} codec={} port={} sync={}ms muted={}",
                party.listener_count.load(Ordering::Relaxed),
                codec_mode_name(party.active_codec_mode.load(Ordering::Relaxed)),
                party.bound_port,
                party.max_group_delay_ms.load(Ordering::Relaxed),
                party.cast_muted.load(Ordering::Relaxed),
            ));
        }
        let report = diagnostics::export_report(&summary);
        match env.new_string(report) {
            Ok(value) => value.into_raw(),
            Err(_) => std::ptr::null_mut(),
        }
    }

    /// Return the current client authentication failure count so Android UI
    /// can prompt for a corrected host PIN on mismatch.
    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeGetAuthFailures(
        _env: JNIEnv,
        _class: JClass,
    ) -> jlong {
        if let Some(ref client) = *GLOBAL_ANDROID_CLIENT.read().unwrap_or_else(|e| e.into_inner()) {
            client.telemetry.auth_failures.load(Ordering::Relaxed) as jlong
        } else {
            0
        }
    }

    /// Bridge Kotlin-side state transitions into the shared ring-buffered
    /// diagnostics log. Mirrors
    /// `AudioEngineService.nativeLogEvent(level, component, message)`:
    /// level 0 = info, 1 = warn, anything else = error.
    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeLogEvent(
        mut env: JNIEnv,
        _class: JClass,
        level: jint,
        component: JString,
        message: JString,
    ) {
        let component = env
            .get_string(&component)
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|_| {
                // A failed GetStringUTFChars leaves a pending Java exception;
                // clear it before returning or the next JNI call inherits it.
                let _ = env.exception_clear();
                "kotlin".to_string()
            });
        let message = env
            .get_string(&message)
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|_| {
                let _ = env.exception_clear();
                String::new()
            });
        match level {
            0 => diagnostics::info(&component, &message),
            1 => diagnostics::warn(&component, &message),
            _ => diagnostics::error(&component, &message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audioshare_protocol::wire::pack_datagram;

    #[test]
    fn test_parse_target_endpoints_party_mode() {
        let input = "192.168.1.100, 192.168.1.101:65532; 10.0.0.5  192.168.1.100";
        let addrs = parse_target_endpoints_with_locals(input, &[], false);
        assert_eq!(addrs.len(), 3);
        assert_eq!(addrs[0], "192.168.1.100:65532".parse().unwrap());
        assert_eq!(addrs[1], "192.168.1.101:65532".parse().unwrap());
        assert_eq!(addrs[2], "10.0.0.5:65532".parse().unwrap());
    }

    #[test]
    fn test_party_targets_filter_self_broadcast_and_legacy_rndis() {
        let local = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50));
        let input = "192.168.1.50, 192.168.1.60, 127.0.0.1, 127.0.0.1:65530, [::1]:65530, \
                     255.255.255.255, 255.255.255.255:65530, 239.255.77.77, 0.0.0.0, \
                     0.0.0.0:65530, [ff02::1]:65530, 192.168.42.129:65530";
        let targets = parse_target_endpoints_with_locals(input, &[local], false);
        assert_eq!(
            targets,
            vec!["192.168.1.60:65532".parse().unwrap()],
            "self, loopback, broadcast, multicast, unspecified and dead RNDIS hosts must be dropped"
        );

        // A real USB tethering interface makes the legacy RNDIS target reachable.
        let targets = parse_target_endpoints_with_locals("192.168.42.129", &[local], true);
        assert_eq!(targets, vec!["192.168.42.129:65532".parse().unwrap()]);

        // Local IP filtering applies to explicit IPv6 endpoints as well.
        let local_v6: IpAddr = "fe80::1".parse().unwrap();
        assert!(
            parse_target_endpoints_with_locals("[fe80::1]:65530", &[local_v6], false).is_empty()
        );

        // Hardware-mic mode allows ADB USB loopback (`127.0.0.1:65530` ->
        // `127.0.0.1:65532`), while Party mode keeps rejecting loopback.
        let usb_mic_targets =
            parse_target_endpoints_for_mode("127.0.0.1:65530", true, &[local], false);
        assert_eq!(
            usb_mic_targets,
            vec!["127.0.0.1:65532".parse().unwrap()],
            "hardware mic mode must tunnel loopback targets over ADB port 65532"
        );
        assert!(
            parse_target_endpoints_for_mode("127.0.0.1:65530", false, &[local], false).is_empty(),
            "Party mode must continue rejecting loopback targets"
        );
    }

    #[test]
    fn test_parse_target_endpoints_normalizes_explicit_ports() {
        // The UI/discovery layer stores `IP:65530` (the control/client
        // stream port). Phone-mic audio must still go to the PC receiver on
        // 65532, so explicit ports are intentionally normalized rather than
        // preserved. IPv4 and bracketed IPv6 endpoints both work, and
        // duplicates are removed after normalization.
        let input = "192.168.1.50:65533, 192.168.1.51:65530, \
                     [fe80::1]:65530, 192.168.1.50:65533";
        let addrs = parse_target_endpoints_with_locals(input, &[], false);
        assert_eq!(
            addrs,
            vec![
                "192.168.1.50:65532".parse().unwrap(),
                "192.168.1.51:65532".parse().unwrap(),
                "[fe80::1]:65532".parse().unwrap(),
            ],
            "explicit ports must map to PHONE_MIC_PORT and dedupe"
        );
    }

    #[test]
    fn test_ui_default_control_port_maps_to_phone_mic_port() {
        // MainScreen's default host string and discovered endpoints always
        // carry the control port 65530. With a real USB interface the RNDIS
        // host is reachable, but the audio destination must still be 65532.
        let targets = parse_target_endpoints_with_locals("192.168.42.129:65530", &[], true);
        assert_eq!(targets, vec!["192.168.42.129:65532".parse().unwrap()]);
    }

    #[test]
    fn test_parse_target_endpoints_defaults_bare_ips_to_phone_mic_port() {
        let input = "192.168.1.50, fe80::1, 2001:db8::2";
        let addrs = parse_target_endpoints_with_locals(input, &[], false);
        assert_eq!(
            addrs,
            vec![
                "192.168.1.50:65532".parse().unwrap(),
                "[fe80::1]:65532".parse().unwrap(),
                "[2001:db8::2]:65532".parse().unwrap(),
            ],
            "bare IPv4 and IPv6 endpoints must default to PHONE_MIC_PORT"
        );
    }

    #[test]
    fn test_party_broadcast_default_target_is_rejected() {
        // `AudioEngineService` falls back to this Kotlin default when no
        // TARGET_HOSTS extra is present. It must never become a unicast
        // destination: 802.11 sends broadcast frames at the basic rate and
        // they would consume the whole channel's airtime.
        for has_usb_iface in [false, true] {
            assert!(
                parse_target_endpoints_with_locals("255.255.255.255:65530", &[], has_usb_iface)
                    .is_empty(),
                "the broadcast default must not produce a static target"
            );
        }
    }

    #[test]
    fn test_party_peer_lifecycle_dedupes_and_expires() {
        let now = Instant::now();
        let pc: SocketAddr = "192.168.1.60:65532".parse().unwrap();
        let static_targets = vec![pc];
        let mut peers = initial_party_peers(&static_targets, now);
        assert!(peers.contains_key(&pc));
        assert_eq!(
            peers.len(),
            1,
            "one configured PC receiver must count as one peer, not two ports"
        );

        // A PC client on the same IP registers dynamically on port 65530.
        let pc_client: SocketAddr = "192.168.1.60:65530".parse().unwrap();
        register_party_peer(&mut peers, pc_client, &static_targets, now);
        assert!(peers.contains_key(&pc_client));
        assert_eq!(peers.len(), 2);

        let phone_old: SocketAddr = "192.168.1.70:40000".parse().unwrap();
        let phone_new: SocketAddr = "192.168.1.70:40002".parse().unwrap();
        register_party_peer(&mut peers, phone_old, &static_targets, now);
        register_party_peer(&mut peers, phone_new, &static_targets, now);
        assert!(
            !peers.contains_key(&phone_old),
            "a reconnect on a new port must evict the stale port"
        );
        assert!(peers.contains_key(&phone_new));

        assert!(update_party_peer_feedback(
            &mut peers, phone_new, 120, 80, 7, true, &static_targets, now
        ));
        assert_eq!(peers[&phone_new].delay_ms, 120);
        assert_eq!(peers[&phone_new].fec_permille, 80);
        assert!(
            !update_party_peer_feedback(
                &mut peers, phone_new, 120, 80, 7, true, &static_targets, now
            ),
            "a duplicate authenticated sequence is a replay"
        );

        expire_party_peers(&mut peers, &static_targets, now + Duration::from_secs(11));
        assert!(
            peers.contains_key(&pc),
            "configured PC endpoints never expire"
        );
        assert!(
            !peers.contains_key(&phone_new),
            "a silent dynamic peer must expire"
        );
    }

    #[test]
    fn test_party_codec_is_user_locked_and_stable() {
        // Opus RLD default: locked at 2 sf (5 ms), user bitrate clamped 96..=320.
        assert_eq!(
            resolve_party_tx_params(PartyCodecProfile::OpusRld, 256),
            (CodecMode::CompressedLowDelay, true, 2, 256)
        );
        assert_eq!(
            resolve_party_tx_params(PartyCodecProfile::OpusRld, 320),
            (CodecMode::CompressedLowDelay, true, 2, 320)
        );
        assert_eq!(
            resolve_party_tx_params(PartyCodecProfile::OpusRld, 510),
            (CodecMode::CompressedLowDelay, true, 2, 320)
        );

        // Studio Lossless: locked on 16-bit lossless LPC, 2 sf.
        assert_eq!(
            resolve_party_tx_params(PartyCodecProfile::StudioLossless, 160),
            (CodecMode::StudioLosslessLpc, false, 2, 256)
        );

        // Max Stability Opus: locked at 4 sf (10 ms), bitrate clamped 64..=160
        // so 4-subframe exact RED blobs always fit inside 255 bytes.
        assert_eq!(
            resolve_party_tx_params(PartyCodecProfile::MaxStabilityOpus10ms, 256),
            (CodecMode::CompressedLowDelay, true, 4, 160)
        );
        assert_eq!(
            resolve_party_tx_params(PartyCodecProfile::MaxStabilityOpus10ms, 96),
            (CodecMode::CompressedLowDelay, true, 4, 96)
        );

        // Pure-Rust VBR: locked on VBR without Opus.
        assert_eq!(
            resolve_party_tx_params(PartyCodecProfile::PureRustVbr, 256),
            (CodecMode::CompressedLowDelay, false, 2, 256)
        );
    }

    #[test]
    fn test_unique_peer_ip_count_collapses_ports_per_listener() {
        let now = Instant::now();
        let pc_static: SocketAddr = "192.168.1.60:65532".parse().unwrap();
        let pc_dynamic: SocketAddr = "192.168.1.60:65530".parse().unwrap();
        let phone: SocketAddr = "192.168.1.70:40000".parse().unwrap();
        let static_targets = [pc_static];

        let mut peers = initial_party_peers(&static_targets, now);
        assert_eq!(unique_peer_ip_count(&peers), 1);

        // One PC registered on both its configured and dynamic ports is still
        // one listener, even though both socket entries remain for TX.
        register_party_peer(&mut peers, pc_dynamic, &static_targets, now);
        assert_eq!(peers.len(), 2, "both TX endpoints remain in the map");
        assert_eq!(
            unique_peer_ip_count(&peers),
            1,
            "one PC on two ports must count as one listener"
        );

        register_party_peer(&mut peers, phone, &static_targets, now);
        assert_eq!(
            unique_peer_ip_count(&peers),
            2,
            "a second IP must count as the second listener"
        );
    }

    #[test]
    fn test_party_dtx_threshold_and_heartbeat_cadence() {
        assert!(!party_dtx_active(false, 10_000));
        assert!(!party_dtx_active(true, PARTY_DTX_SILENT_SUBFRAMES));
        assert!(party_dtx_active(true, PARTY_DTX_SILENT_SUBFRAMES + 1));

        let start = Instant::now();
        // A sequence that starts odd can never hit a multiple of 160 when it
        // advances by two, so the old modulo gate goes silent forever after a
        // framing change. The wall-clock gate still fires every 200 ms.
        let mut seq = 101u16;
        for _ in 0..200 {
            assert_ne!(seq % 160, 0);
            seq = seq.wrapping_add(2);
        }
        assert!(!party_dtx_heartbeat_due(start, start + Duration::from_millis(199)));
        assert!(party_dtx_heartbeat_due(start, start + Duration::from_millis(200)));
    }

    #[test]
    fn test_party_beacon_advertises_bound_port() {
        assert_eq!(party_beacon_port(false, 49_152), Some(49_152));
        assert_eq!(
            party_beacon_port(true, 49_152),
            None,
            "hardware-mic mode must not advertise a Party beacon"
        );
    }

    #[test]
    fn test_party_vbr_256_saves_bytes_on_music() {
        let mut pcm = [0i16; SUBFRAME_STEREO_SAMPLES];
        for i in 0..(SUBFRAME_STEREO_SAMPLES / 2) {
            let t = i as f32 / 48_000.0;
            pcm[i * 2] = ((t * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
            pcm[i * 2 + 1] = ((t * 554.37 * std::f32::consts::TAU).sin() * 8000.0) as i16;
        }

        let mut vbr = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
        let mut lossless = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
        let vbr_len = encode_vbr_subframe(&pcm, PARTY_VBR_BITRATE_KBPS, &mut vbr);
        let lossless_len = encode_lossless_subframe(&pcm, &mut lossless);
        assert_eq!(vbr[0], audioshare_protocol::lpc_lossless::TAG_VBR_BFP);
        assert_eq!(vbr_len, 83, "256 kbps VBR subframe is fixed at 83 bytes");
        assert!(
            vbr_len < lossless_len,
            "VBR ({vbr_len} B) must beat lossless ({lossless_len} B) on music"
        );
    }

    #[test]
    fn test_party_opus_packet_suppresses_adpcm_fec_airtime() {
        let mut pcm = [0i16; SUBFRAME_STEREO_SAMPLES];
        for i in 0..(SUBFRAME_STEREO_SAMPLES / 2) {
            let t = i as f32 / 48_000.0;
            pcm[i * 2] = ((t * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
            pcm[i * 2 + 1] = ((t * 554.37 * std::f32::consts::TAU).sin() * 8000.0) as i16;
        }

        let mut encoder =
            OpusStreamEncoder::new(PARTY_VBR_BITRATE_KBPS).expect("create Opus encoder");
        let mut opus_bundle = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES * 2 + 4];
        let mut cursor = 0usize;
        for _ in 0..2 {
            let mut enc = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
            let len = encoder
                .encode_subframe(&pcm, PARTY_VBR_BITRATE_KBPS, &mut enc)
                .expect("encode Opus subframe");
            assert_eq!(enc[0], audioshare_protocol::opus_codec::TAG_OPUS_RLD);
            opus_bundle[cursor..cursor + 2].copy_from_slice(&(len as u16).to_be_bytes());
            cursor += 2;
            opus_bundle[cursor..cursor + len].copy_from_slice(&enc[..len]);
            cursor += len;
        }

        let mut vbr_bundle = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES * 2 + 4];
        let mut vbr_cursor = 0usize;
        for _ in 0..2 {
            let mut enc = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
            let len = encode_vbr_subframe(&pcm, PARTY_VBR_BITRATE_KBPS, &mut enc);
            vbr_bundle[vbr_cursor..vbr_cursor + 2].copy_from_slice(&(len as u16).to_be_bytes());
            vbr_cursor += 2;
            vbr_bundle[vbr_cursor..vbr_cursor + len].copy_from_slice(&enc[..len]);
            vbr_cursor += len;
        }

        let hdr = |mode| WireHeader {
            mode,
            subframe_count: 2,
            encrypted: false,
            sequence: 0,
            tx_timestamp_us: 0,
            primary_len: 0,
            fec_n1_len: 0,
            fec_n2_len: 0,
            host_volume: 100,
            stream_id: 0,
            header_crc16: 0,
        };
        let mut dgram = [0u8; MAX_SAFE_UDP_PAYLOAD];
        let opus_len = pack_datagram(
            hdr(CodecMode::OpusRld),
            &opus_bundle[..cursor],
            &[],
            &[],
            &mut dgram,
        )
        .expect("pack Opus packet");
        let vbr_no_fec = pack_datagram(
            hdr(CodecMode::CompressedLowDelay),
            &vbr_bundle[..vbr_cursor],
            &[],
            &[],
            &mut dgram,
        )
        .expect("pack VBR packet");
        let mut fec = [0u8; ADPCM_FEC_BYTES];
        encode_fec_subframe(&pcm, &mut fec);
        let vbr_with_fec = pack_datagram(
            hdr(CodecMode::CompressedLowDelay),
            &vbr_bundle[..vbr_cursor],
            &fec,
            &fec,
            &mut dgram,
        )
        .expect("pack VBR+FEC packet");

        assert_eq!(
            vbr_with_fec - vbr_no_fec,
            2 * ADPCM_FEC_BYTES,
            "each packet carries two 124-byte ADPCM backups"
        );
        // Suppressing FEC on Opus removes both backups (248 B), but Opus's
        // own framing costs ~60 B more than the fixed 256 kbps VBR rung here.
        // The net saving is still ~188 B per datagram, or ~300 kbps at the
        // 200 packets/s Party cadence.
        let net_saving = vbr_with_fec - opus_len;
        println!(
            "party packet: Opus {opus_len} B vs VBR+FEC {vbr_with_fec} B (net {net_saving} B)"
        );
        assert!(
            net_saving > 160,
            "Opus + FEC suppression must save >160 B/packet, got {net_saving}"
        );
    }

    #[test]
    fn test_client_volume_feedback_roundtrip() {
        use audioshare_protocol::wire::{ClientFeedbackPacket, CONTROL_FEEDBACK_LEN};
        let fb = ClientFeedbackPacket {
            fec_rate_permille: 45,
            bluetooth_delay_ms: 120,
            client_volume: 85,
            requested_mode: 0,
            hardware_delay_ms: 17,
            feedback_seq: 1,
            auth_tag: [0u8; 16],
        };
        let mut bytes = [0u8; CONTROL_FEEDBACK_LEN];
        fb.encode(&mut bytes);
        let decoded = ClientFeedbackPacket::decode(&bytes).expect("Failed to decode feedback");
        assert_eq!(decoded.client_volume, 85);
        assert_eq!(decoded.fec_rate_permille, 45);
        assert_eq!(decoded.bluetooth_delay_ms, 120);
        assert_eq!(decoded.hardware_delay_ms, 17);
    }

    #[test]
    fn test_hardware_mic_transmitter_uses_ephemeral_port() {
        let socket = bind_phone_mic_socket(true).expect("hardware-mic socket bind");
        assert_ne!(
            socket.local_addr().unwrap().port(),
            65530,
            "hardware-mic mode must not occupy ClientEngine's speaker port"
        );
    }

    #[test]
    fn test_phone_mic_gap_stays_below_packet_cadence() {
        assert_eq!(phone_mic_min_gap_us(1), 800);
        assert_eq!(phone_mic_min_gap_us(2), 1_500);
        assert_eq!(phone_mic_min_gap_us(4), 1_500);
        // The gap must never exceed the 2.5 ms capture cadence.
        assert!(phone_mic_min_gap_us(1) < 2_500);
        assert!(phone_mic_min_gap_us(2) < 5_000);
    }

    #[test]
    fn test_phone_mic_backlog_trim_discards_oldest_audio() {
        let ring = BurstDecouplerRing::new();
        let mut rng = 7u32;
        // 25 subframes tagged with their index so the trim boundary is visible.
        for subframe in 0..25u16 {
            let samples = [subframe as f32; SUBFRAME_STEREO_SAMPLES];
            assert_eq!(ring.push_samples(&samples), SUBFRAME_STEREO_SAMPLES);
        }

        trim_phone_mic_backlog(&ring, 1, &mut rng);
        // Keep one packet + two subframes of headroom.
        assert_eq!(
            ring.available_samples(),
            SUBFRAME_STEREO_SAMPLES * 3,
            "trim must keep exactly 7.5 ms"
        );
        let mut first = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        assert!(ring.pop_exact_or_dither(&mut first, &mut rng));
        assert_eq!(first[0], 22.0, "oldest audio must be discarded");

        // At or below the 20-subframe cap nothing is dropped.
        let ring = BurstDecouplerRing::new();
        for subframe in 0..20u16 {
            let samples = [subframe as f32; SUBFRAME_STEREO_SAMPLES];
            ring.push_samples(&samples);
        }
        trim_phone_mic_backlog(&ring, 2, &mut rng);
        assert_eq!(ring.available_samples(), SUBFRAME_STEREO_SAMPLES * 20);
    }

    #[test]
    fn test_party_transmitter_starts_with_zero_static_targets() {
        // The Kotlin service now passes an empty TARGET_HOSTS when the user
        // has not selected a PC. Party mode must still start: peers register
        // themselves through HELLO/feedback after discovery.
        let tx = PhoneMicTransmitter::start("", false);
        assert!(tx.is_ok());
        let mut tx = tx.unwrap();
        tx.stop();

        // The old broadcast fallback must stay harmless for older callers.
        let tx = PhoneMicTransmitter::start("255.255.255.255:65530", false);
        assert!(tx.is_ok());
        let mut tx = tx.unwrap();
        tx.stop();
    }

    #[test]
    fn test_hardware_mic_transmitter_rejects_zero_static_targets() {
        // Hardware-mic mode has no peer discovery, so it must not start
        // without at least one configured PC receiver.
        assert!(PhoneMicTransmitter::start("255.255.255.255:65530", true).is_err());
    }

    #[test]
    fn test_party_codec_config_updates_live_and_default_state() {
        android_set_party_codec_config(PartyCodecProfile::StudioLossless as u8, 320);
        assert_eq!(
            PARTY_DESIRED_CODEC_PROFILE.load(Ordering::Relaxed),
            PartyCodecProfile::StudioLossless as u8
        );
        assert_eq!(PARTY_DESIRED_BITRATE_KBPS.load(Ordering::Relaxed), 320);
        // Restore default Opus RLD 256 kbps so parallel tests see default state.
        android_set_party_codec_config(PartyCodecProfile::OpusRld as u8, 256);
    }

    #[test]
    fn test_party_host_telemetry_packed_slots() {
        let listeners: i64 = 3;
        let mode: i64 = CodecMode::CompressedLowDelay as i64;
        let port: i64 = 65530;
        let remote_cmd: i64 = 11; // KEYCODE_MEDIA_NEXT
        let party_vu: i64 = 71;
        let packed_slot22: i64 =
            (party_vu << 40) | (remote_cmd << 32) | (port << 16) | (mode << 8) | listeners;

        // Verify Kotlin bit unpacking logic
        let unpacked_vu = ((packed_slot22 as u64 >> 40) & 0xFF) as i64;
        let unpacked_cmd = ((packed_slot22 as u64 >> 32) & 0xFF) as i64;
        let unpacked_port = (packed_slot22 >> 16) & 0xFFFF;
        let unpacked_mode = (packed_slot22 >> 8) & 0xFF;
        let unpacked_listeners = packed_slot22 & 0xFF;

        assert_eq!(unpacked_vu, 71);
        assert_eq!(unpacked_cmd, 11);
        assert_eq!(unpacked_port, 65530);
        assert_eq!(unpacked_mode, CodecMode::CompressedLowDelay as i64);
        assert_eq!(unpacked_listeners, 3);
    }

    #[test]
    fn test_bare_ip_port_resolution() {
        let trimmed = "192.168.1.50";
        let addr = trimmed
            .parse::<SocketAddr>()
            .or_else(|_| {
                trimmed
                    .parse::<IpAddr>()
                    .map(|ip| SocketAddr::new(ip, DEFAULT_PORT))
            })
            .expect("bare IP must parse to DEFAULT_PORT");
        assert_eq!(addr, SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)), 65530));

        let trimmed_v6 = "fe80::1";
        let addr_v6 = trimmed_v6
            .parse::<SocketAddr>()
            .or_else(|_| {
                trimmed_v6
                    .parse::<IpAddr>()
                    .map(|ip| SocketAddr::new(ip, DEFAULT_PORT))
            })
            .expect("bare IPv6 must parse to DEFAULT_PORT");
        assert_eq!(addr_v6.port(), 65530);
    }

    #[test]
    fn test_party_transmitter_start_with_pin() {
        let tx = PhoneMicTransmitter::start_with_pin("", false, Some("1234"));
        assert!(tx.is_ok());
        let mut tx = tx.unwrap();
        assert_eq!(tx.party_telemetry.listener_count.load(Ordering::Relaxed), 0);
        tx.stop();
    }

    #[test]
    fn test_party_feedback_replay_window_accepts_once() {
        let now = Instant::now();
        let peer: SocketAddr = "192.168.1.70:40000".parse().unwrap();
        let static_targets: [SocketAddr; 0] = [];
        let mut peers = initial_party_peers(&static_targets, now);
        let mut fb = |seq, auth| {
            update_party_peer_feedback(&mut peers, peer, 120, 80, seq, auth, &static_targets, now)
        };

        // The first authenticated feedback from a peer is never a replay.
        assert!(fb(1, true));
        // A duplicate and sequence 0 are always rejected when authenticated.
        assert!(!fb(1, true));
        assert!(!fb(0, true));
        // Newer sequences advance the window.
        assert!(fb(2, true));
        assert!(!fb(2, true));
        // Out-of-order sequences inside the 64-bit window are accepted once.
        assert!(fb(4, true));
        assert!(fb(3, true));
        assert!(!fb(3, true));

        // Unencrypted feedback has no sequence guarantee: never rejected.
        assert!(fb(2, false));
        assert!(fb(0, false));
    }

    #[test]
    fn test_zero_peer_party_updates_capture_vu_without_crash() {
        let mut tx = PhoneMicTransmitter::start("", false).expect("start zero-peer Party host");
        assert_eq!(
            tx.party_telemetry
                .capture_vu_peak_pct
                .load(Ordering::Relaxed),
            0
        );
        let samples = [0.5f32; SUBFRAME_STEREO_SAMPLES];
        let deadline = Instant::now() + Duration::from_secs(3);
        while tx
            .party_telemetry
            .capture_vu_peak_pct
            .load(Ordering::Relaxed)
            == 0
            && Instant::now() < deadline
        {
            tx.mic_input_ring.push_samples(&samples);
            std::thread::sleep(Duration::from_millis(5));
        }
        let vu = tx
            .party_telemetry
            .capture_vu_peak_pct
            .load(Ordering::Relaxed);
        assert!(
            vu > 0,
            "the 0-peer fast path must publish a live capture VU"
        );
        assert!(vu <= 100);
        tx.stop();
    }

    #[test]
    fn test_party_cast_muted_silences_capture() {
        // One static peer keeps the transmitter on the normal encode path,
        // where cast_muted must zero the capture before VU publication.
        let mut tx = PhoneMicTransmitter::start("192.168.1.60", false)
            .expect("start Party host with one static peer");
        let samples = [0.5f32; SUBFRAME_STEREO_SAMPLES];
        tx.party_telemetry.cast_muted.store(true, Ordering::Relaxed);
        for _ in 0..20 {
            tx.mic_input_ring.push_samples(&samples);
        }
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(
            tx.party_telemetry
                .capture_vu_peak_pct
                .load(Ordering::Relaxed),
            0,
            "muted capture must publish silence"
        );

        // Unmute: the same feed must produce a non-zero VU.
        tx.party_telemetry
            .cast_muted
            .store(false, Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_secs(3);
        while tx
            .party_telemetry
            .capture_vu_peak_pct
            .load(Ordering::Relaxed)
            == 0
            && Instant::now() < deadline
        {
            tx.mic_input_ring.push_samples(&samples);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            tx.party_telemetry
                .capture_vu_peak_pct
                .load(Ordering::Relaxed)
                > 0,
            "unmuting must restore the live capture VU"
        );
        tx.stop();
    }

    #[test]
    fn test_interleaved_fec_history_pairs() {
        let mut hist = [[0u8; ADPCM_FEC_BYTES]; 4];
        for i in 0..4 {
            hist[i].fill(i as u8 + 1);
        }
        let mut p1 = [0u8; ADPCM_FEC_BYTES * 2];
        let mut p2 = [0u8; ADPCM_FEC_BYTES * 2];

        // Four-subframe VBR: [N-1,N-3] / [N-2,N-4], newest first in each field.
        assert_eq!(
            pack_fec_history(true, &hist, 4, &mut p1, &mut p2),
            (ADPCM_FEC_BYTES * 2, ADPCM_FEC_BYTES * 2)
        );
        assert_eq!(&p1[..ADPCM_FEC_BYTES], &hist[0][..]);
        assert_eq!(&p1[ADPCM_FEC_BYTES..], &hist[2][..]);
        assert_eq!(&p2[..ADPCM_FEC_BYTES], &hist[1][..]);
        assert_eq!(&p2[ADPCM_FEC_BYTES..], &hist[3][..]);

        // Ramp-up with three subframes: N-4 is not available yet.
        assert_eq!(
            pack_fec_history(true, &hist, 3, &mut p1, &mut p2),
            (ADPCM_FEC_BYTES * 2, ADPCM_FEC_BYTES)
        );

        // Two-subframe framing keeps the legacy one-frame-per-field layout
        // even when a full history happens to be present.
        assert_eq!(
            pack_fec_history(false, &hist, 4, &mut p1, &mut p2),
            (ADPCM_FEC_BYTES, ADPCM_FEC_BYTES)
        );
        assert_eq!(&p1[..ADPCM_FEC_BYTES], &hist[0][..]);
        assert_eq!(&p2[..ADPCM_FEC_BYTES], &hist[1][..]);

        // Pushing shifts the previous newest frame to the N-2 slot.
        let frame_a = [100i16; SUBFRAME_STEREO_SAMPLES];
        let frame_b = [200i16; SUBFRAME_STEREO_SAMPLES];
        let mut push_hist = [[0u8; ADPCM_FEC_BYTES]; 4];
        let mut len = 0;
        push_fec_history(&mut push_hist, &mut len, &frame_a);
        let after_a = push_hist[0];
        push_fec_history(&mut push_hist, &mut len, &frame_b);
        assert_eq!(push_hist[1], after_a, "previous newest must shift to N-2");
        assert_eq!(len, 2);
    }

    #[test]
    fn test_max_party_peers_cap_allows_existing_ip_updates() {
        let now = Instant::now();
        let static_targets: [SocketAddr; 0] = [];
        let mut peers = initial_party_peers(&static_targets, now);
        for i in 0..MAX_PARTY_PEERS {
            let addr: SocketAddr = format!("192.168.2.{}:40000", i + 1).parse().unwrap();
            register_party_peer(&mut peers, addr, &static_targets, now);
            assert!(peers.contains_key(&addr), "peer {i} should register");
        }
        assert_eq!(unique_peer_ip_count(&peers), MAX_PARTY_PEERS);

        // A 17th distinct IP is rejected from either registration path.
        let extra: SocketAddr = "192.168.2.200:40000".parse().unwrap();
        register_party_peer(&mut peers, extra, &static_targets, now);
        assert!(!peers.contains_key(&extra));
        let extra_fb: SocketAddr = "192.168.2.201:40000".parse().unwrap();
        assert!(!update_party_peer_feedback(
            &mut peers, extra_fb, 10, 20, 5, true, &static_targets, now
        ));
        assert!(!peers.contains_key(&extra_fb));
        assert_eq!(unique_peer_ip_count(&peers), MAX_PARTY_PEERS);

        // An existing IP may still reconnect on a new port and receive updates
        // while the map is at the cap.
        let reconnect: SocketAddr = "192.168.2.1:41000".parse().unwrap();
        register_party_peer(&mut peers, reconnect, &static_targets, now);
        assert!(peers.contains_key(&reconnect));
        assert!(!peers.contains_key(&"192.168.2.1:40000".parse().unwrap()));
        assert_eq!(unique_peer_ip_count(&peers), MAX_PARTY_PEERS);
        assert!(update_party_peer_feedback(
            &mut peers, reconnect, 30, 40, 1, true, &static_targets, now
        ));
    }

    #[test]
    fn test_db_to_tenths_rejects_nonfinite_and_rounds() {
        assert_eq!(db_to_tenths(f32::NAN, -24.0, 24.0), 0);
        assert_eq!(db_to_tenths(f32::INFINITY, -24.0, 24.0), 0);
        assert_eq!(db_to_tenths(f32::NEG_INFINITY, -24.0, 24.0), 0);
        // Half-way values round away from zero, not truncate.
        assert_eq!(db_to_tenths(1.25, -12.0, 24.0), 13);
        assert_eq!(db_to_tenths(1.24, -12.0, 24.0), 12);
        assert_eq!(db_to_tenths(-1.25, -12.0, 24.0), -13);
        // Out-of-range values clamp before scaling.
        assert_eq!(db_to_tenths(100.0, -12.0, 24.0), 240);
        assert_eq!(db_to_tenths(-100.0, -12.0, 24.0), -120);
    }
}
