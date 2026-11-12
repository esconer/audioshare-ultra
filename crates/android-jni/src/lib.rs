use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[cfg(target_os = "android")]
use audioshare_client_engine::android_aaudio::AAudioInputGuard;
use audioshare_client_engine::{ClientConfig, ClientEngine};
use audioshare_host_engine::discovery_and_os::{DiscoveryBeacon, DiscoveryScanner};
use audioshare_host_engine::virtual_mic::PHONE_MIC_PORT;
use audioshare_host_engine::{
    configure_realtime_udp_socket, list_valid_host_interfaces, promote_thread_realtime,
};
use audioshare_protocol::dsp::BurstDecouplerRing;
use audioshare_protocol::lpc_lossless::{
    encode_fec_subframe, encode_lossless_subframe, encode_vbr_subframe, ADPCM_FEC_BYTES,
    MAX_LOSSLESS_SUBFRAME_BYTES,
};
use audioshare_protocol::wire::{
    pack_datagram, ClientFeedbackPacket, CodecMode, WireHeader, CONTROL_FEEDBACK_MAGIC,
};
use audioshare_protocol::{MAX_SAFE_UDP_PAYLOAD, SUBFRAME_STEREO_SAMPLES};

static GLOBAL_ANDROID_CLIENT: Mutex<Option<ClientEngine>> = Mutex::new(None);
static GLOBAL_PHONE_MIC_TX: Mutex<Option<PhoneMicTransmitter>> = Mutex::new(None);
static GLOBAL_SCANNER: OnceLock<DiscoveryScanner> = OnceLock::new();

pub fn get_or_start_scanner() -> &'static DiscoveryScanner {
    GLOBAL_SCANNER.get_or_init(DiscoveryScanner::start)
}

pub struct PhoneMicTransmitter {
    running: Arc<AtomicBool>,
    pub mic_input_ring: Arc<BurstDecouplerRing>,
    _party_beacon: Option<DiscoveryBeacon>,
    #[cfg(target_os = "android")]
    _mic_input_stream: Option<AAudioInputGuard>,
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

/// Peer phones register on the well-known Party port, while PC receivers use
/// `PHONE_MIC_PORT` (65532).
const PARTY_PHONE_PORT: u16 = 65530;
const PARTY_PEER_TIMEOUT: Duration = Duration::from_secs(10);
const PARTY_VBR_LOSS_THRESHOLD_PERMILLE: u16 = 45;
const PARTY_VBR_BITRATE_KBPS: u16 = 256;
const PARTY_DTX_SILENT_SUBFRAMES: u32 = 200;
const PARTY_DTX_HEARTBEAT_INTERVAL: u16 = 80;

#[derive(Clone, Copy)]
struct PartyPeerState {
    last_seen: Instant,
    delay_ms: u16,
    fec_permille: u16,
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
        let host_only = item.split(':').next().unwrap_or(item);
        let Ok(addr) = format!("{}:{}", host_only, PHONE_MIC_PORT).parse::<SocketAddr>() else {
            continue;
        };

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
    add_phone_peers: bool,
    now: Instant,
) -> HashMap<SocketAddr, PartyPeerState> {
    let fresh = PartyPeerState {
        last_seen: now,
        delay_ms: 0,
        fec_permille: 0,
    };
    let mut peers = HashMap::new();
    for &addr in static_targets {
        peers.insert(addr, fresh);
        if add_phone_peers {
            peers
                .entry(SocketAddr::new(addr.ip(), PARTY_PHONE_PORT))
                .or_insert(fresh);
        }
    }
    peers
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
        },
    );
}

