use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU16, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
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
use audioshare_protocol::dsp::BurstDecouplerRing;
use audioshare_protocol::lpc_lossless::{
    encode_fec_subframe, encode_lossless_subframe, encode_vbr_subframe, ADPCM_FEC_BYTES,
    MAX_LOSSLESS_SUBFRAME_BYTES,
};
use audioshare_protocol::opus_codec::OpusStreamEncoder;
use audioshare_protocol::wire::{
    feedback_is_replay, pack_datagram_with_key, ClientFeedbackPacket, CodecMode, WireHeader,
    CONTROL_FEEDBACK_MAGIC,
};
use audioshare_protocol::{MAX_SAFE_UDP_PAYLOAD, SUBFRAME_STEREO_SAMPLES};

static GLOBAL_ANDROID_CLIENT: Mutex<Option<ClientEngine>> = Mutex::new(None);
static GLOBAL_PHONE_MIC_TX: Mutex<Option<PhoneMicTransmitter>> = Mutex::new(None);
static GLOBAL_SCANNER: OnceLock<DiscoveryScanner> = OnceLock::new();

pub fn get_or_start_scanner() -> &'static DiscoveryScanner {
    GLOBAL_SCANNER.get_or_init(DiscoveryScanner::start)
}

pub struct PartyHostTelemetry {
    pub listener_count: AtomicU8,
    pub active_codec_mode: AtomicU8,
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
const PARTY_VBR_LOSS_THRESHOLD_PERMILLE: u16 = 45;
const PARTY_HIGH_LOSS_THRESHOLD_PERMILLE: u16 = 120;
const PARTY_CRITICAL_LOSS_THRESHOLD_PERMILLE: u16 = 200;
const PARTY_LOSS_HOLD: Duration = Duration::from_secs(3);
const PARTY_VBR_BITRATE_KBPS: u16 = 256;
const PARTY_DTX_SILENT_SUBFRAMES: u32 = 200;
const PARTY_DTX_HEARTBEAT_INTERVAL: u16 = 80;

/// Select Party compressed bitrate based on peer fan-out and worst link loss.
/// Phone-to-phone traffic traverses the AP twice (uplink + downlink), so 3+
/// peers or heavy loss steps down from 256 kbps to 160 / 96 kbps to prevent
/// 802.11 TX queue saturation.
#[inline]
fn party_target_bitrate_kbps(listener_ips: usize, worst_loss_permille: u16) -> u16 {
    if worst_loss_permille > PARTY_CRITICAL_LOSS_THRESHOLD_PERMILLE {
        96
    } else if listener_ips >= 3 || worst_loss_permille > PARTY_HIGH_LOSS_THRESHOLD_PERMILLE {
        160
    } else {
        PARTY_VBR_BITRATE_KBPS
    }
}

/// Dynamically bundle 10 ms (4 subframes = 100 pps) instead of 5 ms when
/// serving 3+ peers or heavy Wi-Fi loss, halving 802.11 airtime contention.
#[inline]
fn party_subframes_per_packet(listener_ips: usize, worst_loss_permille: u16) -> usize {
    if listener_ips >= 3 || worst_loss_permille > PARTY_HIGH_LOSS_THRESHOLD_PERMILLE {
        4
    } else {
        2
    }
}

#[derive(Clone, Copy)]
struct PartyPeerState {
    last_seen: Instant,
    delay_ms: u16,
    fec_permille: u16,
    /// Highest authenticated feedback sequence seen from this peer (AEAD v2).
    last_feedback_seq: u32,
}

/// Replay gate for authenticated Party feedback. Unencrypted LAN feedback has
/// no sequence guarantee, so the check only applies with a session key.
#[inline]
fn party_feedback_is_replay(
    peers: &HashMap<SocketAddr, PartyPeerState>,
    src: SocketAddr,
    seq: u32,
    authenticated: bool,
) -> bool {
    let last = peers.get(&src).map(|p| p.last_feedback_seq).unwrap_or(0);
    feedback_is_replay(authenticated, seq, last)
}

fn parse_target_endpoints_with_locals(
    target_hosts: &str,
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
        let addr = SocketAddr::new(ip, PHONE_MIC_PORT);

        // A unicast phone-to-phone transmitter must never send to a broadcast,
        // multicast or unspecified address: 802.11 transmits those at the
        // mandatory basic rate, which can consume the entire channel airtime.
        if addr.ip().is_unspecified() || addr.ip().is_multicast() {
            continue;
        }
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

pub fn parse_target_endpoints(target_hosts: &str) -> Vec<SocketAddr> {
    let ifaces = list_valid_host_interfaces();
    let local_ips: Vec<IpAddr> = ifaces.iter().map(|iface| iface.ip).collect();
    let has_usb_iface = ifaces.iter().any(|iface| iface.is_usb_tethering);
    parse_target_endpoints_with_locals(target_hosts, &local_ips, has_usb_iface)
}

fn initial_party_peers(
    static_targets: &[SocketAddr],
    now: Instant,
) -> HashMap<SocketAddr, PartyPeerState> {
    let fresh = PartyPeerState {
        last_seen: now,
        delay_ms: 0,
        fec_permille: 0,
        last_feedback_seq: 0,
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
    retain_static_and_current(peers, src, static_targets);
    peers.insert(
        src,
        PartyPeerState {
            last_seen: now,
            delay_ms: 0,
            fec_permille: 0,
            last_feedback_seq: 0,
        },
    );
}

fn update_party_peer_feedback(
    peers: &mut HashMap<SocketAddr, PartyPeerState>,
    src: SocketAddr,
    delay_ms: u16,
    fec_permille: u16,
    feedback_seq: u32,
    static_targets: &[SocketAddr],
    now: Instant,
) {
    retain_static_and_current(peers, src, static_targets);
    peers.insert(
        src,
        PartyPeerState {
            last_seen: now,
            delay_ms,
            fec_permille,
            last_feedback_seq: feedback_seq,
        },
    );
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

fn party_codec_mode(peer_count: usize, worst_loss_permille: u16) -> CodecMode {
    if peer_count >= 2 || worst_loss_permille > PARTY_VBR_LOSS_THRESHOLD_PERMILLE {
        CodecMode::CompressedLowDelay
    } else {
        CodecMode::StudioLosslessLpc
    }
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
        let static_targets = parse_target_endpoints(target_hosts);
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
        let party_telemetry = Arc::new(PartyHostTelemetry {
            listener_count: AtomicU8::new(unique_peer_ip_count(&initial_peers) as u8),
            active_codec_mode: AtomicU8::new(CodecMode::StudioLosslessLpc as u8),
            capture_vu_peak_pct: AtomicU8::new(0),
            cast_muted: AtomicBool::new(false),
            max_group_delay_ms: AtomicU16::new(0),
            pending_remote_media_cmd: AtomicU8::new(0),
            bound_port,
            rt_thread_tid: AtomicI32::new(0),
        });
        let session_key = pin
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
                let mut fec_n1 = [0u8; ADPCM_FEC_BYTES];
                let mut fec_n2 = [0u8; ADPCM_FEC_BYTES];
                let mut has_fec_n1 = false;
                let mut has_fec_n2 = false;
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
                    let encoder = OpusStreamEncoder::new(PARTY_VBR_BITRATE_KBPS);
                    opus_available = encoder.is_some();
                    encoder
                };
                let mut ctrl_buf = [0u8; 64];
                let mut peers = initial_peers;
                // Per-peer highest authenticated feedback sequence lives in
                // `PartyPeerState::last_feedback_seq` so a feedback update
                // cannot reset the anti-replay watermark.
                let mut last_sweep = Instant::now();
                let mut governed_loss_permille: u16 = 0;
                let mut loss_hold_until = Instant::now();
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
                            if local_ips.contains(&src_addr.ip()) {
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
                                    if party_feedback_is_replay(
                                        &peers,
                                        src_addr,
                                        fb.feedback_seq,
                                        session_key.is_some(),
                                    ) {
                                        continue;
                                    }
                                    if (10..=12).contains(&fb.requested_mode) {
                                        party_telem_clone
                                            .pending_remote_media_cmd
                                            .store(fb.requested_mode, Ordering::Relaxed);
                                    }
                                    // Align the fixed downstream delay (A2DP or
                                    // measured DAC), never the adaptive network
                                    // jitter buffer.
                                    update_party_peer_feedback(
                                        &mut peers,
                                        src_addr,
                                        fb.bluetooth_delay_ms.max(fb.hardware_delay_ms),
                                        fb.fec_rate_permille,
                                        fb.feedback_seq,
                                        &static_targets,
                                        Instant::now(),
                                    );
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
                            local_ips = list_valid_host_interfaces()
                                .into_iter()
                                .map(|iface| iface.ip)
                                .collect();
                        }
                    }

                    let raw_worst_loss = peers.values().map(|p| p.fec_permille).max().unwrap_or(0);
                    let loop_now = Instant::now();
                    if raw_worst_loss > PARTY_VBR_LOSS_THRESHOLD_PERMILLE {
                        governed_loss_permille = governed_loss_permille.max(raw_worst_loss);
                        loss_hold_until = loop_now + PARTY_LOSS_HOLD;
                    } else if loop_now >= loss_hold_until {
                        governed_loss_permille = raw_worst_loss;
                    }
                    let worst_loss_permille = governed_loss_permille;
                    // A single listener may be registered on two ports (a
                    // configured PC endpoint plus its dynamic HELLO source),
                    // so codec policy must count distinct listener IPs.
                    let listener_ips = unique_peer_ip_count(&peers);
                    party_telem_clone
                        .listener_count
                        .store(listener_ips.min(255) as u8, Ordering::Relaxed);
                    let subframes_per_pkt = if use_hardware_mic {
                        hw_mic_subframes
                    } else {
                        party_subframes_per_packet(listener_ips, worst_loss_permille)
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
                        silent_subframes = 0;
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
                            has_fec_n1 = false;
                            has_fec_n2 = false;
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
                        let prev_n1 = fec_n1;
                        let prev_n2 = fec_n2;
                        let send_n1 = has_fec_n1;
                        let send_n2 = has_fec_n2;
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
                            has_fec_n1 = false;
                            has_fec_n2 = false;
                            let now = Instant::now();
                            if !party_dtx_heartbeat_due(last_party_dtx_heartbeat, now) {
                                // Nothing goes out: leave last_tx at the last
                                // real transmission so anti-burst pacing tracks
                                // actual sends, not silent skips.
                                continue;
                            }
                            last_party_dtx_heartbeat = now;
                        }

                        // A Party host feeding several phones cannot afford a
                        // lossless unicast copy per listener on 2.4 GHz. Step
                        // down to 256 kbps compressed when a second listener
                        // joins or when any listener reports more than 4.5%
                        // loss. Opus RLD replaces VBR when the encoder is
                        // available; VBR remains the fallback. Hardware-mic
                        // mode always stays lossless so the wire header and
                        // payload agree even with multiple PC targets.
                        let packet_codec = if use_hardware_mic {
                            CodecMode::StudioLosslessLpc
                        } else {
                            party_codec_mode(listener_ips, worst_loss_permille)
                        };
                        let target_kbps =
                            party_target_bitrate_kbps(listener_ips, worst_loss_permille);
                        let use_compressed =
                            !use_hardware_mic && packet_codec == CodecMode::CompressedLowDelay;

                        // Opus is stateful and the receiver drops its decoder
                        // on every non-Opus frame. Mirror that here: drop the
                        // encoder when leaving compressed mode so a later
                        // return starts from clean history, and recreate it
                        // only if creation has not already failed.
                        if !dtx {
                            if !use_compressed {
                                opus_encoder = None;
                            } else if opus_encoder.is_none() && opus_available {
                                opus_encoder = OpusStreamEncoder::new(target_kbps);
                                opus_available = opus_encoder.is_some();
                            }
                        }

                        let mut codec_mode = packet_codec;
                        if !dtx {
                            let mut enc = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
                            let mut opus_primary = use_compressed && opus_encoder.is_some();
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
                                        break;
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
                            codec_mode = if opus_primary {
                                CodecMode::OpusRld
                            } else {
                                packet_codec
                            };

                            if opus_primary {
                                // Receivers heal Opus gaps with the codec's own
                                // phase-aligned CELT PLC. A zero-lookahead ADPCM
                                // backup would jump the waveform by one subframe,
                                // so suppress it and save 248 bytes per packet.
                                has_fec_n1 = false;
                                has_fec_n2 = false;
                            } else {
                                for frame in pcm_frames.iter().take(subframes_per_pkt) {
                                    fec_n2 = fec_n1;
                                    has_fec_n2 = has_fec_n1;
                                    encode_fec_subframe(frame, &mut fec_n1);
                                    has_fec_n1 = true;
                                }
                            }
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
                        let f1 = if !dtx && send_n1 { &prev_n1[..] } else { &[] };
                        let f2 = if !dtx && send_n2 { &prev_n2[..] } else { &[] };
                        let packed = pack_datagram_with_key(
                            hdr,
                            primary,
                            f1,
                            f2,
                            session_key.as_ref(),
                            &mut dgram,
                        )
                        .or_else(|_| {
                            // Oversized lossless burst + FEC would otherwise
                            // drop the whole datagram. Retry without the ADPCM
                            // backups so loud audio still reaches listeners.
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
                                let _ = socket.send_to(&dgram[..pkt_len], *target);
                            }
                            last_tx = Some(Instant::now());
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
        .map_err(|e| format!("Invalid SocketAddr {}: {}", host_ip_port, e))?;

    let existing = GLOBAL_ANDROID_CLIENT.lock().unwrap_or_else(|e| e.into_inner()).take();
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

    let client = ClientEngine::connect_with_pin(cfg, pin)?;
    *GLOBAL_ANDROID_CLIENT.lock().unwrap_or_else(|e| e.into_inner()) = Some(client);
    Ok(())
}

pub fn android_stop_client_only() {
    let client = GLOBAL_ANDROID_CLIENT.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(mut client) = client {
        client.stop();
    }
}

pub fn android_stop_stream() {
    android_stop_client_only();
    android_stop_phone_mic();
}

pub fn android_stop_phone_mic() {
    let mic = GLOBAL_PHONE_MIC_TX.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(mut mic) = mic {
        mic.stop();
    }
}

/// Mute/unmute the Party cast at the capture source. The service owns this so
/// a notification toggle keeps working after the app goes to the background.
pub fn android_set_party_cast_muted(muted: bool) {
    if let Some(ref tx) = *GLOBAL_PHONE_MIC_TX.lock().unwrap_or_else(|e| e.into_inner()) {
        tx.party_telemetry.cast_muted.store(muted, Ordering::Relaxed);
    }
}

pub fn android_set_gain_db(gain_db: f32) {
    if let Some(ref client) = *GLOBAL_ANDROID_CLIENT.lock().unwrap_or_else(|e| e.into_inner()) {
        client
            .telemetry
            .preamp_gain_db_tenths
            .store((gain_db * 10.0) as i32, Ordering::Relaxed);
    }
}

pub fn android_report_bt_delay_ms(delay_ms: u16) {
    if let Some(ref client) = *GLOBAL_ANDROID_CLIENT.lock().unwrap_or_else(|e| e.into_inner()) {
        client
            .telemetry
            .downstream_bt_delay_ms
            .store(delay_ms, Ordering::Relaxed);
    }
}

pub fn android_send_media_command(cmd_code: u8) {
    if let Some(ref client) = *GLOBAL_ANDROID_CLIENT.lock().unwrap_or_else(|e| e.into_inner()) {
        // Quality/media, compressed bitrate and codec-backend commands each
        // have their own slot so a startup restore cannot overwrite a command
        // that has not reached the host yet.
        if cmd_code == 20 || cmd_code == 21 {
            client
                .telemetry
                .pending_codec_backend_command
                .store(cmd_code, Ordering::Relaxed);
        } else if (30..=33).contains(&cmd_code) {
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
    if let Some(ref client) = *GLOBAL_ANDROID_CLIENT.lock().unwrap_or_else(|e| e.into_inner()) {
        client
            .telemetry
            .eq_bass_db_tenths
            .store((bass_db * 10.0) as i32, Ordering::Relaxed);
        client
            .telemetry
            .eq_presence_db_tenths
            .store((pres_db * 10.0) as i32, Ordering::Relaxed);
        client
            .telemetry
            .eq_treble_db_tenths
            .store((treb_db * 10.0) as i32, Ordering::Relaxed);
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
    use jni::sys::{jboolean, jfloat, jint, jintArray, jlongArray, jstring};
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
        let mut buf = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        if env.get_float_array_region(&pcm_float, 0, &mut buf).is_ok() {
            if let Some(ref tx) = *GLOBAL_PHONE_MIC_TX.lock().unwrap_or_else(|e| e.into_inner()) {
                tx.mic_input_ring.push_samples(&buf);
            }
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
            *GLOBAL_PHONE_MIC_TX.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
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
        if let Some(ref client) = *GLOBAL_ANDROID_CLIENT.lock().unwrap_or_else(|e| e.into_inner()) {
            let tid = client.telemetry.rt_thread_tid.load(Ordering::Relaxed);
            if tid > 0 && !tids.contains(&tid) {
                tids.push(tid);
            }
        }
        if let Some(ref tx) = *GLOBAL_PHONE_MIC_TX.lock().unwrap_or_else(|e| e.into_inner()) {
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
        if let Some(ref c) = *GLOBAL_ANDROID_CLIENT.lock().unwrap_or_else(|e| e.into_inner()) {
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
        if let Some(ref tx) = *GLOBAL_PHONE_MIC_TX.lock().unwrap_or_else(|e| e.into_inner()) {
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
        if let Some(ref tx) = *GLOBAL_PHONE_MIC_TX.lock().unwrap_or_else(|e| e.into_inner()) {
            tx.party_telemetry
                .pending_remote_media_cmd
                .swap(0, Ordering::Relaxed) as jint
        } else {
            0
        }
    }

    /// Return a pasteable diagnostics bundle (platform info + event log).
    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeGetDiagnostics(
        env: JNIEnv,
        _class: JClass,
    ) -> jstring {
        let report = audioshare_protocol::diagnostics::export_report("Platform: Android JNI");
        match env.new_string(report) {
            Ok(value) => value.into_raw(),
            Err(_) => std::ptr::null_mut(),
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
        let input = "192.168.1.50, 192.168.1.60, 255.255.255.255, \
                     255.255.255.255:65530, 239.255.77.77, 0.0.0.0, \
                     0.0.0.0:65530, [ff02::1]:65530, 192.168.42.129:65530";
        let targets = parse_target_endpoints_with_locals(input, &[local], false);
        assert_eq!(
            targets,
            vec!["192.168.1.60:65532".parse().unwrap()],
            "self, broadcast, multicast, unspecified and dead RNDIS hosts must be dropped"
        );

        // A real USB tethering interface makes the legacy RNDIS target reachable.
        let targets = parse_target_endpoints_with_locals("192.168.42.129", &[local], true);
        assert_eq!(targets, vec!["192.168.42.129:65532".parse().unwrap()]);

        // Local IP filtering applies to explicit IPv6 endpoints as well.
        let local_v6: IpAddr = "fe80::1".parse().unwrap();
        assert!(
            parse_target_endpoints_with_locals("[fe80::1]:65530", &[local_v6], false).is_empty()
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

        update_party_peer_feedback(&mut peers, phone_new, 120, 80, 7, &static_targets, now);
        assert_eq!(peers[&phone_new].delay_ms, 120);
        assert_eq!(peers[&phone_new].fec_permille, 80);
        assert_eq!(peers[&phone_new].last_feedback_seq, 7);

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
    fn test_party_codec_vbr_engages_for_multi_peer_or_loss() {
        assert_eq!(party_codec_mode(0, 0), CodecMode::StudioLosslessLpc);
        assert_eq!(
            party_codec_mode(1, PARTY_VBR_LOSS_THRESHOLD_PERMILLE),
            CodecMode::StudioLosslessLpc
        );
        assert_eq!(
            party_codec_mode(1, PARTY_VBR_LOSS_THRESHOLD_PERMILLE + 1),
            CodecMode::CompressedLowDelay
        );
        assert_eq!(party_codec_mode(2, 0), CodecMode::CompressedLowDelay);
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
    fn test_party_codec_mode_uses_unique_listener_ips() {
        let now = Instant::now();
        let pc_static: SocketAddr = "192.168.1.60:65532".parse().unwrap();
        let pc_dynamic: SocketAddr = "192.168.1.60:65530".parse().unwrap();
        let phone: SocketAddr = "192.168.1.70:40000".parse().unwrap();
        let static_targets = [pc_static];

        // Static PC endpoint plus its dynamic HELLO source: one IP, two
        // ports. This must stay lossless, not step down to VBR.
        let mut peers = initial_party_peers(&static_targets, now);
        register_party_peer(&mut peers, pc_dynamic, &static_targets, now);
        let listener_ips = unique_peer_ip_count(&peers);
        assert_eq!(listener_ips, 1);
        assert_eq!(
            party_codec_mode(listener_ips, 0),
            CodecMode::StudioLosslessLpc
        );

        // Loss still forces VBR even with a single listener IP.
        assert_eq!(
            party_codec_mode(listener_ips, PARTY_VBR_LOSS_THRESHOLD_PERMILLE + 1),
            CodecMode::CompressedLowDelay
        );

        // A genuinely different IP is the second listener and forces VBR.
        register_party_peer(&mut peers, phone, &static_targets, now);
        let listener_ips = unique_peer_ip_count(&peers);
        assert_eq!(listener_ips, 2);
        assert_eq!(
            party_codec_mode(listener_ips, 0),
            CodecMode::CompressedLowDelay
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
    fn test_party_target_bitrate_adapts_to_peers_and_loss() {
        // 1-2 peers and low loss -> 256 kbps
        assert_eq!(party_target_bitrate_kbps(1, 0), 256);
        assert_eq!(party_target_bitrate_kbps(2, 45), 256);
        assert_eq!(party_target_bitrate_kbps(2, 120), 256);

        // 3+ peers -> 160 kbps (saves AP airtime)
        assert_eq!(party_target_bitrate_kbps(3, 0), 160);
        assert_eq!(party_target_bitrate_kbps(4, 50), 160);

        // High loss (>12%) -> 160 kbps
        assert_eq!(party_target_bitrate_kbps(1, 121), 160);

        // Critical loss (>20%) -> 96 kbps
        assert_eq!(party_target_bitrate_kbps(1, 201), 96);
        assert_eq!(party_target_bitrate_kbps(5, 250), 96);
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
    fn test_party_subframes_per_packet_thresholds() {
        // 1-2 peers and <=12% loss keep the 5 ms / 200 pps cadence.
        assert_eq!(party_subframes_per_packet(0, 0), 2);
        assert_eq!(party_subframes_per_packet(1, 0), 2);
        assert_eq!(
            party_subframes_per_packet(2, PARTY_HIGH_LOSS_THRESHOLD_PERMILLE),
            2
        );

        // 3+ peers -> 4 subframes (10 ms / 100 pps).
        assert_eq!(party_subframes_per_packet(3, 0), 4);
        assert_eq!(party_subframes_per_packet(5, 0), 4);

        // >12% loss -> 4 subframes even with a single listener.
        assert_eq!(
            party_subframes_per_packet(1, PARTY_HIGH_LOSS_THRESHOLD_PERMILLE + 1),
            4
        );
        assert_eq!(party_subframes_per_packet(2, 200), 4);
    }

    #[test]
    fn test_party_last_feedback_seq_replay_rejection() {
        let now = Instant::now();
        let peer: SocketAddr = "192.168.1.70:40000".parse().unwrap();
        let static_targets: [SocketAddr; 0] = [];
        let mut peers = initial_party_peers(&static_targets, now);

        // The first authenticated feedback from a peer is never a replay.
        assert!(!party_feedback_is_replay(&peers, peer, 1, true));
        update_party_peer_feedback(&mut peers, peer, 120, 80, 1, &static_targets, now);
        assert_eq!(peers[&peer].last_feedback_seq, 1);

        // Authenticated stale/duplicate sequences are rejected; newer accepted.
        assert!(party_feedback_is_replay(&peers, peer, 1, true));
        assert!(party_feedback_is_replay(&peers, peer, 0, true));
        assert!(!party_feedback_is_replay(&peers, peer, 2, true));
        update_party_peer_feedback(&mut peers, peer, 130, 90, 2, &static_targets, now);
        assert!(party_feedback_is_replay(&peers, peer, 2, true));
        assert!(!party_feedback_is_replay(&peers, peer, 3, true));

        // Unencrypted feedback has no sequence guarantee: never rejected.
        assert!(!party_feedback_is_replay(&peers, peer, 2, false));
        assert!(!party_feedback_is_replay(&peers, peer, 0, false));
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
}
