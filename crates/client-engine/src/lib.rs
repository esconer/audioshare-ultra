use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use audioshare_host_engine::{configure_realtime_udp_socket, promote_thread_realtime};
use audioshare_protocol::crypto::SessionKey;
use audioshare_protocol::dsp::{BurstDecouplerRing, ClockDriftResampler, TruePeakLimiter};
use audioshare_protocol::latency::ClientLatencySnapshot;
use audioshare_protocol::lpc_lossless::{
    decode_fec_subframe_to_f32, decode_lossless_pcm24_to_f32, decode_lossless_subframe,
    decode_vbr_subframe_to_f32, TAG_VBR_BFP,
};
use audioshare_protocol::opus_codec::{OpusStreamDecoder, OPUS_LOOKAHEAD_MS, TAG_OPUS_RLD};
use audioshare_protocol::sequencer::{
    ZeroJitterSequencer, STATUS_EMPTY, STATUS_HEALED_BY_FEC, STATUS_PRIMARY_LOSSLESS,
};
use audioshare_protocol::wire::{
    ClientFeedbackPacket, CodecMode, ParsedPacket, WireError, CONTROL_FEEDBACK_LEN,
    CONTROL_FEEDBACK_MAGIC,
};
use audioshare_protocol::{MAX_SAFE_UDP_PAYLOAD, SAMPLE_RATE, SUBFRAME_STEREO_SAMPLES};

#[cfg(not(target_os = "android"))]
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

const CONTROL_MAGIC_HELLO: &[u8; 8] = b"ASULTRA1";

#[derive(Debug, Clone, Copy)]
pub struct ClientConfig {
    pub server_addr: SocketAddr,
    pub target_buffer_subframes: f32,
    pub preamp_gain_db: f32,
    /// Android only: open the shared mixer path so OEM post-processing
    /// (Dolby Atmos, Dirac, system equalizers) can process the stream.
    /// `false` asks AAudio for the raw low-latency path and explicitly
    /// disables spatialization. Ignored by the desktop CPAL sink.
    pub dolby_effects: bool,
}

/// Emergency PLC is allowed only after the output decoupler has less than one
/// 2.5 ms subframe of real audio left. Keeping a full subframe of margin
/// (instead of the old two) stops a slightly late packet from triggering
/// WSOLA while the DAC still has audio it can play.
const PLC_EMERGENCY_WATERMARK_SUBFRAMES: usize = 1;

/// A post-stall fast-forward keeps the combined sequencer + decoupler backlog
/// within `target_buf + 6` subframes (15 ms). The old `+16` allowance let one
/// Wi-Fi stall leave up to 40 ms of permanent playback latency behind.
const MAX_BACKLOG_OVER_TARGET_SUBFRAMES: f32 = 6.0;

/// Gaming hard-lock never adds group-sync delay. The target must stay above
/// the sender's packet size plus the one-subframe PLC watermark, otherwise
/// every packet arrival starts from behind and the clock loop has to drain.
/// One measured packet interval (2.5 ms) is already covered by the packet
/// floor; only jitter above that adds cushion.
const GAMING_MAX_TARGET_SUBFRAMES: f32 = 8.0;
const GAMING_JITTER_MARGIN_GAIN: f32 = 1.4;

#[inline]
fn gaming_base_target_subframes(pkt_subframes: u8, jitter_us: f32) -> f32 {
    let packet_subframes = pkt_subframes.max(1);
    let packet_floor = match packet_subframes {
        1 => 4.0, // 10.0 ms: one in-flight packet + 2 subframes above PLC
        2 => 6.0, // 15.0 ms: one pkt of margin above the 2-subframe stream
        n => (n as f32) * 1.5 + 1.0,
    };
    let jitter_extra = ((jitter_us.max(0.0) / 2500.0) * GAMING_JITTER_MARGIN_GAIN
        - GAMING_JITTER_MARGIN_GAIN)
        .max(0.0);
    (packet_floor + jitter_extra).min(GAMING_MAX_TARGET_SUBFRAMES)
}

/// Loss at or below this is treated as a healthy link for cushion decay. FEC
/// heals count as loss in the feedback window, so requiring exactly zero
/// pinned a 35 ms cushion forever on a link with even 0.5% background
/// recovery.
const ADAPTIVE_DECAY_MAX_FEC_PERMILLE: u16 = 15;

/// Shrink the adaptive cushion toward the floor after a healthy feedback
/// window. Proportional decay removes most of a large burst-induced cushion in
/// a few seconds; the old fixed 0.15-subframe step needed ~27 seconds from the
/// 35 ms cap, and any FEC heal reset the timer.
#[inline]
fn decay_adaptive_cushion(adaptive: f32, floor: f32, fec_permille: u16) -> f32 {
    if adaptive <= floor || fec_permille > ADAPTIVE_DECAY_MAX_FEC_PERMILLE {
        return adaptive;
    }
    let step = ((adaptive - floor) * 0.12).max(0.15);
    (adaptive - step).max(floor)
}

/// Observation fed to the clock-drift controller.
///
/// An AAudio/CPAL callback drains a whole HAL burst (4-20 ms on Bluetooth)
/// from the decoupler at once, so the instantaneous combined depth spends most
/// of each callback period at a structural trough. Feeding that trough to the
/// windowed-min controller makes it compare "post-burst low" against the mean
/// target and wind the integral negative until the sequencer piles up the same
/// burst worth of latency. Regulate the planned cushion instead: the sequencer
/// backlog plus the decoupler target. Genuine starvation is signalled by
/// `STATUS_EMPTY`/FEC and grows the adaptive target; slowing the resampler by
/// a few hundred ppm cannot replace the missing audio.
#[inline]
fn clock_loop_observation(net_subframes: f32, target_buf: f32) -> f32 {
    net_subframes.max(0.0) + target_buf.max(0.0)
}

/// Output decoupler watermark in 2.5 ms subframes (4..=12).
#[inline]
fn fifo_target_subframes(target_buf: f32) -> usize {
    (target_buf.ceil() as usize).clamp(4, 12)
}

/// Number of subframes of real audio left after the current decoupler
/// watermark and one sender packet are consumed, before emergency PLC starts.
#[cfg(test)]
#[inline]
fn plc_jitter_margin_subframes(target_fifo: usize, packet_subframes: usize) -> i32 {
    target_fifo as i32 - PLC_EMERGENCY_WATERMARK_SUBFRAMES as i32 - packet_subframes as i32
}

/// Fast-forward a post-stall backlog so `sequencer + decoupler` stays within
/// `target_buf + MAX_BACKLOG_OVER_TARGET_SUBFRAMES`.
///
/// Only the sequencer portion can be shortened (the decoupler is already
/// feeding the DAC), so `keep` is computed from the space left under the cap.
/// It is clamped to the current sequencer depth so a decoupler that is already
/// at the cap can never rewind `next_play_seq` and replay old audio.
#[inline]
fn trim_post_stall_backlog(
    latest_rx_seq: u16,
    next_play_seq: &mut u16,
    net_subframes: &mut f32,
    fifo_subframes: f32,
    target_buf: f32,
) {
    let combined = *net_subframes + fifo_subframes;
    if combined <= target_buf + MAX_BACKLOG_OVER_TARGET_SUBFRAMES {
        return;
    }

    let max_net = (target_buf + MAX_BACKLOG_OVER_TARGET_SUBFRAMES - fifo_subframes)
        .max(0.0)
        .min(*net_subframes);
    let keep = max_net.floor() as u16;
    let new_next = latest_rx_seq.wrapping_add(1).wrapping_sub(keep);
    // Never move the playhead backwards. With the invariants above this is
    // already guaranteed, but the FIFO estimate is fractional and a defensive
    // check is cheaper than replaying audio.
    if (new_next.wrapping_sub(*next_play_seq) as i16) <= 0 {
        return;
    }
    *next_play_seq = new_next;
    *net_subframes = keep as f32;
}

/// Take at most one pending control command for this feedback packet.
///
/// Quality/media commands win the current packet, then the compressed bitrate,
/// then the codec backend, and finally the periodic gaming-lock refresh. Every
/// command has its own slot, so a startup restore of all of them is delivered
/// over consecutive loop passes (the feedback send condition stays true while
/// any slot is non-zero) instead of the later store overwriting the earlier
/// one. Gaming lock is last because it is refreshed every two seconds and must
/// never delay a one-shot user action.
#[inline]
fn take_pending_command(
    media: &AtomicU8,
    bitrate: &AtomicU8,
    codec: &AtomicU8,
    gaming: &AtomicU8,
) -> u8 {
    let media_cmd = media.swap(0, Ordering::Relaxed);
    if media_cmd != 0 {
        return media_cmd;
    }
    let bitrate_cmd = bitrate.swap(0, Ordering::Relaxed);
    if bitrate_cmd != 0 {
        return bitrate_cmd;
    }
    let codec_cmd = codec.swap(0, Ordering::Relaxed);
    if codec_cmd != 0 {
        return codec_cmd;
    }
    gaming.swap(0, Ordering::Relaxed)
}

/// A packet this far ahead of the playhead means the sequence tracker was
/// lost or the sender restarted. 48 subframes = 120 ms.
const STREAM_RESYNC_AHEAD_SUBFRAMES: i16 = 48;

/// A packet this far behind the playhead is outside the 35 ms maximum jitter
/// buffer. Treat it as a new stream instead of discarding hundreds of
/// milliseconds of audio after a host restart. 32 subframes = 80 ms.
const STREAM_RESYNC_BEHIND_SUBFRAMES: i16 = -32;

#[inline]
fn needs_stream_resync(seq_delta: i16) -> bool {
    seq_delta > STREAM_RESYNC_AHEAD_SUBFRAMES || seq_delta < STREAM_RESYNC_BEHIND_SUBFRAMES
}

/// Decode a `CodecMode::RawPcm16` payload.
///
/// The lossless encoder's raw fallback is a tagged 481-byte frame
/// (`TAG_RAW_PCM16` + 480 bytes). A raw-mode sender may also omit the tag and
/// send exactly 480 bytes. Accept both so the mode is not tied to one
/// historical framing choice.
fn decode_raw_pcm16_payload(payload: &[u8], out: &mut [f32; SUBFRAME_STEREO_SAMPLES]) -> bool {
    if payload.len() == SUBFRAME_STEREO_SAMPLES * 2 {
        for i in 0..SUBFRAME_STEREO_SAMPLES {
            let s = i16::from_le_bytes([payload[i * 2], payload[i * 2 + 1]]);
            out[i] = (s as f32) * (1.0 / 32768.0);
        }
        return true;
    }

    let mut pcm_i16 = [0i16; SUBFRAME_STEREO_SAMPLES];
    if decode_lossless_subframe(payload, &mut pcm_i16).is_ok() {
        for (dst, &s) in out.iter_mut().zip(pcm_i16.iter()) {
            *dst = (s as f32) * (1.0 / 32768.0);
        }
        return true;
    }
    false
}

/// True when a compressed packet's primary payload is Opus RLD.
///
/// The header mode is authoritative for current hosts. The payload tag check
/// also covers a packet whose header was switched to `CompressedLowDelay`
/// after an Opus encode failure mid-bundle: its first subframe may still be
/// Opus, and splicing zero-lookahead ADPCM FEC into that stream would jump
/// the waveform by one subframe.
#[inline]
fn primary_is_opus(mode: CodecMode, primary_payload: &[u8]) -> bool {
    mode == CodecMode::OpusRld || primary_payload.get(2) == Some(&TAG_OPUS_RLD)
}

/// Decode a compressed-mode primary payload by its self-identifying tag.
///
/// The host can switch between the pure-Rust VBR codec and Opus live, so the
/// tag is authoritative even when the wire header says
/// `CompressedLowDelay` or `OpusRld`. Dropping the Opus decoder when a
/// non-Opus payload arrives prevents stale codec history from leaking across
/// a backend switch or a stream resync.
///
/// When the payload is Opus, any sequence gap is concealed by the codec's own
/// CELT PLC and delivered straight into the sequencer. Opus RLD carries a
/// 2.5 ms lookahead, so those PLC frames are phase-aligned with the primary
/// stream; the zero-lookahead IMA-ADPCM FEC backup is not (it would jump the
/// waveform by one subframe), so the caller skips that backup for Opus.
fn decode_compressed_payload(
    payload: &[u8],
    seq: u16,
    opus_decoder: &mut Option<OpusStreamDecoder>,
    sequencer: &ZeroJitterSequencer,
    next_play_seq: u16,
    out: &mut [f32; SUBFRAME_STEREO_SAMPLES],
) -> bool {
    if payload.first() == Some(&TAG_OPUS_RLD) {
        if opus_decoder.is_none() {
            *opus_decoder = OpusStreamDecoder::new();
        }
        return opus_decoder
            .as_mut()
            .map(|decoder| {
                decoder.decode_subframe_with_plc(seq, payload, out, |missed_seq, plc_pcm| {
                    // Only fill slots that are still ahead of the playhead and
                    // genuinely empty. A late primary copy may arrive later and
                    // is allowed to replace this concealment.
                    if (missed_seq.wrapping_sub(next_play_seq) as i16) >= 0
                        && sequencer.is_slot_missing(missed_seq)
                    {
                        sequencer.try_heal_with_fec(missed_seq, plc_pcm);
                    }
                })
            })
            .unwrap_or(false);
    }

    // A non-Opus frame means the host switched back to VBR (or sent a legacy
    // ADPCM frame). Reset the Opus decoder so a later switch starts clean.
    *opus_decoder = None;
    if payload.first() == Some(&TAG_VBR_BFP) {
        decode_vbr_subframe_to_f32(payload, out)
    } else {
        // Legacy fallback: older hosts used a raw 124-byte IMA-ADPCM frame as
        // the compressed primary payload. The FEC decoder also reads those.
        decode_fec_subframe_to_f32(payload, out)
    }
}