fn update_party_peer_feedback(
    peers: &mut HashMap<SocketAddr, PartyPeerState>,
    src: SocketAddr,
    delay_ms: u16,
    fec_permille: u16,
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

fn party_dtx_heartbeat_due(base_seq: u16, subframes_per_pkt: usize) -> bool {
    // `base_seq` counts 2.5 ms subframes, not packets. Two subframes per
    // datagram means one heartbeat every 80 packets (200 ms).
    let interval_subframes = PARTY_DTX_HEARTBEAT_INTERVAL as usize * subframes_per_pkt.max(1);
    base_seq as usize % interval_subframes == 0
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
        let initial_peers = initial_party_peers(&static_targets, !use_hardware_mic, Instant::now());

        // Party peers discover this phone through the beacon on the port the
        // transmitter actually bound. Advertising a hardcoded 65530 after a
        // fallback bind leaves every listener sending HELLO to a port nobody
        // reads.
        let _party_beacon = party_beacon_port(use_hardware_mic, bound_port)
            .map(|bound_port| DiscoveryBeacon::start(bound_port, "Android-Party-Host".into()));

        #[cfg(target_os = "android")]
        let _mic_input_stream = if use_hardware_mic {
            AAudioInputGuard::open_low_latency_mic(Arc::clone(&mic_input_ring))
        } else {
            None
        };

        let run_clone = Arc::clone(&running);
        let ring_clone = Arc::clone(&mic_input_ring);
        let local_ips: Vec<IpAddr> = list_valid_host_interfaces()
            .into_iter()
            .map(|iface| iface.ip)
            .collect();

        let handle = std::thread::Builder::new()
            .name("as-ultra-phone-mic-tx".into())
            .spawn(move || {
                promote_thread_realtime();
                let start = Instant::now();
                let mut seq: u16 = 0;
                let mut fec_n1 = [0u8; ADPCM_FEC_BYTES];
                let mut fec_n2 = [0u8; ADPCM_FEC_BYTES];
                let mut has_fec_n1 = false;
                let mut has_fec_n2 = false;
                let mut dither_rng = 1u32;
                let mut ctrl_buf = [0u8; 64];
                let mut peers = initial_peers;
                let mut last_sweep = Instant::now();
                let mut silent_subframes: u32 = 0;
                let subframes_per_pkt: usize = if use_hardware_mic { 1 } else { 2 };
                let mut last_tx: Option<Instant> = None;
                let min_tx_gap = Duration::from_micros(phone_mic_min_gap_us(subframes_per_pkt));

                while run_clone.load(Ordering::Relaxed) {
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
                                register_party_peer(
                                    &mut peers,
                                    src_addr,
                                    &static_targets,
                                    Instant::now(),
                                );
                            } else if len >= 8 && &ctrl_buf[..8] == CONTROL_FEEDBACK_MAGIC {
                                if let Some(fb) = ClientFeedbackPacket::decode(&ctrl_buf[..len]) {
                                    // Align the fixed downstream delay (A2DP or
                                    // measured DAC), never the adaptive network
                                    // jitter buffer.
                                    update_party_peer_feedback(
                                        &mut peers,
                                        src_addr,
                                        fb.bluetooth_delay_ms.max(fb.hardware_delay_ms),
                                        fb.fec_rate_permille,
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
                        }
                    }

                    trim_phone_mic_backlog(&ring_clone, subframes_per_pkt, &mut dither_rng);

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
                        let mut primary_bundle = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES * 2 + 4];
                        let mut cursor = 0usize;
                        let mut packet_is_silent = true;
                        let worst_loss_permille =
                            peers.values().map(|p| p.fec_permille).max().unwrap_or(0);
                        // A Party host feeding several phones cannot afford a
                        // lossless unicast copy per listener on 2.4 GHz. Step
                        // down to 256 kbps VBR when a second listener joins or
                        // when any listener reports more than 4.5% loss.
                        let codec_mode = party_codec_mode(peers.len(), worst_loss_permille);
                        let use_compressed =
                            !use_hardware_mic && codec_mode == CodecMode::CompressedLowDelay;

                        for _ in 0..subframes_per_pkt {
                            let mut f32_buf = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                            ring_clone.pop_exact_or_dither(&mut f32_buf, &mut dither_rng);

                            let mut i16_buf = [0i16; SUBFRAME_STEREO_SAMPLES];
                            let mut max_abs = 0i16;
                            for (dst, &s) in i16_buf.iter_mut().zip(f32_buf.iter()) {
                                // Round rather than truncate toward zero: the
                                // latter doubles the zero-crossing quantization
                                // bin and adds odd-harmonic distortion to the
                                // phone-mic capture.
                                let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
                                *dst = v;
                                max_abs = max_abs.max(v.saturating_abs());
                            }
                            if max_abs > 3 {
                                packet_is_silent = false;
                                silent_subframes = 0;
                            } else {
                                silent_subframes = silent_subframes.saturating_add(1);
                            }

                            let mut enc = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
                            let enc_len = if use_compressed {
                                encode_vbr_subframe(&i16_buf, PARTY_VBR_BITRATE_KBPS, &mut enc)
                            } else {
                                encode_lossless_subframe(&i16_buf, &mut enc)
                            };
                            let len_be = (enc_len as u16).to_be_bytes();
                            primary_bundle[cursor] = len_be[0];
                            primary_bundle[cursor + 1] = len_be[1];
                            cursor += 2;
                            primary_bundle[cursor..cursor + enc_len]
                                .copy_from_slice(&enc[..enc_len]);
                            cursor += enc_len;

                            fec_n2 = fec_n1;
                            has_fec_n2 = has_fec_n1;
                            encode_fec_subframe(&i16_buf, &mut fec_n1);
                            has_fec_n1 = true;
                            seq = seq.wrapping_add(1);
                        }

                        // After 500 ms of silence, keep only a 200 ms
                        // heartbeat. Every heartbeat advances the sequence and
                        // wakes the receiver's jitter buffer; audio resumes on
                        // the next non-silent packet.
                        let dtx = !use_hardware_mic
                            && party_dtx_active(packet_is_silent, silent_subframes);
                        if dtx && !party_dtx_heartbeat_due(base_seq, subframes_per_pkt) {
                            last_tx = Some(Instant::now());
                            continue;
                        }
                        let max_peer_delay_ms = if peers.len() >= 2 {
                            peers.values().map(|p| p.delay_ms).max().unwrap_or(0)
                        } else {
                            0
                        };
                        let hdr = WireHeader {
                            mode: if dtx {
                                CodecMode::SilenceHeartbeat
                            } else {
                                codec_mode
                            },
                            subframe_count: if dtx { 0 } else { subframes_per_pkt as u8 },
                            encrypted: false,
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
                        if let Ok(pkt_len) = pack_datagram(hdr, primary, f1, f2, &mut dgram) {
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
            _party_beacon,
            #[cfg(target_os = "android")]
            _mic_input_stream,
            handle: Some(handle),
        })
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        drop(self._party_beacon.take());
        #[cfg(target_os = "android")]
        if let Some(stream) = self._mic_input_stream.take() {
            drop(stream);
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

pub fn android_start_stream(
    host_ip_port: &str,
    is_usb: bool,
    gain_db: f32,
    pin: Option<&str>,
    dolby_effects: bool,
) -> Result<(), String> {
    let addr: SocketAddr = host_ip_port
        .parse()
        .map_err(|e| format!("Invalid SocketAddr {}: {}", host_ip_port, e))?;

    let mut guard = GLOBAL_ANDROID_CLIENT.lock().unwrap();
    if let Some(mut existing) = guard.take() {
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
    *guard = Some(client);
    Ok(())
}

pub fn android_stop_stream() {
    if let Some(mut client) = GLOBAL_ANDROID_CLIENT.lock().unwrap().take() {
        client.stop();
    }
    android_stop_phone_mic();
}

pub fn android_stop_phone_mic() {
    if let Some(mut mic) = GLOBAL_PHONE_MIC_TX.lock().unwrap().take() {
        mic.stop();
    }
}

pub fn android_set_gain_db(gain_db: f32) {
    if let Some(ref client) = *GLOBAL_ANDROID_CLIENT.lock().unwrap() {
        client
            .telemetry
            .preamp_gain_db_tenths
            .store((gain_db * 10.0) as i32, Ordering::Relaxed);
    }
}

pub fn android_report_bt_delay_ms(delay_ms: u16) {
    if let Some(ref client) = *GLOBAL_ANDROID_CLIENT.lock().unwrap() {
        client
            .telemetry
            .downstream_bt_delay_ms
            .store(delay_ms, Ordering::Relaxed);
    }
}

pub fn android_send_media_command(cmd_code: u8) {
    if let Some(ref client) = *GLOBAL_ANDROID_CLIENT.lock().unwrap() {
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
    if let Some(ref client) = *GLOBAL_ANDROID_CLIENT.lock().unwrap() {
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
        out.push_str(&format!("{}|{}|{}\n", h.hostname, h.addr, mode));
    }
    out
}

#[cfg(target_os = "android")]
pub mod android_native {
    use super::*;
    use jni::objects::{JClass, JString};
    use jni::sys::{jboolean, jfloat, jint, jlongArray, jstring};
    use jni::JNIEnv;

    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeGetDiscoveredHosts(
        env: JNIEnv,
        _class: JClass,
    ) -> jstring {
        let csv = android_get_discovered_hosts_csv();
        env.new_string(csv).unwrap().into_raw()
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
        let pin_opt = if pin_rust.is_empty() {
            None
        } else {
            Some(pin_rust.as_str())
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
        android_stop_stream();
    }

    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeSetGainDb(
        _env: JNIEnv,
        _class: JClass,
        gain_db: jfloat,
    ) {
        android_set_gain_db(gain_db);
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
            if let Some(ref tx) = *GLOBAL_PHONE_MIC_TX.lock().unwrap() {
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
    ) -> jboolean {
        let Ok(ip_str) = env.get_string(&host_ip) else {
            return 0;
        };
        let ip_rust: String = ip_str.into();
        android_stop_phone_mic();
        if let Ok(tx) = PhoneMicTransmitter::start(&ip_rust, use_hardware_mic != 0) {
            *GLOBAL_PHONE_MIC_TX.lock().unwrap() = Some(tx);
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

    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeGetTelemetry(
        env: JNIEnv,
        _class: JClass,
    ) -> jlongArray {
        // Keep this array length and index order in sync with
        // MainScreen.kt's `LongArray(24)` telemetry buffer:
        // 0..10 legacy counters, 11..21 live latency breakdown.
        let mut stats = [0i64; 24];
        if let Some(ref c) = *GLOBAL_ANDROID_CLIENT.lock().unwrap() {
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
        }
        let arr = env.new_long_array(24).unwrap();
        let _ = env.set_long_array_region(&arr, 0, &stats);
        arr.into_raw()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                     239.255.77.77, 0.0.0.0, 192.168.42.129:65530";
        let targets = parse_target_endpoints_with_locals(input, &[local], false);
        assert_eq!(
            targets,
            vec!["192.168.1.60:65532".parse().unwrap()],
            "self, broadcast, multicast, unspecified and dead RNDIS hosts must be dropped"
        );

        // A real USB tethering interface makes the legacy RNDIS target reachable.
        let targets = parse_target_endpoints_with_locals("192.168.42.129", &[local], true);
        assert_eq!(targets, vec!["192.168.42.129:65532".parse().unwrap()]);
    }

    #[test]
    fn test_party_peer_lifecycle_dedupes_and_expires() {
        let now = Instant::now();
        let pc: SocketAddr = "192.168.1.60:65532".parse().unwrap();
        let static_targets = vec![pc];
        let mut peers = initial_party_peers(&static_targets, true, now);
        assert!(peers.contains_key(&pc));
        assert!(peers.contains_key(&"192.168.1.60:65530".parse().unwrap()));

        let phone_old: SocketAddr = "192.168.1.70:40000".parse().unwrap();
        let phone_new: SocketAddr = "192.168.1.70:40002".parse().unwrap();
        register_party_peer(&mut peers, phone_old, &static_targets, now);
        register_party_peer(&mut peers, phone_new, &static_targets, now);
        assert!(
            !peers.contains_key(&phone_old),
            "a reconnect on a new port must evict the stale port"
        );
        assert!(peers.contains_key(&phone_new));

        update_party_peer_feedback(&mut peers, phone_new, 120, 80, &static_targets, now);
        assert_eq!(peers[&phone_new].delay_ms, 120);
        assert_eq!(peers[&phone_new].fec_permille, 80);

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
    fn test_party_dtx_threshold_and_heartbeat_cadence() {
        assert!(!party_dtx_active(false, 10_000));
        assert!(!party_dtx_active(true, PARTY_DTX_SILENT_SUBFRAMES));
        assert!(party_dtx_active(true, PARTY_DTX_SILENT_SUBFRAMES + 1));

        // Two subframes per packet: a heartbeat every 160 subframes is one
        // every 80 packets, i.e. every 200 ms.
        assert!(party_dtx_heartbeat_due(0, 2));
        assert!(!party_dtx_heartbeat_due(80, 2));
        assert!(party_dtx_heartbeat_due(160, 2));
        assert!(!party_dtx_heartbeat_due(159, 2));
        assert!(party_dtx_heartbeat_due(80, 1));
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
    fn test_client_volume_feedback_roundtrip() {
        use audioshare_protocol::wire::{ClientFeedbackPacket, CONTROL_FEEDBACK_LEN};
        let fb = ClientFeedbackPacket {
            fec_rate_permille: 45,
            bluetooth_delay_ms: 120,
            client_volume: 85,
            requested_mode: 0,
            hardware_delay_ms: 17,
            auth_mac32: 0,
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
    fn test_phone_mic_transmitter_no_hardware_mic_start_stop() {
        let tx = PhoneMicTransmitter::start("127.0.0.1", false);
        assert!(tx.is_ok());
        let mut tx = tx.unwrap();
        tx.stop();
    }
}