/// Convert an AAudio timestamp snapshot into the milliseconds still queued
/// ahead of the DAC. `AAudioStream_getTimestamp` reports the frame position at
/// the last HAL interrupt, not at "now"; the position must be extrapolated by
/// the elapsed monotonic time before subtracting it from frames written.
/// `sample_rate_hz` is the rate the stream actually opened, which is not
/// necessarily the 48 kHz network rate on a 44.1 kHz Bluetooth route.
#[inline]
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn pending_hardware_delay_ms(
    frame_position: i64,
    timestamp_ns: i64,
    now_ns: i64,
    frames_written: i64,
    sample_rate_hz: u32,
) -> Option<u16> {
    if frame_position < 0 || timestamp_ns <= 0 {
        return None;
    }
    let rate = sample_rate_hz.max(8_000) as i64;
    let elapsed_ns = (now_ns - timestamp_ns).clamp(0, 500_000_000);
    let presented_now =
        frame_position.saturating_add(elapsed_ns.saturating_mul(rate) / 1_000_000_000);
    let pending_frames = frames_written.saturating_sub(presented_now).max(0);
    let delay_ms = ((pending_frames * 1000) / rate).clamp(0, 120) as u16;
    Some(delay_ms)
}

/// AAudio's output ring must hold at least one HAL burst. A2DP/Bluetooth
/// bursts are commonly 480-960 frames (10-20 ms), so the old unconditional
/// `burst * 3` turned them into 30-60 ms of output latency. Use one burst for
/// large bursts and keep the verified 3x cushion (minimum 256 frames) for
/// speaker/USB bursts, where it costs only a few milliseconds.
#[inline]
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn aaudio_buffer_frames_for_burst(burst: i32) -> Option<i32> {
    if burst <= 0 {
        None
    } else if burst >= 240 {
        Some(burst)
    } else {
        Some((burst * 3).max(256))
    }
}

pub struct ClientTelemetry {
    pub packets_received: AtomicU64,
    pub primary_frames_played: AtomicU64,
    pub fec_healed_frames: AtomicU64,
    pub plc_extrapolated_frames: AtomicU64,
    pub auth_failures: AtomicU64,
    pub clock_drift_ppm: AtomicI32,
    pub preamp_gain_db_tenths: AtomicI32,
    pub eq_bass_db_tenths: AtomicI32,
    pub eq_presence_db_tenths: AtomicI32,
    pub eq_treble_db_tenths: AtomicI32,
    pub channel_role: AtomicU8,
    pub enable_binaural_crossfeed: AtomicBool,
    pub enable_night_mode: AtomicBool,
    pub gaming_mode_hard_lock: AtomicBool,
    /// Android: `true` keeps the shared mixer + OEM post-processing path
    /// (Dolby/Atmos/system EQ); `false` selects the raw low-latency path.
    pub enable_dolby_effects: AtomicBool,
    pub downstream_bt_delay_ms: AtomicU16,
    /// Hardware/DAC latency measured with `AAudioStream_getTimestamp` on
    /// Android. Kept separate from the A2DP codec estimate reported by Kotlin;
    /// consumers use the larger of the two.
    pub downstream_hw_delay_ms: AtomicU16,
    pub pending_media_command: AtomicU8,
    /// Compressed-codec backend selection (20 = pure-Rust VBR, 21 = Opus RLD).
    /// Kept separate from [`Self::pending_media_command`] so restoring the
    /// quality profile and the backend back-to-back cannot overwrite either.
    pub pending_codec_backend_command: AtomicU8,
    /// Compressed bitrate selection (30 = 96, 31 = 160, 32 = 256, 33 = 320
    /// kbps). A third slot keeps a startup restore of quality + bitrate +
    /// backend from collapsing into a single command.
    pub pending_bitrate_command: AtomicU8,
    /// Host framing request for 5 ms gaming lock (40 = off, 41 = on). Kept
    /// separate from the user-action slots so the periodic refresh cannot
    /// overwrite a pending quality/bitrate/codec command.
    pub pending_gaming_lock_command: AtomicU8,
    pub host_volume: AtomicU8,
    pub max_group_delay_ms: AtomicU16,
    pub seconds_since_last_packet: AtomicU16,
    pub server_dtx_silent: AtomicBool,
    /// RFC 3550 inter-arrival jitter estimate in microseconds.
    pub network_jitter_us: AtomicU32,
    /// Adaptive network cushion before group-sync alignment, in microseconds.
    pub adaptive_buffer_us: AtomicU32,
    /// Final clamped FIFO target (including effective group sync), in
    /// microseconds.
    pub target_buffer_us: AtomicU32,
    /// Live combined sequencer + output FIFO depth, in microseconds.
    pub buffered_audio_us: AtomicU32,
    /// Effective group-sync addition after the FIFO clamp, in microseconds.
    pub group_sync_extra_us: AtomicU32,
    /// Sender bundling interval (`subframe_count * 2.5 ms`), in microseconds.
    pub packet_interval_us: AtomicU32,
    /// Wire codec mode of the most recent audio packet.
    pub codec_mode: AtomicU8,
    /// True once the prebuffer has filled enough to start playback.
    pub prebuffer_ready: AtomicBool,
}

impl ClientTelemetry {
    /// Build a point-in-time breakdown for the live debug views.
    ///
    /// `output_buffer_ms` is the platform's configured output stage: 5 ms on
    /// desktop CPAL, 0 on Android where the AAudio DAC queue is measured.
    pub fn latency_snapshot(&self, output_buffer_ms: f32) -> ClientLatencySnapshot {
        ClientLatencySnapshot {
            packet_interval_ms: self.packet_interval_us.load(Ordering::Relaxed) as f32 / 1000.0,
            network_jitter_ms: self.network_jitter_us.load(Ordering::Relaxed) as f32 / 1000.0,
            adaptive_buffer_ms: self.adaptive_buffer_us.load(Ordering::Relaxed) as f32 / 1000.0,
            target_buffer_ms: self.target_buffer_us.load(Ordering::Relaxed) as f32 / 1000.0,
            buffered_audio_ms: self.buffered_audio_us.load(Ordering::Relaxed) as f32 / 1000.0,
            group_sync_extra_ms: self.group_sync_extra_us.load(Ordering::Relaxed) as f32 / 1000.0,
            hardware_delay_ms: self.downstream_hw_delay_ms.load(Ordering::Relaxed),
            bluetooth_delay_ms: self.downstream_bt_delay_ms.load(Ordering::Relaxed),
            output_buffer_ms,
            codec_lookahead_ms: if self.codec_mode.load(Ordering::Relaxed)
                == CodecMode::OpusRld as u8
            {
                OPUS_LOOKAHEAD_MS
            } else {
                0.0
            },
            prebuffer_ready: self.prebuffer_ready.load(Ordering::Relaxed),
            codec_mode: self.codec_mode.load(Ordering::Relaxed),
        }
    }
}

pub struct ClientEngine {
    running: Arc<AtomicBool>,
    pub telemetry: Arc<ClientTelemetry>,
    session_key: Arc<Mutex<Option<SessionKey>>>,
    rx_handle: Option<std::thread::JoinHandle<()>>,
}

impl ClientEngine {
    pub fn connect(config: ClientConfig) -> Result<Self, String> {
        Self::connect_with_pin(config, None)
    }

    pub fn connect_with_pin(config: ClientConfig, pin: Option<&str>) -> Result<Self, String> {
        let running = Arc::new(AtomicBool::new(true));
        // Mirror the RX loop's initial cushion so the debug view has sensible
        // values before the first packet arrives.
        let initial_floor_subframes = if config.target_buffer_subframes <= 2.5 {
            4.0
        } else {
            config.target_buffer_subframes.max(6.0)
        };
        let initial_target_subframes = (initial_floor_subframes.ceil() as u32).clamp(4, 12);
        let telemetry = Arc::new(ClientTelemetry {
            packets_received: AtomicU64::new(0),
            primary_frames_played: AtomicU64::new(0),
            fec_healed_frames: AtomicU64::new(0),
            plc_extrapolated_frames: AtomicU64::new(0),
            auth_failures: AtomicU64::new(0),
            clock_drift_ppm: AtomicI32::new(0),
            preamp_gain_db_tenths: AtomicI32::new((config.preamp_gain_db * 10.0) as i32),
            eq_bass_db_tenths: AtomicI32::new(0),
            eq_presence_db_tenths: AtomicI32::new(0),
            eq_treble_db_tenths: AtomicI32::new(0),
            channel_role: AtomicU8::new(0),
            enable_binaural_crossfeed: AtomicBool::new(false),
            enable_night_mode: AtomicBool::new(false),
            gaming_mode_hard_lock: AtomicBool::new(false),
            enable_dolby_effects: AtomicBool::new(config.dolby_effects),
            downstream_bt_delay_ms: AtomicU16::new(0),
            downstream_hw_delay_ms: AtomicU16::new(0),
            pending_media_command: AtomicU8::new(0),
            pending_codec_backend_command: AtomicU8::new(0),
            pending_bitrate_command: AtomicU8::new(0),
            pending_gaming_lock_command: AtomicU8::new(0),
            host_volume: AtomicU8::new(100),
            max_group_delay_ms: AtomicU16::new(0),
            seconds_since_last_packet: AtomicU16::new(0),
            server_dtx_silent: AtomicBool::new(false),
            network_jitter_us: AtomicU32::new(2500),
            adaptive_buffer_us: AtomicU32::new((initial_floor_subframes * 2500.0) as u32),
            target_buffer_us: AtomicU32::new(initial_target_subframes * 2500),
            buffered_audio_us: AtomicU32::new(0),
            group_sync_extra_us: AtomicU32::new(0),
            packet_interval_us: AtomicU32::new(2500),
            codec_mode: AtomicU8::new(CodecMode::StudioLosslessLpc as u8),
            prebuffer_ready: AtomicBool::new(false),
        });

        let session_key = Arc::new(Mutex::new(pin.map(SessionKey::from_pin)));

        // Bind port 65530 with SO_REUSEADDR first (enables receiving 1-Phone-to-Multi-Phone broadcasts!),
        // falling back to an ephemeral port (0.0.0.0:0) if HostEngine is running on the same device.
        let socket = {
            let mut bound = None;
            if let Ok(raw) = socket2::Socket::new(
                socket2::Domain::IPV4,
                socket2::Type::DGRAM,
                Some(socket2::Protocol::UDP),
            ) {
                let _ = raw.set_reuse_address(true);
                #[cfg(all(unix, not(target_os = "solaris"), not(target_os = "illumos")))]
                let _ = raw.set_reuse_port(true);
                let addr = SocketAddr::from(([0, 0, 0, 0], 65530));
                if raw.bind(&addr.into()).is_ok() {
                    bound = Some(UdpSocket::from(raw));
                }
            }
            match bound {
                Some(s) => s,
                None => UdpSocket::bind("0.0.0.0:0")
                    .map_err(|e| format!("Failed to bind client UDP socket: {}", e))?,
            }
        };
        configure_realtime_udp_socket(&socket)
            .map_err(|e| format!("Failed to configure real-time client socket: {}", e))?;
        let _ = socket.set_read_timeout(Some(Duration::from_millis(5)));

        let _ = socket.send_to(CONTROL_MAGIC_HELLO, config.server_addr);

        let burst_fifo = Arc::new(BurstDecouplerRing::new());
        let running_clone = Arc::clone(&running);
        let telemetry_clone = Arc::clone(&telemetry);
        let key_clone = Arc::clone(&session_key);

        let rx_handle = std::thread::Builder::new()
            .name("as-ultra-client-rx".into())
            .spawn(move || {
                promote_thread_realtime();

                #[cfg(not(target_os = "android"))]
                let _audio_stream = Self::spawn_desktop_output_sink(Arc::clone(&burst_fifo)).ok();

                #[cfg(target_os = "android")]
                let mut _android_aaudio: Option<android_aaudio::AAudioOutputGuard> = None;

                Self::realtime_rx_and_dsp_loop(
                    socket,
                    config,
                    burst_fifo,
                    #[cfg(target_os = "android")]
                    &mut _android_aaudio,
                    running_clone,
                    telemetry_clone,
                    key_clone,
                );
            })
            .map_err(|e| format!("Failed to spawn client RX thread: {}", e))?;

        Ok(Self {
            running,
            telemetry,
            session_key,
            rx_handle: Some(rx_handle),
        })
    }

    pub fn set_encryption_pin(&self, pin: Option<&str>) {
        let mut guard = self.session_key.lock().unwrap();
        *guard = pin.map(SessionKey::from_pin);
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(h) = self.rx_handle.take() {
            let _ = h.join();
        }
    }

    #[cfg(not(target_os = "android"))]
    fn fill_i16_output(fifo: &BurstDecouplerRing, out: &mut [i16], dither_rng: &mut u32) {
        // CPAL callbacks can be larger than 4096 samples on PulseAudio/ALSA.
        // Fill every sample in chunks; the old code only filled the first
        // 4096 and left the rest of the buffer uninitialized.
        let mut tmp = [0.0f32; 4096];
        let mut offset = 0usize;
        while offset < out.len() {
            let chunk = (out.len() - offset).min(tmp.len());
            fifo.pop_exact_or_dither(&mut tmp[..chunk], dither_rng);
            for (dst, &s) in out[offset..offset + chunk]
                .iter_mut()
                .zip(tmp[..chunk].iter())
            {
                *dst = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
            }
            offset += chunk;
        }
    }

    #[cfg(not(target_os = "android"))]
    fn spawn_desktop_output_sink(
        burst_fifo: Arc<BurstDecouplerRing>,
    ) -> Result<cpal::Stream, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "No audio output device available".to_string())?;
        let default_cfg = device
            .default_output_config()
            .map_err(|e| format!("Output config error: {}", e))?;
        let sample_format = default_cfg.sample_format();

        // Prefer a 5 ms fixed callback buffer to cut desktop output latency.
        // Many ALSA/PulseAudio backends reject an unsupported period size, so
        // fall back to the device default instead of failing to start audio.
        let low_lat_cfg = cpal::StreamConfig {
            channels: 2,
            sample_rate: cpal::SampleRate(SAMPLE_RATE),
            buffer_size: cpal::BufferSize::Fixed(240),
        };
        let default_stream_cfg = cpal::StreamConfig {
            channels: 2,
            sample_rate: cpal::SampleRate(SAMPLE_RATE),
            buffer_size: cpal::BufferSize::Default,
        };

        let build_for_cfg = |cfg: &cpal::StreamConfig| {
            let fifo = Arc::clone(&burst_fifo);
            let mut dither_rng = 0x9E37_79B9u32;
            match sample_format {
                cpal::SampleFormat::I16 => device.build_output_stream(
                    cfg,
                    move |out: &mut [i16], _: &cpal::OutputCallbackInfo| {
                        Self::fill_i16_output(&fifo, out, &mut dither_rng);
                    },
                    |_| {},
                    None,
                ),
                _ => device.build_output_stream(
                    cfg,
                    move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        fifo.pop_exact_or_dither(out, &mut dither_rng);
                    },
                    |_| {},
                    None,
                ),
            }
        };

        let stream = build_for_cfg(&low_lat_cfg)
            .or_else(|_| build_for_cfg(&default_stream_cfg))
            .map_err(|e| format!("Failed to open audio output: {}", e))?;

        stream
            .play()
            .map_err(|e| format!("Failed to start audio output: {}", e))?;
        Ok(stream)
    }

    fn realtime_rx_and_dsp_loop(
        socket: UdpSocket,
        config: ClientConfig,
        burst_fifo: Arc<BurstDecouplerRing>,
        #[cfg(target_os = "android")] aaudio_guard: &mut Option<android_aaudio::AAudioOutputGuard>,
        running: Arc<AtomicBool>,
        telemetry: Arc<ClientTelemetry>,
        session_key: Arc<Mutex<Option<SessionKey>>>,
    ) {
        let mut sequencer = ZeroJitterSequencer::new();
        let mut resampler = ClockDriftResampler::new();
        let mut limiter = TruePeakLimiter::new(config.preamp_gain_db);
        let mut studio_eq = audioshare_protocol::sync_and_eq::StudioEqualizer::new();
        let mut crossfeed = audioshare_protocol::sync_and_eq::BauerCrossfeedFilter::new();
        let mut night_compressor = audioshare_protocol::sync_and_eq::NightModeCompressor::new();
        let group_sync = audioshare_protocol::sync_and_eq::GroupSyncEstimator::new();
        // Created lazily on the first Opus packet. Dropped on DTX/resync and
        // whenever the host switches back to VBR.
        let mut opus_decoder: Option<OpusStreamDecoder> = None;

        let mut recv_buf = [0u8; MAX_SAFE_UDP_PAYLOAD];
        let mut stream_initialized = false;
        let mut prebuffer_ready = false;
        let mut next_play_seq: u16 = 0;
        let mut latest_rx_seq: u16 = 0;

        let mut last_feedback = Instant::now();
        let mut last_packet_instant = Instant::now();
        #[cfg(target_os = "android")]
        let mut last_hw_latency_poll = Instant::now();
        #[cfg(target_os = "android")]
        let mut last_dolby_state = telemetry.enable_dolby_effects.load(Ordering::Relaxed);
        #[cfg(target_os = "android")]
        let mut last_aaudio_open_attempt: Option<Instant> = None;
        let mut window_primary = 0u32;
        let mut window_fec = 0u32;
        // Auto-adaptive jitter cushion: 3.0 (7.5ms) on 5GHz, auto-expands up to 14.0 (35.0ms) on 2.4GHz!
        let mut initial_wifi_floor = if config.target_buffer_subframes <= 2.5 {
            4.0 // Minimum 4 subframes (10.0ms) for USB so OS capture chunks never underrun
        } else {
            config.target_buffer_subframes.max(6.0)
        };
        let mut adaptive_wifi_subframes = initial_wifi_floor;
        let mut last_pkt_subframe_count: u8 = 1;
        // The refill loop targets this watermark. Keeping the last value lets
        // the socket poll below decide whether the decoupler needs audio
        // before the new target is recomputed for this pass.
        let mut last_fifo_watermark =
            resampler.subframes_to_samples(fifo_target_subframes(initial_wifi_floor));
        let mut consecutive_empty_ticks: u16 = 0;
        // RFC 3550 Inter-Arrival Jitter Estimator state (microseconds)
        let mut prev_rx_us: Option<u32> = None;
        let mut prev_tx_us: u32 = 0;
        let mut jitter_estimate_us: f32 = 2500.0;
        // Output at the device's native rate can be 44.1 kHz (A2DP) or more
        // than 48 kHz (USB DAC). Size for up to ~384 kHz so the resampler can
        // never truncate the tail of a subframe.
        let mut resampled_out = vec![0.0f32; 4096];
        let start_instant = Instant::now();
        let _ = socket.set_nonblocking(false);
        let _ = socket.set_read_timeout(Some(Duration::from_micros(1000)));
        // Send the current gaming-lock state on the first pass, then refresh it
        // every two seconds so a restarted host converges even if the client
        // never toggles again.
        let mut last_gaming_lock_state: Option<bool> = None;
        let mut last_gaming_feedback = Instant::now();

        while running.load(Ordering::Relaxed) {
            // Auto-recover the AAudio stream within 25 ms when the
            // headphones/Bluetooth route changes, or reopen it when the user
            // toggles Dolby/system post-processing. The resampler is pointed
            // at the rate the new stream actually opened, so 44.1 kHz A2DP
            // does not need a second AudioFlinger resampler.
            #[cfg(target_os = "android")]
            {
                let want_dolby = telemetry.enable_dolby_effects.load(Ordering::Relaxed);
                let retry_due = aaudio_guard.is_none()
                    && last_aaudio_open_attempt
                        .map(|attempt| attempt.elapsed() >= Duration::from_millis(500))
                        .unwrap_or(true);
                let reopen = aaudio_guard
                    .as_ref()
                    .map(|g| g.needs_reopen())
                    .unwrap_or(false)
                    || want_dolby != last_dolby_state
                    || retry_due;
                if reopen {
                    last_dolby_state = want_dolby;
                    last_aaudio_open_attempt = Some(Instant::now());
                    *aaudio_guard = android_aaudio::AAudioOutputGuard::open_with_mode(
                        Arc::clone(&burst_fifo),
                        want_dolby,
                    );
                    if let Some(guard) = aaudio_guard.as_ref() {
                        resampler.set_output_sample_rate(guard.sample_rate());
                    }
                    // The previous route's measurement is no longer valid.
                    telemetry.downstream_hw_delay_ms.store(0, Ordering::Relaxed);
                }
            }

            // Sample the AAudio hardware timestamp twice a second. This is
            // intentionally done from the RX loop (not the audio callback) so
            // a slow HAL timestamp query can never cause an output glitch.
            #[cfg(target_os = "android")]
            if last_hw_latency_poll.elapsed() >= Duration::from_millis(500) {
                last_hw_latency_poll = Instant::now();
                if let Some(delay_ms) = aaudio_guard
                    .as_ref()
                    .and_then(|guard| guard.measure_hardware_latency_ms())
                {
                    telemetry
                        .downstream_hw_delay_ms
                        .store(delay_ms, Ordering::Relaxed);
                }
            }

            let active_key = *session_key.lock().unwrap();

            // Tell the host whenever 5 ms hard-lock changes, and refresh the
            // state every two seconds so a host restart or client reconnect
            // converges even if the user never toggles again.
            let gaming_lock_now = telemetry.gaming_mode_hard_lock.load(Ordering::Relaxed);
            if last_gaming_lock_state != Some(gaming_lock_now)
                || last_gaming_feedback.elapsed() >= Duration::from_secs(2)
            {
                telemetry
                    .pending_gaming_lock_command
                    .store(if gaming_lock_now { 41 } else { 40 }, Ordering::Relaxed);
                last_gaming_lock_state = Some(gaming_lock_now);
                last_gaming_feedback = Instant::now();
            }

            if last_feedback.elapsed() >= Duration::from_millis(500)
                || telemetry.pending_media_command.load(Ordering::Relaxed) != 0
                || telemetry
                    .pending_codec_backend_command
                    .load(Ordering::Relaxed)
                    != 0
                || telemetry.pending_bitrate_command.load(Ordering::Relaxed) != 0
                || telemetry
                    .pending_gaming_lock_command
                    .load(Ordering::Relaxed)
                    != 0
            {
                telemetry.seconds_since_last_packet.store(
                    last_packet_instant.elapsed().as_secs().min(65535) as u16,
                    Ordering::Relaxed,
                );

                let total_win = (window_primary + window_fec).max(1);
                let fec_permille = ((window_fec * 1000) / total_win) as u16;
                window_primary = 0;
                window_fec = 0;

                // Decay the 2.4 GHz cushion when the link is healthy enough
                // that FEC is keeping up. Minor background recovery must not
                // pin the cushion at its 35 ms ceiling forever.
                adaptive_wifi_subframes = decay_adaptive_cushion(
                    adaptive_wifi_subframes,
                    initial_wifi_floor,
                    fec_permille,
                );

                // Transport/quality commands take the current packet; a
                // simultaneous codec-backend selection stays pending and is
                // sent on the next loop pass, so neither command is lost.
                let media_cmd = take_pending_command(
                    &telemetry.pending_media_command,
                    &telemetry.pending_bitrate_command,
                    &telemetry.pending_codec_backend_command,
                    &telemetry.pending_gaming_lock_command,
                );

                let gain_db =
                    (telemetry.preamp_gain_db_tenths.load(Ordering::Relaxed) as f32) / 10.0;
                let client_vol = if gain_db <= -40.0 {
                    0u8
                } else {
                    (10.0f32.powf(gain_db / 20.0) * 100.0).clamp(0.0, 100.0) as u8
                };

                // Report the two downstream delays separately. The network
                // jitter buffer is a local, adaptive quantity and must never
                // be fed back into the group-sync loop: doing so makes the
                // host echo a stale buffer depth back to the only client and
                // causes `target_buf` to inflate/collapse every 500ms.
                let bt_delay_ms = telemetry.downstream_bt_delay_ms.load(Ordering::Relaxed);
                let hw_delay_ms = telemetry.downstream_hw_delay_ms.load(Ordering::Relaxed);

                let fb = ClientFeedbackPacket {
                    fec_rate_permille: fec_permille,
                    bluetooth_delay_ms: bt_delay_ms,
                    hardware_delay_ms: hw_delay_ms,
                    client_volume: client_vol,
                    requested_mode: media_cmd,
                    auth_mac32: 0,
                };
                let mut fb_bytes = [0u8; CONTROL_FEEDBACK_LEN];
                fb.encode_signed(active_key.as_ref(), &mut fb_bytes);

                let _ = socket.send_to(&fb_bytes, config.server_addr);

                last_feedback = Instant::now();
            }

            // A blocking recv_from (up to 1 ms, often longer under Windows or
            // Android power-save) can starve the decoupler even when the
            // sequencer already holds the next playable subframe. Poll
            // non-blocking in that case so refill runs before waiting on the
            // network.
            let refill_poll = stream_initialized
                && prebuffer_ready
                && burst_fifo.available_samples() < last_fifo_watermark
                && !sequencer.is_slot_missing(next_play_seq);
            if refill_poll {
                let _ = socket.set_nonblocking(true);
            }

            let mut packets_drained = 0u8;

            while let Ok((len, _)) = socket.recv_from(&mut recv_buf) {
                if packets_drained == 0 {
                    let _ = socket.set_nonblocking(true);
                }
                packets_drained = packets_drained.saturating_add(1);

                // Ignore control packets (ASULTRA1 / ASULCTRL) if received on shared port 65530
                if len >= 8
                    && (&recv_buf[..8] == CONTROL_MAGIC_HELLO
                        || &recv_buf[..8] == CONTROL_FEEDBACK_MAGIC)
                {
                    continue;
                }
                match ParsedPacket::parse_and_decrypt(&mut recv_buf[..len], active_key.as_ref()) {
                    Ok(parsed) => {
                        last_packet_instant = Instant::now();
                        let base_seq = parsed.header.sequence;

                        telemetry.packets_received.fetch_add(1, Ordering::Relaxed);
                        telemetry
                            .host_volume
                            .store(parsed.header.host_volume, Ordering::Relaxed);
                        telemetry
                            .max_group_delay_ms
                            .store((parsed.header.stream_id as u16) * 2, Ordering::Relaxed);
                        telemetry.packet_interval_us.store(
                            (parsed.header.subframe_count.max(1) as u32) * 2500,
                            Ordering::Relaxed,
                        );
                        last_pkt_subframe_count = parsed.header.subframe_count.max(1);
                        telemetry
                            .codec_mode
                            .store(parsed.header.mode as u8, Ordering::Relaxed);

                        // DTX heartbeats are not audio: they arrive every
                        // 200 ms and must not feed the RFC 3550 jitter
                        // estimator, which assumes back-to-back 2.5 ms
                        // packets. They still advance both sequence trackers
                        // so a long silence cannot leave `latest_rx_seq`
                        // behind the 16-bit wrap point.
                        if parsed.header.mode == CodecMode::SilenceHeartbeat {
                            telemetry.server_dtx_silent.store(true, Ordering::Relaxed);
                            prebuffer_ready = false;
                            telemetry.prebuffer_ready.store(false, Ordering::Relaxed);
                            next_play_seq = base_seq;
                            latest_rx_seq = base_seq;
                            prev_rx_us = None;
                            // DTX means the host stopped sending audio. Drop
                            // any buffered slots and PLC history from before
                            // the silence so a later stream restart cannot
                            // replay stale payloads.
                            sequencer.reset();
                            opus_decoder = None;
                            continue;
                        }

                        // RFC 3550 Inter-Arrival Jitter calculation using sender `tx_timestamp_us`
                        let rx_now_us = start_instant.elapsed().as_micros() as u32;
                        let tx_now_us = parsed.header.tx_timestamp_us;
                        if let Some(last_r) = prev_rx_us {
                            let d_rx = rx_now_us.wrapping_sub(last_r) as i32;
                            let d_tx = tx_now_us.wrapping_sub(prev_tx_us) as i32;
                            let transit_delta = (d_rx - d_tx).unsigned_abs().min(50_000) as f32;
                            jitter_estimate_us +=
                                (transit_delta - jitter_estimate_us) * (1.0 / 16.0);
                        }
                        telemetry
                            .network_jitter_us
                            .store(jitter_estimate_us.max(0.0) as u32, Ordering::Relaxed);
                        prev_rx_us = Some(rx_now_us);
                        prev_tx_us = tx_now_us;
                        // Ensure floor is always at least 2.5 packets for the sender's bundling mode
                        let pkt_subframes =
                            (parsed.header.subframe_count.max(1) as f32) * 2.5 + 1.0;
                        if pkt_subframes > initial_wifi_floor {
                            initial_wifi_floor = pkt_subframes;
                        }
                        // Proactively cushion buffer to >= 2.2x measured network jitter (in 2500us subframes)
                        let proactive_floor =
                            ((jitter_estimate_us / 2500.0) * 2.2).clamp(initial_wifi_floor, 14.0);
                        adaptive_wifi_subframes = adaptive_wifi_subframes.max(proactive_floor);

                        if !stream_initialized {
                            next_play_seq = base_seq;
                            latest_rx_seq = base_seq;
                            stream_initialized = true;
                        }

                        // Signed 16-bit sequence delta (RFC 1982): >0 means future, <0 means past/late duplicate
                        let seq_delta = base_seq.wrapping_sub(next_play_seq) as i16;
                        if telemetry.server_dtx_silent.swap(false, Ordering::Relaxed)
                            || needs_stream_resync(seq_delta)
                        {
                            // The playhead is jumping to a new base sequence.
                            // Clear old slots so a wrapped/reused sequence
                            // number cannot surface a stale payload, and
                            // clear PLC history from the previous stream.
                            sequencer.reset();
                            next_play_seq = base_seq;
                            latest_rx_seq = base_seq;
                            prebuffer_ready = false;
                            telemetry.prebuffer_ready.store(false, Ordering::Relaxed);
                            prev_rx_us = None;
                            opus_decoder = None;
                        }

                        // Lazy FEC Decoding: Only decode FEC [N-1] / [N-2] if that slot is ACTUALLY missing
                        // and still ahead of the DAC playhead (saves 99.5% of FEC CPU work!).
                        //
                        // Opus RLD has a 2.5 ms codec lookahead, while the
                        // IMA-ADPCM FEC backup has zero lookahead. Splicing
                        // that FEC into an Opus stream would advance the
                        // waveform by one subframe, so Opus gaps are healed by
                        // phase-aligned CELT PLC in `decode_compressed_payload`
                        // instead.
                        let opus_primary =
                            primary_is_opus(parsed.header.mode, parsed.primary_payload);
                        if !opus_primary && !parsed.fec_n1_payload.is_empty() {
                            let s1 = base_seq.wrapping_sub(1);
                            if (s1.wrapping_sub(next_play_seq) as i16) >= 0
                                && sequencer.is_slot_missing(s1)
                            {
                                let mut fec1_f32 = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                                if decode_fec_subframe_to_f32(parsed.fec_n1_payload, &mut fec1_f32)
                                {
                                    sequencer.try_heal_with_fec(s1, &fec1_f32);
                                }
                            }
                        }
                        if !opus_primary && !parsed.fec_n2_payload.is_empty() {
                            let s2 = base_seq.wrapping_sub(2);
                            if (s2.wrapping_sub(next_play_seq) as i16) >= 0
                                && sequencer.is_slot_missing(s2)
                            {
                                let mut fec2_f32 = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                                if decode_fec_subframe_to_f32(parsed.fec_n2_payload, &mut fec2_f32)
                                {
                                    sequencer.try_heal_with_fec(s2, &fec2_f32);
                                }
                            }
                        }

                        let mut cursor = 0usize;
                        let prim = parsed.primary_payload;
                        for sub_idx in 0..(parsed.header.subframe_count as u16) {
                            if cursor + 2 > prim.len() {
                                break;
                            }
                            let sub_len =
                                u16::from_be_bytes([prim[cursor], prim[cursor + 1]]) as usize;
                            cursor += 2;
                            if cursor + sub_len > prim.len() {
                                break;
                            }

                            let s = base_seq.wrapping_add(sub_idx);
                            // Skip redundant work before touching any decoder.
                            // Party-mode broadcast and USB+Wi-Fi active-active
                            // delivery send the same subframe twice, and a
                            // packet that arrives after its playhead has
                            // passed cannot be heard at all.
                            if (s.wrapping_sub(next_play_seq) as i16) < 0
                                || sequencer.has_primary(s)
                            {
                                cursor += sub_len;
                                continue;
                            }

                            let mut pcm_f32 = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                            let decoded_ok = if matches!(
                                parsed.header.mode,
                                CodecMode::CompressedLowDelay | CodecMode::OpusRld
                            ) {
                                let payload = &prim[cursor..cursor + sub_len];
                                decode_compressed_payload(
                                    payload,
                                    s,
                                    &mut opus_decoder,
                                    &sequencer,
                                    next_play_seq,
                                    &mut pcm_f32,
                                )
                            } else if parsed.header.mode == CodecMode::RawPcm16 {
                                decode_raw_pcm16_payload(
                                    &prim[cursor..cursor + sub_len],
                                    &mut pcm_f32,
                                )
                            } else {
                                let payload = &prim[cursor..cursor + sub_len];
                                // True 24-bit profile: upper-16-bit LPC plus
                                // the exact 8-bit LSB plane. Falls through to
                                // the legacy 16-bit packer for all other tags.
                                if decode_lossless_pcm24_to_f32(payload, &mut pcm_f32) {
                                    true
                                } else {
                                    let mut pcm_i16 = [0i16; SUBFRAME_STEREO_SAMPLES];
                                    if decode_lossless_subframe(payload, &mut pcm_i16).is_ok() {
                                        for (dst, &s) in pcm_f32.iter_mut().zip(pcm_i16.iter()) {
                                            *dst = (s as f32) * (1.0 / 32768.0);
                                        }
                                        true
                                    } else {
                                        false
                                    }
                                }
                            };

                            if decoded_ok {
                                sequencer.push_primary(s, &pcm_f32);
                                if (s.wrapping_sub(latest_rx_seq) as i16) > 0 {
                                    latest_rx_seq = s;
                                }
                            }
                            cursor += sub_len;
                        }

                        let buffered = (latest_rx_seq.wrapping_add(1).wrapping_sub(next_play_seq)
                            as i16)
                            .max(0) as f32;
                        // Gaming lock uses the same live target as the refill
                        // loop. Waiting for the non-gaming packet floor (for
                        // example 11 subframes on a 4-subframe stream) while
                        // the target is only 7 makes the first playback pass
                        // trim several subframes immediately.
                        let prebuffer_floor =
                            if telemetry.gaming_mode_hard_lock.load(Ordering::Relaxed) {
                                gaming_base_target_subframes(
                                    last_pkt_subframe_count,
                                    jitter_estimate_us,
                                )
                            } else {
                                initial_wifi_floor
                            };
                        if buffered >= prebuffer_floor {
                            prebuffer_ready = true;
                            telemetry.prebuffer_ready.store(true, Ordering::Relaxed);
                        }
                    }
                    Err(WireError::AuthenticationFailed) => {
                        telemetry.auth_failures.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {}
                }
                if packets_drained >= 16 {
                    break;
                }
            }
            if packets_drained > 0 || refill_poll {
                let _ = socket.set_nonblocking(false);
            }

            // 4. Multi-Phone Group Sync Alignment + Competitive Gaming Hard-Lock
            let gaming_lock = telemetry.gaming_mode_hard_lock.load(Ordering::Relaxed);
            let base_target = if gaming_lock {
                gaming_base_target_subframes(last_pkt_subframe_count, jitter_estimate_us)
            } else {
                adaptive_wifi_subframes
            };
            let my_downstream_delay_ms = telemetry
                .downstream_bt_delay_ms
                .load(Ordering::Relaxed)
                .max(telemetry.downstream_hw_delay_ms.load(Ordering::Relaxed));
            // `base_target` already contains this client's adaptive network
            // jitter cushion. Group sync only aligns the fixed downstream
            // hardware/A2DP delay, so the network buffer is deliberately not
            // added here (that was the source of the 500ms oscillation).
            // Gaming hard-lock is an explicit lowest-latency, independent
            // mode: adding room-wide alignment delay would defeat it and can
            // push a 2-subframe Wi-Fi stream back into the PLC path.
            let target_buf = if gaming_lock {
                base_target
            } else {
                group_sync.aligned_target_subframes(
                    base_target,
                    telemetry.max_group_delay_ms.load(Ordering::Relaxed),
                    my_downstream_delay_ms,
                )
            };

            // Two-Level DAC Watermark:
            // 1. `fifo_watermark` holds enough ready PCM (4 to 12 subframes = 10-30ms) so neither
            //    Desktop CPAL (480-1024 frame callbacks) nor Android AAudio ever starves.
            // 2. `plc_emergency_watermark` (1 subframe = 2.5ms): If `next_play_seq` is missing in
            //    `sequencer`, we DO NOT synthesize PLC while `burst_fifo` still has >= 1 subframe!
            //    We wait for the delayed packet or Packet [K+1]'s FEC to arrive first!
            //    Gaming Mode targets 6 subframes on 2-subframe Wi-Fi, leaving
            //    (6 - 1) - 2 = 3 subframes (7.5ms) of real jitter margin instead of 0.
            let base_fifo_subframes = fifo_target_subframes(base_target);
            let target_fifo_subframes = fifo_target_subframes(target_buf);
            telemetry
                .target_buffer_us
                .store((target_fifo_subframes as u32) * 2500, Ordering::Relaxed);
            telemetry.group_sync_extra_us.store(
                (target_fifo_subframes.saturating_sub(base_fifo_subframes) as u32) * 2500,
                Ordering::Relaxed,
            );
            let fifo_capacity = burst_fifo.capacity_samples().saturating_sub(2);
            let fifo_watermark = resampler
                .subframes_to_samples(target_fifo_subframes)
                .min(fifo_capacity);
            let plc_emergency_watermark = resampler
                .subframes_to_samples(PLC_EMERGENCY_WATERMARK_SUBFRAMES)
                .min(fifo_capacity);
            last_fifo_watermark = fifo_watermark;

            while stream_initialized
                && prebuffer_ready
                && !telemetry.server_dtx_silent.load(Ordering::Relaxed)
                && burst_fifo.available_samples() < fifo_watermark
            {
                // If the missing slot is still within the in-flight/FEC
                // recovery window, wait while the FIFO has at least 2.5ms of
                // real audio. Once a newer packet is already buffered
                // (`missing_delta >= 3`), waiting can no longer heal the hole:
                // skip it immediately instead of draining the FIFO first.
                let missing_delta = latest_rx_seq.wrapping_sub(next_play_seq) as i16;
                if sequencer.is_slot_missing(next_play_seq)
                    && burst_fifo.available_samples() >= plc_emergency_watermark
                    && missing_delta < 3
                {
                    // Wait for in-flight packet or [N-1]/[N-2] FEC before triggering WSOLA PLC!
                    break;
                }
                let mut raw_subframe = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                let status = sequencer.pop_next(next_play_seq, &mut raw_subframe);

                match status {
                    STATUS_PRIMARY_LOSSLESS => {
                        next_play_seq = next_play_seq.wrapping_add(1);
                        consecutive_empty_ticks = 0;
                        window_primary += 1;
                        telemetry
                            .primary_frames_played
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    STATUS_HEALED_BY_FEC => {
                        next_play_seq = next_play_seq.wrapping_add(1);
                        consecutive_empty_ticks = 0;
                        window_fec += 1;
                        // 2.4GHz Wi-Fi jitter/drop detected: expand cushion by +0.35 subframes (max 14.0 = 35.0ms)
                        adaptive_wifi_subframes = (adaptive_wifi_subframes + 0.35).min(14.0);
                        telemetry.fec_healed_frames.fetch_add(1, Ordering::Relaxed);
                    }
                    STATUS_EMPTY => {
                        // `pop_next` has already synthesized one PLC subframe
                        // for this playhead and rendered it into the decoupler.
                        // Holding the playhead here would play a late primary
                        // packet a second time (comb filtering plus permanent
                        // +2.5 ms queue inflation), so advance exactly one
                        // slot. The late packet then fails the
                        // `s < next_play_seq` test and is dropped, which is the
                        // right tradeoff once its slot has been concealed.
                        next_play_seq = next_play_seq.wrapping_add(1);
                        consecutive_empty_ticks = consecutive_empty_ticks.saturating_add(1);
                        window_fec += 1;
                        // 2.4GHz burst delay exceeded cushion: expand cushion by +0.75 subframes (max 14.0 = 35.0ms)
                        adaptive_wifi_subframes = (adaptive_wifi_subframes + 0.75).min(14.0);
                        telemetry
                            .plc_extrapolated_frames
                            .fetch_add(1, Ordering::Relaxed);

                        // If Host disconnected or Wi-Fi dropped for >200ms (80 PLC
                        // subframes), suspend into clean standby. 60ms was too
                        // aggressive for 2.4GHz bursts and caused false standby.
                        if consecutive_empty_ticks >= 80 {
                            prebuffer_ready = false;
                            consecutive_empty_ticks = 0;
                        }
                    }
                    _ => {
                        next_play_seq = next_play_seq.wrapping_add(1);
                        consecutive_empty_ticks = 0;
                    }
                }

                // 1. Apply 3-Band Studio Parametric EQ (0.00ms delay)
                studio_eq.update_if_changed(
                    telemetry.eq_bass_db_tenths.load(Ordering::Relaxed),
                    telemetry.eq_presence_db_tenths.load(Ordering::Relaxed),
                    telemetry.eq_treble_db_tenths.load(Ordering::Relaxed),
                );
                studio_eq.process(&mut raw_subframe);

                // 2. Apply Spatial Channel Role (Stereo / Left / Right / Center / Surround)
                let role = audioshare_protocol::sync_and_eq::ChannelRole::from_u8(
                    telemetry.channel_role.load(Ordering::Relaxed),
                );
                role.apply_in_place(&mut raw_subframe);

                // 3. Apply Audiophile Bauer Headphone Crossfeed if enabled (0.00ms delay)
                if telemetry.enable_binaural_crossfeed.load(Ordering::Relaxed) {
                    crossfeed.process_in_place(&mut raw_subframe);
                }

                // 3b. Apply Night-Mode Upward Whisper Compressor & Explosion Tamer (0.00ms delay)
                if telemetry.enable_night_mode.load(Ordering::Relaxed) {
                    night_compressor.process_in_place(&mut raw_subframe);
                }

                let gain_db =
                    (telemetry.preamp_gain_db_tenths.load(Ordering::Relaxed) as f32) / 10.0;
                limiter.set_gain_db(gain_db);
                limiter.process_in_place(&mut raw_subframe);

                // Apply Host Master Volume (0..=100%)
                let host_vol = (telemetry.host_volume.load(Ordering::Relaxed) as f32) / 100.0;
                if (host_vol - 1.0).abs() > 0.005 {
                    for s in &mut raw_subframe {
                        *s *= host_vol;
                    }
                }

                // Move the popped subframe through the resampler and into the
                // decoupler before measuring either queue. Measuring first
                // counted this subframe in neither `net_subframes` (the
                // playhead had already advanced) nor `fifo_subframes` (not
                // pushed yet), leaving a permanent -1 subframe bias that the
                // clock loop then "corrected" with negative drift.
                let written_frames = resampler.process(&raw_subframe, &mut resampled_out);
                burst_fifo.push_samples(&resampled_out[..written_frames * 2]);

                // Measure true network sequencer queue depth (Host Clock vs Client DAC Clock).
                // The decoupler is filled at the output rate, so convert its
                // sample count through the active base ratio instead of
                // assuming 48 kHz.
                let fifo_subframes = resampler.samples_to_subframes(burst_fifo.available_samples());
                let mut net_subframes = (latest_rx_seq.wrapping_add(1).wrapping_sub(next_play_seq)
                    as i16)
                    .max(0) as f32;
                // Cap the post-stall Hold-Playhead backlog at `target_buf + 6`
                // subframes (15ms) instead of the old +16 (40ms), so 24-bit and
                // 2.4GHz streams never accumulate permanent latency after a
                // Wi-Fi stall. Only the sequencer share can be shortened; the
                // decoupler is already feeding the DAC.
                trim_post_stall_backlog(
                    latest_rx_seq,
                    &mut next_play_seq,
                    &mut net_subframes,
                    fifo_subframes,
                    target_buf,
                );

                // Never synthesize more than one PLC subframe per loop pass
                // without first polling the socket again. The previous code
                // could fill the whole FIFO with PLC in one CPU-speed pass,
                // advancing the playhead multiple slots ahead of the network.
                if status == STATUS_EMPTY {
                    break;
                }
            }

            // Feed the clock-drift controller once per pass, using the planned
            // cushion rather than the decoupler level the output callback just
            // drained. A whole AAudio/CPAL burst (4-20 ms on Bluetooth) makes
            // the instantaneous combined depth look permanently low even when
            // the network clock is locked; see `clock_loop_observation`.
            if stream_initialized
                && prebuffer_ready
                && !telemetry.server_dtx_silent.load(Ordering::Relaxed)
            {
                let fifo_subframes = resampler.samples_to_subframes(burst_fifo.available_samples());
                let net_subframes = (latest_rx_seq.wrapping_add(1).wrapping_sub(next_play_seq)
                    as i16)
                    .max(0) as f32;
                telemetry.buffered_audio_us.store(
                    ((net_subframes + fifo_subframes).max(0.0) * 2500.0) as u32,
                    Ordering::Relaxed,
                );
                resampler.update_clock_loop(
                    clock_loop_observation(net_subframes, target_buf),
                    target_buf,
                );
                let ppm = (((resampler.ratio / resampler.base_ratio) - 1.0) * 1_000_000.0) as i32;
                telemetry.clock_drift_ppm.store(ppm, Ordering::Relaxed);
            }

            // Publish the adaptive cushion and prebuffer state for the live
            // debug views. The audio path itself never reads these atomics.
            telemetry.adaptive_buffer_us.store(
                (adaptive_wifi_subframes.max(0.0) * 2500.0) as u32,
                Ordering::Relaxed,
            );
            telemetry
                .prebuffer_ready
                .store(prebuffer_ready, Ordering::Relaxed);
        }
    }
}

impl Drop for ClientEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signed_sequence_delta_rfc1982() {
        // Late duplicate packet arrived after playback moved ahead
        let next_play_seq = 101u16;
        let late_seq = 99u16;
        let delta = late_seq.wrapping_sub(next_play_seq) as i16;
        assert_eq!(delta, -2);
        assert!(delta < 0, "Duplicate/past packet must have negative delta");

        // Forward sequence wrap-around across 65535 boundary
        let next_play_seq = 65535u16;
        let future_seq = 2u16;
        let delta = future_seq.wrapping_sub(next_play_seq) as i16;
        assert_eq!(delta, 3);
        assert!(
            delta > 0,
            "Future packet across wrap boundary must have positive delta"
        );

        // Past sequence across wrap-around boundary
        let next_play_seq = 2u16;
        let past_seq = 65535u16;
        let delta = past_seq.wrapping_sub(next_play_seq) as i16;
        assert_eq!(delta, -3);
        assert!(
            delta < 0,
            "Past packet across wrap boundary must have negative delta"
        );

        // Normal buffered calculation
        let latest_rx = 50u16;
        let next_play = 46u16;
        let buffered = (latest_rx.wrapping_sub(next_play) as i16).max(0) as f32;
        assert_eq!(buffered, 4.0);

        // Negative buffered (late packet or race condition) clamped to 0
        let latest_rx = 40u16;
        let next_play = 45u16;
        let buffered = (latest_rx.wrapping_sub(next_play) as i16).max(0) as f32;
        assert_eq!(buffered, 0.0);
    }

    #[test]
    fn test_instant_burst_spike_fast_forward() {
        let latest_rx = 100u16;
        let mut next_play = 70u16; // 30 subframes lag (>20ms lag)
        let target_buf = 4.0f32;
        let fifo_subframes = 4.0f32;

        let mut net_subframes = (latest_rx.wrapping_sub(next_play) as i16).max(0) as f32;
        assert_eq!(net_subframes, 30.0);

        trim_post_stall_backlog(
            latest_rx,
            &mut next_play,
            &mut net_subframes,
            fifo_subframes,
            target_buf,
        );

        // The combined sequencer + decoupler backlog is capped at
        // `target_buf + 6` = 10 subframes; the sequencer keeps the remaining
        // 10 - 4 = 6.
        assert_eq!(net_subframes, 6.0);
        assert_eq!(net_subframes + fifo_subframes, 10.0);
        assert_eq!(next_play, 95);
    }

    #[test]
    fn test_gaming_target_respects_packet_size_and_jitter() {
        // Old gaming target (4 subframes) with the old 2-subframe PLC
        // watermark left zero margin for a 2-subframe Wi-Fi packet, so every
        // late packet triggered WSOLA PLC. Keep this regression explicit.
        let old_margin = fifo_target_subframes(4.0) as i32 - 2 - 2;
        assert_eq!(old_margin, 0);

        // One-subframe USB/desktop framing: 4 subframes (10 ms) leaves
        // (4 - 1) - 1 = 2 subframes (5 ms) above the PLC watermark.
        assert_eq!(gaming_base_target_subframes(1, 2500.0), 4.0);
        let usb_target = fifo_target_subframes(gaming_base_target_subframes(1, 2500.0));
        assert_eq!(usb_target, 4);
        assert_eq!(plc_jitter_margin_subframes(usb_target, 1), 2);

        // Two-subframe Wi-Fi framing: 6 subframes (15 ms) leaves
        // (6 - 1) - 2 = 3 subframes (7.5 ms) of margin.
        assert_eq!(gaming_base_target_subframes(2, 2500.0), 6.0);
        let wifi_target = fifo_target_subframes(gaming_base_target_subframes(2, 2500.0));
        assert_eq!(wifi_target, 6);
        assert_eq!(plc_jitter_margin_subframes(wifi_target, 2), 3);

        // Four-subframe 2.4 GHz framing must not be clamped below its own
        // packet size. 7 subframes (17.5 ms) covers one 10 ms packet plus the
        // PLC watermark and one subframe of scheduling margin.
        assert_eq!(gaming_base_target_subframes(4, 2500.0), 7.0);
        assert_eq!(
            fifo_target_subframes(gaming_base_target_subframes(4, 2500.0)),
            7
        );

        // Excess measured jitter adds cushion above the baseline packet
        // interval, but the hard-lock target never exceeds four packets
        // worth (8 subframes = 20 ms).
        assert!((gaming_base_target_subframes(2, 5000.0) - 7.4).abs() < 1e-4);
        assert_eq!(gaming_base_target_subframes(4, 60_000.0), 8.0);
        assert_eq!(gaming_base_target_subframes(2, 0.0), 6.0);
    }

    #[test]
    fn test_post_stall_trim_caps_combined_backlog_and_never_rewinds() {
        // Gaming Wi-Fi: 6-subframe target with a full 6-subframe decoupler.
        let mut next_play = 70u16;
        let mut net_subframes = 20.0f32;
        trim_post_stall_backlog(100, &mut next_play, &mut net_subframes, 6.0, 6.0);
        assert_eq!(net_subframes, 6.0);
        assert_eq!(next_play, 95);
        assert_eq!(net_subframes + 6.0, 12.0); // target_buf + 6

        // 2.4GHz normal mode: a 14-subframe target with the decoupler capped
        // at 12. The trim must account for the decoupler, not just reset the
        // sequencer to the target (which would retain 26 subframes).
        let mut next_play = 50u16;
        let mut net_subframes = 30.0f32;
        trim_post_stall_backlog(100, &mut next_play, &mut net_subframes, 12.0, 14.0);
        assert_eq!(net_subframes, 8.0);
        assert_eq!(next_play, 93);
        assert_eq!(net_subframes + 12.0, 20.0); // target_buf + 6

        // At or below the cap the playhead and queue are untouched.
        let mut next_play = 90u16;
        let mut net_subframes = 6.0f32;
        trim_post_stall_backlog(100, &mut next_play, &mut net_subframes, 6.0, 6.0);
        assert_eq!(next_play, 90);
        assert_eq!(net_subframes, 6.0);

        // A normal 2-packet Wi-Fi A-MPDU burst (2 + 2 subframes) arriving on
        // top of a full decoupler stays inside target + 6 and must be
        // absorbed by the resampler, not skipped as a post-stall backlog.
        let mut next_play = 97u16;
        let mut net_subframes = 4.0f32;
        trim_post_stall_backlog(100, &mut next_play, &mut net_subframes, 6.0, 6.0);
        assert_eq!(next_play, 97);
        assert_eq!(net_subframes, 4.0);
        assert_eq!(net_subframes + 6.0, 10.0); // <= target_buf + 6

        // Never rewind, even if the FIFO estimate alone exceeds the cap.
        let mut next_play = 99u16;
        let mut net_subframes = 2.0f32;
        trim_post_stall_backlog(100, &mut next_play, &mut net_subframes, 20.0, 6.0);
        assert_eq!(next_play, 101);
        assert_eq!(net_subframes, 0.0);

        // Sequence wrap-around keeps the same forward-only invariant.
        let mut next_play = 65533u16;
        let mut net_subframes = 7.0f32;
        trim_post_stall_backlog(3, &mut next_play, &mut net_subframes, 6.0, 6.0);
        assert_eq!(net_subframes, 6.0);
        assert_eq!(next_play, 65534);
    }

    #[test]
    fn test_pending_control_commands_do_not_clobber_each_other() {
        let media = AtomicU8::new(0);
        let bitrate = AtomicU8::new(0);
        let codec = AtomicU8::new(0);
        let gaming = AtomicU8::new(0);

        // Startup restores the quality profile (1..=5), the bitrate (30..=33)
        // and the codec backend (20/21) back-to-back. All must survive.
        media.store(4, Ordering::Relaxed);
        bitrate.store(33, Ordering::Relaxed);
        codec.store(21, Ordering::Relaxed);
        assert_eq!(take_pending_command(&media, &bitrate, &codec, &gaming), 4);
        assert_eq!(take_pending_command(&media, &bitrate, &codec, &gaming), 33);
        assert_eq!(take_pending_command(&media, &bitrate, &codec, &gaming), 21);
        assert_eq!(take_pending_command(&media, &bitrate, &codec, &gaming), 0);

        // A bitrate-only change is sent immediately.
        bitrate.store(30, Ordering::Relaxed);
        assert_eq!(take_pending_command(&media, &bitrate, &codec, &gaming), 30);

        // A media key still takes priority over the other pending controls.
        media.store(11, Ordering::Relaxed);
        bitrate.store(31, Ordering::Relaxed);
        codec.store(20, Ordering::Relaxed);
        assert_eq!(take_pending_command(&media, &bitrate, &codec, &gaming), 11);
        assert_eq!(take_pending_command(&media, &bitrate, &codec, &gaming), 31);
        assert_eq!(take_pending_command(&media, &bitrate, &codec, &gaming), 20);

        // Gaming lock is last: a periodic refresh must not delay a one-shot
        // user command, but it is still delivered when it is the only slot.
        media.store(2, Ordering::Relaxed);
        gaming.store(41, Ordering::Relaxed);
        assert_eq!(take_pending_command(&media, &bitrate, &codec, &gaming), 2);
        assert_eq!(take_pending_command(&media, &bitrate, &codec, &gaming), 41);
        assert_eq!(take_pending_command(&media, &bitrate, &codec, &gaming), 0);

        // The latest toggle wins when both states are pending.
        gaming.store(40, Ordering::Relaxed);
        gaming.store(41, Ordering::Relaxed);
        assert_eq!(take_pending_command(&media, &bitrate, &codec, &gaming), 41);
    }

    #[test]
    fn test_adaptive_wifi_cushion_expansion_and_decay() {
        let mut adaptive_wifi_subframes = 5.5f32;
        let initial_floor = 5.5f32;

        // On FEC heal: +0.35
        adaptive_wifi_subframes = (adaptive_wifi_subframes + 0.35).min(12.0);
        assert!((adaptive_wifi_subframes - 5.85).abs() < 1e-4);

        // On empty gap (PLC): +0.75
        adaptive_wifi_subframes = (adaptive_wifi_subframes + 0.75).min(12.0);
        assert!((adaptive_wifi_subframes - 6.60).abs() < 1e-4);

        // On clean 500ms window: decay by 0.15 down to floor
        adaptive_wifi_subframes = (adaptive_wifi_subframes - 0.15).max(initial_floor);
        assert!((adaptive_wifi_subframes - 6.45).abs() < 1e-4);

        // Ceiling cap at 12.0
        for _ in 0..20 {
            adaptive_wifi_subframes = (adaptive_wifi_subframes + 0.75).min(12.0);
        }
        assert_eq!(adaptive_wifi_subframes, 12.0);
    }

    #[test]
    fn test_adaptive_cushion_decays_under_minor_fec_loss() {
        let floor = 6.0f32;
        let mut adaptive = 14.0f32;

        // 0.5% background FEC recovery = 5 permille. The old `== 0`
        // condition held the cushion at 35 ms forever.
        for _ in 0..40 {
            adaptive = decay_adaptive_cushion(adaptive, floor, 5);
        }
        assert!(
            adaptive < 7.0,
            "minor loss must not pin the cushion: {}",
            adaptive
        );

        // A genuinely lossy window (4%) holds the current cushion.
        assert_eq!(decay_adaptive_cushion(10.0, floor, 40), 10.0);

        // The floor is a hard lower bound.
        let mut at_floor = floor;
        for _ in 0..100 {
            at_floor = decay_adaptive_cushion(at_floor, floor, 0);
        }
        assert_eq!(at_floor, floor);

        // The first healthy step removes most of one subframe, not 0.15.
        let first = decay_adaptive_cushion(14.0, floor, 0);
        assert!(first < 13.2, "first decay step is too slow: {}", first);
    }

    #[test]
    fn test_clock_loop_ignores_output_burst_troughs() {
        // A 20 ms A2DP callback drains six subframes from a six-subframe
        // decoupler at once, then the refill loop observes levels 1..=6.
        // Feeding that sawtooth to the old windowed-min controller made it
        // compare "post-burst low" with the mean target.
        let target = 6.0f32;
        let mut old = ClockDriftResampler::new();
        for _ in 0..200 {
            for level in 1..=6 {
                old.update_clock_loop(level as f32, target);
            }
        }
        let old_ppm = ((old.ratio - 1.0) * 1_000_000.0) as i32;
        assert!(
            old_ppm < -50,
            "trough observation should bias the old loop slow, got {} ppm",
            old_ppm
        );

        // The fixed loop regulates sequencer backlog + target. A network that
        // keeps up keeps the observation at target even while the decoupler
        // dips after every callback, so the ratio stays at the base rate.
        let mut fixed = ClockDriftResampler::new();
        for _ in 0..1200 {
            fixed.update_clock_loop(clock_loop_observation(0.0, target), target);
        }
        let fixed_ppm = ((fixed.ratio - 1.0) * 1_000_000.0) as i32;
        assert_eq!(
            fixed_ppm, 0,
            "structural output bursts must not change the clock ratio"
        );
    }

    #[test]
    fn test_i16_output_conversion_clamping() {
        let f32_samples: [f32; 4] = [1.5, -1.5, 0.0, 0.5];
        let mut i16_samples = [0i16; 4];
        for (dst, &s) in i16_samples.iter_mut().zip(f32_samples.iter()) {
            *dst = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        }
        assert_eq!(i16_samples[0], 32767);
        assert_eq!(i16_samples[1], -32767);
        assert_eq!(i16_samples[2], 0);
        assert_eq!(i16_samples[3], 16383);
    }

    #[test]
    fn test_plc_advance_drops_late_primary_instead_of_duplicating() {
        // `pop_next` returned STATUS_EMPTY for slot 101, so the refill loop
        // rendered 2.5 ms of PLC and advanced the playhead to 102. The late
        // primary for 101 must now be rejected as "past"; playing it after its
        // concealed slot would duplicate 2.5 ms and permanently inflate the
        // queue.
        let next_play_after_plc = 102u16;
        let late_seq = 101u16;
        assert!((late_seq.wrapping_sub(next_play_after_plc) as i16) < 0);

        // The next genuinely new packet is still accepted.
        let next_seq = 102u16;
        assert_eq!(next_seq.wrapping_sub(next_play_after_plc) as i16, 0);

        // Sequence wrap-around keeps the same classification.
        let next_play_after_plc = 1u16;
        assert_eq!(0u16.wrapping_sub(next_play_after_plc) as i16, -1);
        assert_eq!(1u16.wrapping_sub(next_play_after_plc) as i16, 0);
    }

    #[test]
    fn test_aaudio_timestamp_extrapolation_prevents_fake_latency() {
        let rate = SAMPLE_RATE as i64;
        let timestamp_ns = 1_000_000_000i64;
        // Timestamp snapshot says frame 48_000 was presented 10ms ago.
        // The DAC has therefore presented ~48_480 frames by "now".
        let now_ns = timestamp_ns + 10_000_000;
        let frames_written = 48_000 + 480 + 240; // 240 frames = 5ms still queued
        let delay = pending_hardware_delay_ms(48_000, timestamp_ns, now_ns, frames_written, 48_000);
        assert_eq!(delay, Some(5));

        // Without extrapolation the same snapshot would report 15ms.
        let unextrapolated = ((frames_written - 48_000) * 1000) / rate;
        assert_eq!(unextrapolated, 15);

        // A 44.1 kHz A2DP stream must use its actual rate. 441 frames of
        // pending audio are 10 ms, not 441 / 48 kHz.
        let delay = pending_hardware_delay_ms(44_100, timestamp_ns, timestamp_ns, 44_541, 44_100);
        assert_eq!(delay, Some(10));

        // A future timestamp (clock anomaly) must not underflow the subtraction.
        let delay = pending_hardware_delay_ms(
            48_000,
            timestamp_ns + 5_000_000,
            timestamp_ns,
            48_100,
            48_000,
        );
        assert_eq!(delay, Some(2));

        // Invalid snapshots are rejected instead of producing fake latency.
        assert_eq!(
            pending_hardware_delay_ms(-1, timestamp_ns, now_ns, 48_000, 48_000),
            None
        );
        assert_eq!(
            pending_hardware_delay_ms(0, 0, now_ns, 48_000, 48_000),
            None
        );

        // Very stale timestamps are bounded so a broken HAL cannot report
        // hundreds of milliseconds of fake hardware latency.
        let stale = pending_hardware_delay_ms(
            0,
            timestamp_ns,
            timestamp_ns + 5_000_000_000,
            48_000,
            48_000,
        );
        assert_eq!(stale, Some(120));
    }

    #[test]
    fn test_aaudio_large_bluetooth_bursts_are_not_tripled() {
        // Speaker/USB bursts keep the verified 3x cushion, minimum 256.
        assert_eq!(aaudio_buffer_frames_for_burst(48), Some(256));
        assert_eq!(aaudio_buffer_frames_for_burst(96), Some(288));
        assert_eq!(aaudio_buffer_frames_for_burst(192), Some(576));
        // A2DP bursts are already 5-20 ms; one burst is the minimum AAudio
        // can use, and multiplying it would add 10-40 ms of dead latency.
        assert_eq!(aaudio_buffer_frames_for_burst(240), Some(240));
        assert_eq!(aaudio_buffer_frames_for_burst(480), Some(480));
        assert_eq!(aaudio_buffer_frames_for_burst(960), Some(960));
        assert_eq!(aaudio_buffer_frames_for_burst(0), None);
        assert_eq!(aaudio_buffer_frames_for_burst(-1), None);
    }

    #[test]
    fn test_compressed_payload_auto_detects_opus_and_vbr() {
        use audioshare_protocol::lpc_lossless::{encode_vbr_subframe, MAX_LOSSLESS_SUBFRAME_BYTES};
        use audioshare_protocol::opus_codec::OpusStreamEncoder;

        let mut pcm = [0i16; SUBFRAME_STEREO_SAMPLES];
        for i in 0..(SUBFRAME_STEREO_SAMPLES / 2) {
            let t = i as f32 / SAMPLE_RATE as f32;
            pcm[i * 2] = ((t * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
            pcm[i * 2 + 1] = ((t * 554.37 * std::f32::consts::TAU).sin() * 8000.0) as i16;
        }

        let mut opus_encoder = OpusStreamEncoder::new(128).expect("create Opus encoder");
        let mut opus_payload = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
        let opus_len = opus_encoder
            .encode_subframe(&pcm, 128, &mut opus_payload)
            .expect("encode Opus");
        assert_eq!(opus_payload[0], TAG_OPUS_RLD);

        let mut decoder = None;
        let sequencer = ZeroJitterSequencer::new();
        let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        assert!(decode_compressed_payload(
            &opus_payload[..opus_len],
            10,
            &mut decoder,
            &sequencer,
            10,
            &mut out
        ));
        assert!(decoder.is_some(), "Opus tag must create the decoder");

        let mut vbr_payload = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
        let vbr_len = encode_vbr_subframe(&pcm, 160, &mut vbr_payload);
        assert_eq!(vbr_payload[0], TAG_VBR_BFP);
        let mut vbr_out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        assert!(decode_compressed_payload(
            &vbr_payload[..vbr_len],
            11,
            &mut decoder,
            &sequencer,
            11,
            &mut vbr_out
        ));
        assert!(
            decoder.is_none(),
            "VBR payload must reset stale Opus decoder state"
        );

        // Switching back to Opus must lazily recreate the decoder.
        let mut opus_out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        assert!(decode_compressed_payload(
            &opus_payload[..opus_len],
            12,
            &mut decoder,
            &sequencer,
            12,
            &mut opus_out
        ));
        assert!(decoder.is_some());
    }

    #[test]
    fn test_opus_gap_is_healed_with_phase_aligned_plc() {
        use audioshare_protocol::lpc_lossless::MAX_LOSSLESS_SUBFRAME_BYTES;
        use audioshare_protocol::opus_codec::OpusStreamEncoder;

        let mut pcm = [0i16; SUBFRAME_STEREO_SAMPLES];
        for i in 0..(SUBFRAME_STEREO_SAMPLES / 2) {
            let t = i as f32 / SAMPLE_RATE as f32;
            pcm[i * 2] = ((t * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
            pcm[i * 2 + 1] = ((t * 554.37 * std::f32::consts::TAU).sin() * 8000.0) as i16;
        }

        let mut encoder = OpusStreamEncoder::new(128).expect("create Opus encoder");
        let mut payload = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
        let mut decoder = None;
        let mut sequencer = ZeroJitterSequencer::new();
        let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];

        let len = encoder.encode_subframe(&pcm, 128, &mut payload).unwrap();
        assert!(decode_compressed_payload(
            &payload[..len],
            100,
            &mut decoder,
            &sequencer,
            100,
            &mut out
        ));

        // Packet 103 arrives without 101/102. The decoder's CELT PLC must
        // populate both holes so the playhead never falls back to WSOLA or
        // to the zero-lookahead ADPCM backup.
        let len = encoder.encode_subframe(&pcm, 128, &mut payload).unwrap();
        assert!(decode_compressed_payload(
            &payload[..len],
            103,
            &mut decoder,
            &sequencer,
            100,
            &mut out
        ));
        assert!(!sequencer.is_slot_missing(101));
        assert!(!sequencer.is_slot_missing(102));

        let mut healed = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        assert_eq!(sequencer.pop_next(101, &mut healed), STATUS_HEALED_BY_FEC);
        assert!(healed.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn test_primary_is_opus_detects_header_and_payload_tag() {
        let tagged = [0u8, 2, TAG_OPUS_RLD, 0xAA];
        let vbr = [0u8, 2, TAG_VBR_BFP, 0xAA];

        assert!(primary_is_opus(CodecMode::OpusRld, &[]));
        assert!(primary_is_opus(CodecMode::CompressedLowDelay, &tagged));
        assert!(!primary_is_opus(CodecMode::CompressedLowDelay, &vbr));
        assert!(!primary_is_opus(CodecMode::CompressedLowDelay, &[]));
    }

    #[test]
    fn test_stream_resync_thresholds_detect_host_restart() {
        assert!(!needs_stream_resync(0));
        assert!(!needs_stream_resync(48));
        assert!(needs_stream_resync(49));
        assert!(!needs_stream_resync(-32));
        assert!(needs_stream_resync(-33));

        // Host restarted while the playhead was at 150: the new stream starts
        // at sequence 0 and must be accepted immediately, not after 375 ms.
        let next_play = 150u16;
        let new_host_base = 0u16;
        let delta = new_host_base.wrapping_sub(next_play) as i16;
        assert!(needs_stream_resync(delta));

        // A packet still inside the 80 ms window is a late duplicate, not a
        // new stream.
        let late = 140u16;
        let delta = late.wrapping_sub(next_play) as i16;
        assert!(!needs_stream_resync(delta));
    }

    #[test]
    fn test_raw_pcm16_accepts_tagged_and_untagged_frames() {
        let mut raw = [0u8; SUBFRAME_STEREO_SAMPLES * 2];
        for i in 0..SUBFRAME_STEREO_SAMPLES {
            let s = ((i as i16) - 120) * 200;
            raw[i * 2..i * 2 + 2].copy_from_slice(&s.to_le_bytes());
        }

        let mut decoded = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        assert!(decode_raw_pcm16_payload(&raw, &mut decoded));
        for i in 0..SUBFRAME_STEREO_SAMPLES {
            let expected = (((i as i16) - 120) * 200) as f32 / 32768.0;
            assert!((decoded[i] - expected).abs() < 1e-7);
        }

        // The lossless encoder's raw fallback carries a 0x00 tag byte.
        let mut tagged = [0u8; 481];
        tagged[1..].copy_from_slice(&raw);
        decoded.fill(0.0);
        assert!(decode_raw_pcm16_payload(&tagged, &mut decoded));
        for i in 0..SUBFRAME_STEREO_SAMPLES {
            let expected = (((i as i16) - 120) * 200) as f32 / 32768.0;
            assert!((decoded[i] - expected).abs() < 1e-7);
        }

        // Truncated payloads are rejected rather than producing partial audio.
        assert!(!decode_raw_pcm16_payload(
            &raw[..raw.len() - 2],
            &mut decoded
        ));
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn test_i16_output_fills_buffers_larger_than_one_chunk() {
        let ring = BurstDecouplerRing::new();
        let mut input = [0.0f32; 5000];
        for (i, s) in input.iter_mut().enumerate() {
            *s = ((i % 100) as f32 - 50.0) / 100.0;
        }
        assert_eq!(ring.push_samples(&input), input.len());

        let mut out = [i16::MIN; 5000];
        let mut rng = 1u32;
        ClientEngine::fill_i16_output(&ring, &mut out, &mut rng);

        for (i, &sample) in input.iter().enumerate() {
            let expected = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
            assert_eq!(out[i], expected, "unfilled sample at {}", i);
        }
    }
}

/// Direct Zero-Dependency Android NDK `libaaudio.so` Hardware MMAP Binding
#[cfg(target_os = "android")]
pub mod android_aaudio {
    use super::*;
    use core::ffi::c_void;

    #[repr(C)]
    pub struct AAudioStreamBuilder(c_void);
    #[repr(C)]
    pub struct AAudioStream(c_void);

    const AAUDIO_DIRECTION_OUTPUT: i32 = 0;
    const AAUDIO_DIRECTION_INPUT: i32 = 1;
    const AAUDIO_FORMAT_PCM_FLOAT: i32 = 2;
    const AAUDIO_UNSPECIFIED: i32 = 0;
    const AAUDIO_SHARING_MODE_EXCLUSIVE: i32 = 0;
    const AAUDIO_SHARING_MODE_SHARED: i32 = 1;
    const AAUDIO_PERFORMANCE_MODE_NONE: i32 = 10;
    const AAUDIO_PERFORMANCE_MODE_LOW_LATENCY: i32 = 12;
    const AAUDIO_USAGE_MEDIA: i32 = 1;
    const AAUDIO_USAGE_GAME: i32 = 14;
    const AAUDIO_CONTENT_TYPE_MUSIC: i32 = 2;
    const AAUDIO_CONTENT_TYPE_SONIFICATION: i32 = 4;
    const AAUDIO_SPATIALIZATION_BEHAVIOR_AUTO: i32 = 1;
    const AAUDIO_SPATIALIZATION_BEHAVIOR_NEVER: i32 = 2;
    const AAUDIO_CALLBACK_RESULT_CONTINUE: i32 = 0;

    /// 960 frames fits 3x a 240-frame speaker burst (720) and one full
    /// 960-frame Bluetooth burst. Leaving the capacity unspecified lets some
    /// HALs allocate a 1920-3840-frame ring, which excludes the stream from
    /// the low-latency mixer path.
    const AAUDIO_BUFFER_CAPACITY_FRAMES: i32 = 960;

    #[derive(Clone, Copy)]
    struct AAudioProfile {
        sharing_mode: i32,
        performance_mode: i32,
        usage: i32,
        content_type: i32,
        spatialization_behavior: i32,
    }

    /// Raw path: exclusive MMAP first, game/sonification attributes, and
    /// spatialization explicitly disabled. Exclusive mode bypasses
    /// AudioFlinger effects entirely; the shared fallback still carries the
    /// anti-spatialization flags so OEM post-processing does not attach.
    const RAW_PROFILE: AAudioProfile = AAudioProfile {
        sharing_mode: AAUDIO_SHARING_MODE_EXCLUSIVE,
        performance_mode: AAUDIO_PERFORMANCE_MODE_LOW_LATENCY,
        usage: AAUDIO_USAGE_GAME,
        content_type: AAUDIO_CONTENT_TYPE_SONIFICATION,
        spatialization_behavior: AAUDIO_SPATIALIZATION_BEHAVIOR_NEVER,
    };

    const RAW_SHARED_PROFILE: AAudioProfile = AAudioProfile {
        sharing_mode: AAUDIO_SHARING_MODE_SHARED,
        performance_mode: AAUDIO_PERFORMANCE_MODE_LOW_LATENCY,
        usage: AAUDIO_USAGE_GAME,
        content_type: AAUDIO_CONTENT_TYPE_SONIFICATION,
        spatialization_behavior: AAUDIO_SPATIALIZATION_BEHAVIOR_NEVER,
    };

    /// Dolby/system-effects path: the shared mixer is mandatory because
    /// exclusive MMAP streams cannot be processed by AudioFlinger effects.
    /// `PERFORMANCE_MODE_NONE` selects the normal mixer where OEM Dolby,
    /// Dirac, and system equalizers live; the low-latency shared profile is
    /// only a fallback for devices that reject `NONE`.
    const DOLBY_PROFILE: AAudioProfile = AAudioProfile {
        sharing_mode: AAUDIO_SHARING_MODE_SHARED,
        performance_mode: AAUDIO_PERFORMANCE_MODE_NONE,
        usage: AAUDIO_USAGE_MEDIA,
        content_type: AAUDIO_CONTENT_TYPE_MUSIC,
        spatialization_behavior: AAUDIO_SPATIALIZATION_BEHAVIOR_AUTO,
    };

    const DOLBY_LOW_LATENCY_PROFILE: AAudioProfile = AAudioProfile {
        sharing_mode: AAUDIO_SHARING_MODE_SHARED,
        performance_mode: AAUDIO_PERFORMANCE_MODE_LOW_LATENCY,
        usage: AAUDIO_USAGE_MEDIA,
        content_type: AAUDIO_CONTENT_TYPE_MUSIC,
        spatialization_behavior: AAUDIO_SPATIALIZATION_BEHAVIOR_AUTO,
    };

    type SetI32Fn = unsafe extern "C" fn(*mut AAudioStreamBuilder, i32);
    type SetBoolFn = unsafe extern "C" fn(*mut AAudioStreamBuilder, bool);

    static AAUDIO_LIB_HANDLE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

    /// Resolve an AAudio builder setter by name. Usage, content type, and
    /// spatialization were added after the API 26 AAudio base, so a missing
    /// symbol must degrade gracefully instead of blocking the stream open.
    #[inline]
    unsafe fn aaudio_builder_symbol(name: &[u8]) -> Option<*mut c_void> {
        let handle = *AAUDIO_LIB_HANDLE.get_or_init(|| {
            libc::dlopen(
                b"libaaudio.so\0".as_ptr() as *const libc::c_char,
                libc::RTLD_NOW,
            ) as usize
        });
        if handle == 0 {
            return None;
        }
        let sym = libc::dlsym(handle as *mut c_void, name.as_ptr() as *const libc::c_char);
        if sym.is_null() {
            None
        } else {
            Some(sym)
        }
    }

    #[inline]
    unsafe fn apply_aaudio_profile(builder: *mut AAudioStreamBuilder, profile: AAudioProfile) {
        if let Some(sym) = aaudio_builder_symbol(b"AAudioStreamBuilder_setUsage\0") {
            let f: SetI32Fn = core::mem::transmute(sym);
            f(builder, profile.usage);
        }
        if let Some(sym) = aaudio_builder_symbol(b"AAudioStreamBuilder_setContentType\0") {
            let f: SetI32Fn = core::mem::transmute(sym);
            f(builder, profile.content_type);
        }
        if let Some(sym) = aaudio_builder_symbol(b"AAudioStreamBuilder_setSpatializationBehavior\0")
        {
            let f: SetI32Fn = core::mem::transmute(sym);
            f(builder, profile.spatialization_behavior);
        }
        if let Some(sym) = aaudio_builder_symbol(b"AAudioStreamBuilder_setIsContentSpatialized\0") {
            let f: SetBoolFn = core::mem::transmute(sym);
            // The engine always supplies plain stereo PCM, never Ambisonic or
            // object audio. The behavior above decides whether the platform
            // spatializer is allowed to process it.
            f(builder, false);
        }
    }

    type AAudioDataCallback = extern "C" fn(
        stream: *mut AAudioStream,
        user_data: *mut c_void,
        audio_data: *mut c_void,
        num_frames: i32,
    ) -> i32;

    type AAudioErrorCallback =
        extern "C" fn(stream: *mut AAudioStream, user_data: *mut c_void, error: i32);

    #[link(name = "aaudio")]
    extern "C" {
        fn AAudio_createStreamBuilder(builder: *mut *mut AAudioStreamBuilder) -> i32;
        fn AAudioStreamBuilder_setDirection(builder: *mut AAudioStreamBuilder, dir: i32);
        fn AAudioStreamBuilder_setPerformanceMode(builder: *mut AAudioStreamBuilder, mode: i32);
        fn AAudioStreamBuilder_setSharingMode(builder: *mut AAudioStreamBuilder, mode: i32);
        fn AAudioStreamBuilder_setSampleRate(builder: *mut AAudioStreamBuilder, rate: i32);
        fn AAudioStreamBuilder_setChannelCount(builder: *mut AAudioStreamBuilder, channels: i32);
        fn AAudioStreamBuilder_setFormat(builder: *mut AAudioStreamBuilder, format: i32);
        fn AAudioStreamBuilder_setBufferCapacityInFrames(
            builder: *mut AAudioStreamBuilder,
            num_frames: i32,
        );
        fn AAudioStreamBuilder_setDataCallback(
            builder: *mut AAudioStreamBuilder,
            callback: AAudioDataCallback,
            user_data: *mut c_void,
        );
        fn AAudioStreamBuilder_setErrorCallback(
            builder: *mut AAudioStreamBuilder,
            callback: AAudioErrorCallback,
            user_data: *mut c_void,
        );
        fn AAudioStreamBuilder_openStream(
            builder: *mut AAudioStreamBuilder,
            stream: *mut *mut AAudioStream,
        ) -> i32;
        fn AAudioStreamBuilder_delete(builder: *mut AAudioStreamBuilder) -> i32;
        fn AAudioStream_getFramesPerBurst(stream: *mut AAudioStream) -> i32;
        fn AAudioStream_getSampleRate(stream: *mut AAudioStream) -> i32;
        fn AAudioStream_setBufferSizeInFrames(stream: *mut AAudioStream, num_frames: i32) -> i32;
        fn AAudioStream_getFramesWritten(stream: *mut AAudioStream) -> i64;
        fn AAudioStream_getTimestamp(
            stream: *mut AAudioStream,
            clockid: i32,
            frame_position: *mut i64,
            time_nanoseconds: *mut i64,
        ) -> i32;
        fn AAudioStream_requestStart(stream: *mut AAudioStream) -> i32;
        fn AAudioStream_requestStop(stream: *mut AAudioStream) -> i32;
        fn AAudioStream_close(stream: *mut AAudioStream) -> i32;
    }

    struct CallbackContext {
        fifo: Arc<BurstDecouplerRing>,
        // The AAudio data callback and the RX/error threads share this
        // context. A `&mut CallbackContext` in the callback would alias the
        // `&CallbackContext` used by `needs_reopen()`/the error callback, so
        // the RNG state is atomic and every access uses a shared reference.
        dither_rng: AtomicU32,
        disconnected: AtomicBool,
    }

    extern "C" fn aaudio_output_callback(
        _stream: *mut AAudioStream,
        user_data: *mut c_void,
        audio_data: *mut c_void,
        num_frames: i32,
    ) -> i32 {
        if user_data.is_null() || audio_data.is_null() || num_frames <= 0 {
            return AAUDIO_CALLBACK_RESULT_CONTINUE;
        }
        unsafe {
            let ctx = &*(user_data as *const CallbackContext);
            let total_samples = (num_frames as usize) * 2;
            let out_slice = core::slice::from_raw_parts_mut(audio_data as *mut f32, total_samples);
            let mut rng = ctx.dither_rng.load(Ordering::Relaxed);
            ctx.fifo.pop_exact_or_dither(out_slice, &mut rng);
            ctx.dither_rng.store(rng, Ordering::Relaxed);
        }
        AAUDIO_CALLBACK_RESULT_CONTINUE
    }

    extern "C" fn aaudio_error_callback(
        _stream: *mut AAudioStream,
        user_data: *mut c_void,
        _error: i32,
    ) {
        if !user_data.is_null() {
            unsafe {
                let ctx = &*(user_data as *const CallbackContext);
                ctx.disconnected.store(true, Ordering::Release);
            }
        }
    }

    pub struct AAudioOutputGuard {
        stream: *mut AAudioStream,
        ctx_ptr: *mut CallbackContext,
        actual_sample_rate: u32,
    }

    unsafe impl Send for AAudioOutputGuard {}

    impl AAudioOutputGuard {
        #[inline]
        pub fn needs_reopen(&self) -> bool {
            if self.ctx_ptr.is_null() {
                return true;
            }
            unsafe { (*self.ctx_ptr).disconnected.load(Ordering::Acquire) }
        }

        /// The rate AAudio actually opened, which may be 44.1 kHz on an A2DP
        /// route even though the network stream is 48 kHz. The resampler must
        /// convert to this rate; assuming 48 kHz lets the DAC run dry.
        #[inline]
        pub fn sample_rate(&self) -> u32 {
            self.actual_sample_rate
        }

        /// Measure the downstream hardware/DAC pipeline latency from the
        /// difference between frames written and the frame position that
        /// AAudio reports as already presented to the hardware.
        pub fn measure_hardware_latency_ms(&self) -> Option<u16> {
            if self.stream.is_null() {
                return None;
            }
            unsafe {
                let mut frame_position: i64 = 0;
                let mut time_nanoseconds: i64 = 0;
                let result = AAudioStream_getTimestamp(
                    self.stream,
                    libc::CLOCK_MONOTONIC,
                    &mut frame_position,
                    &mut time_nanoseconds,
                );
                if result != 0 {
                    return None;
                }
                let mut now_ts = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                };
                if libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now_ts) != 0 {
                    return None;
                }
                let now_ns = (now_ts.tv_sec as i64) * 1_000_000_000 + (now_ts.tv_nsec as i64);
                let written = AAudioStream_getFramesWritten(self.stream);
                pending_hardware_delay_ms(
                    frame_position,
                    time_nanoseconds,
                    now_ns,
                    written,
                    self.actual_sample_rate,
                )
            }
        }

        /// Raw low-latency path, kept for callers that do not expose the
        /// Dolby toggle.
        pub fn open_low_latency(fifo: Arc<BurstDecouplerRing>) -> Option<Self> {
            Self::open_with_mode(fifo, false)
        }

        /// Open the output stream in one of two deliberately different modes:
        ///
        /// * `enable_dolby_effects = false`: exclusive MMAP first, game /
        ///   sonification attributes, spatialization never. This is the
        ///   lowest-latency path and bypasses AudioFlinger effects.
        /// * `enable_dolby_effects = true`: shared mixer with media / music
        ///   attributes and automatic spatialization so OEM Dolby Atmos,
        ///   Dirac, or the system equalizer can process the stream. The
        ///   normal mixer (`PERFORMANCE_MODE_NONE`) is tried first because
        ///   low-latency shared streams may still be excluded from effects.
        pub fn open_with_mode(
            fifo: Arc<BurstDecouplerRing>,
            enable_dolby_effects: bool,
        ) -> Option<Self> {
            if enable_dolby_effects {
                Self::open_internal(Arc::clone(&fifo), DOLBY_PROFILE)
                    .or_else(|| Self::open_internal(fifo, DOLBY_LOW_LATENCY_PROFILE))
            } else {
                Self::open_internal(Arc::clone(&fifo), RAW_PROFILE)
                    .or_else(|| Self::open_internal(fifo, RAW_SHARED_PROFILE))
            }
        }

        fn open_internal(fifo: Arc<BurstDecouplerRing>, profile: AAudioProfile) -> Option<Self> {
            unsafe {
                let mut builder: *mut AAudioStreamBuilder = core::ptr::null_mut();
                if AAudio_createStreamBuilder(&mut builder) != 0 || builder.is_null() {
                    return None;
                }

                let ctx = Box::new(CallbackContext {
                    fifo,
                    dither_rng: AtomicU32::new(0x1357_9BDF),
                    disconnected: AtomicBool::new(false),
                });
                let ctx_ptr = Box::into_raw(ctx);

                AAudioStreamBuilder_setDirection(builder, AAUDIO_DIRECTION_OUTPUT);
                AAudioStreamBuilder_setPerformanceMode(builder, profile.performance_mode);
                apply_aaudio_profile(builder, profile);
                AAudioStreamBuilder_setSharingMode(builder, profile.sharing_mode);
                // Do not force 48 kHz. AAudio chooses the route's native rate
                // when left unspecified, which is 44.1 kHz for many A2DP
                // codecs. Requesting 48 kHz there forces AudioFlinger to
                // resample through the normal mixer, where OEM effects live.
                AAudioStreamBuilder_setSampleRate(builder, AAUDIO_UNSPECIFIED);
                AAudioStreamBuilder_setChannelCount(builder, 2);
                AAudioStreamBuilder_setFormat(builder, AAUDIO_FORMAT_PCM_FLOAT);
                AAudioStreamBuilder_setBufferCapacityInFrames(
                    builder,
                    AAUDIO_BUFFER_CAPACITY_FRAMES,
                );
                AAudioStreamBuilder_setDataCallback(
                    builder,
                    aaudio_output_callback,
                    ctx_ptr as *mut c_void,
                );
                AAudioStreamBuilder_setErrorCallback(
                    builder,
                    aaudio_error_callback,
                    ctx_ptr as *mut c_void,
                );

                let mut stream: *mut AAudioStream = core::ptr::null_mut();
                let res = AAudioStreamBuilder_openStream(builder, &mut stream);
                AAudioStreamBuilder_delete(builder);

                if res != 0 || stream.is_null() {
                    let _ = Box::from_raw(ctx_ptr);
                    return None;
                }

                let actual_rate = AAudioStream_getSampleRate(stream);
                let actual_sample_rate = if actual_rate >= 8_000 {
                    actual_rate as u32
                } else {
                    SAMPLE_RATE
                };

                // A2DP bursts are already 10-20 ms. Use one burst there and
                // keep the 3x speaker/USB cushion, otherwise the old code
                // stacked another 30-60 ms of AAudio buffer on Bluetooth.
                if let Some(target_frames) =
                    aaudio_buffer_frames_for_burst(AAudioStream_getFramesPerBurst(stream))
                {
                    AAudioStream_setBufferSizeInFrames(stream, target_frames);
                }

                AAudioStream_requestStart(stream);
                Some(Self {
                    stream,
                    ctx_ptr,
                    actual_sample_rate,
                })
            }
        }
    }

    impl Drop for AAudioOutputGuard {
        fn drop(&mut self) {
            unsafe {
                if !self.stream.is_null() {
                    AAudioStream_requestStop(self.stream);
                    AAudioStream_close(self.stream);
                }
                if !self.ctx_ptr.is_null() {
                    let _ = Box::from_raw(self.ctx_ptr);
                }
            }
        }
    }

    struct InputContext {
        fifo: Arc<BurstDecouplerRing>,
    }

    extern "C" fn aaudio_input_callback(
        _stream: *mut AAudioStream,
        user_data: *mut c_void,
        audio_data: *mut c_void,
        num_frames: i32,
    ) -> i32 {
        if user_data.is_null() || audio_data.is_null() || num_frames <= 0 {
            return AAUDIO_CALLBACK_RESULT_CONTINUE;
        }
        unsafe {
            let ctx = &mut *(user_data as *mut InputContext);
            let frames = num_frames as usize;
            let mono = core::slice::from_raw_parts(audio_data as *const f32, frames);
            // The ring is interleaved stereo; expand the mono capture stream.
            let mut stereo = [0.0f32; 2048];
            let mut offset = 0usize;
            while offset < frames {
                let chunk = (frames - offset).min(stereo.len() / 2);
                for i in 0..chunk {
                    let sample = mono[offset + i];
                    stereo[i * 2] = sample;
                    stereo[i * 2 + 1] = sample;
                }
                ctx.fifo.push_samples(&stereo[..chunk * 2]);
                offset += chunk;
            }
        }
        AAUDIO_CALLBACK_RESULT_CONTINUE
    }

    pub struct AAudioInputGuard {
        stream: *mut AAudioStream,
        ctx_ptr: *mut InputContext,
    }

    unsafe impl Send for AAudioInputGuard {}

    impl AAudioInputGuard {
        /// Open a low-latency mono microphone stream and expand it to the
        /// stereo ring consumed by `PhoneMicTransmitter`.
        pub fn open_low_latency_mic(fifo: Arc<BurstDecouplerRing>) -> Option<Self> {
            Self::open_internal(Arc::clone(&fifo), AAUDIO_SHARING_MODE_EXCLUSIVE)
                .or_else(|| Self::open_internal(fifo, AAUDIO_SHARING_MODE_SHARED))
        }

        fn open_internal(fifo: Arc<BurstDecouplerRing>, sharing_mode: i32) -> Option<Self> {
            unsafe {
                let mut builder: *mut AAudioStreamBuilder = core::ptr::null_mut();
                if AAudio_createStreamBuilder(&mut builder) != 0 || builder.is_null() {
                    return None;
                }

                let ctx = Box::new(InputContext { fifo });
                let ctx_ptr = Box::into_raw(ctx);

                AAudioStreamBuilder_setDirection(builder, AAUDIO_DIRECTION_INPUT);
                AAudioStreamBuilder_setPerformanceMode(
                    builder,
                    AAUDIO_PERFORMANCE_MODE_LOW_LATENCY,
                );
                AAudioStreamBuilder_setSharingMode(builder, sharing_mode);
                AAudioStreamBuilder_setSampleRate(builder, SAMPLE_RATE as i32);
                AAudioStreamBuilder_setChannelCount(builder, 1);
                AAudioStreamBuilder_setFormat(builder, AAUDIO_FORMAT_PCM_FLOAT);
                AAudioStreamBuilder_setDataCallback(
                    builder,
                    aaudio_input_callback,
                    ctx_ptr as *mut c_void,
                );

                let mut stream: *mut AAudioStream = core::ptr::null_mut();
                let res = AAudioStreamBuilder_openStream(builder, &mut stream);
                AAudioStreamBuilder_delete(builder);

                if res != 0 || stream.is_null() {
                    let _ = Box::from_raw(ctx_ptr);
                    return None;
                }

                // Use 3 * hardware burst (minimum 256 frames = ~5.3ms) to prevent HAL DMA XRuns
                let burst = AAudioStream_getFramesPerBurst(stream);
                if burst > 0 {
                    AAudioStream_setBufferSizeInFrames(stream, (burst * 3).max(256));
                }

                AAudioStream_requestStart(stream);
                Some(Self { stream, ctx_ptr })
            }
        }
    }

    impl Drop for AAudioInputGuard {
        fn drop(&mut self) {
            unsafe {
                if !self.stream.is_null() {
                    AAudioStream_requestStop(self.stream);
                    AAudioStream_close(self.stream);
                }
                if !self.ctx_ptr.is_null() {
                    let _ = Box::from_raw(self.ctx_ptr);
                }
            }
        }
    }
}
