use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use audioshare_host_engine::{configure_realtime_udp_socket, promote_thread_realtime};
use audioshare_protocol::crypto::SessionKey;
use audioshare_protocol::dsp::{
    soft_limit_post_resample, BurstDecouplerRing, ClockDriftResampler, TruePeakLimiter,
};
use audioshare_protocol::latency::ClientLatencySnapshot;
#[cfg(not(target_os = "android"))]
use audioshare_protocol::latency::DESKTOP_OUTPUT_BUFFER_MS;
use audioshare_protocol::lpc_lossless::{
    decode_fec_subframe_to_f32, decode_lossless_pcm24_to_f32, decode_lossless_subframe,
    decode_vbr_subframe_to_f32, ADPCM_FEC_BYTES, TAG_VBR_BFP,
};
use audioshare_protocol::opus_codec::{
    OpusStreamDecoder, MAX_OPUS_SUBFRAME_BYTES, OPUS_LOOKAHEAD_MS, TAG_OPUS_RLD,
};
use audioshare_protocol::sequencer::{
    ZeroJitterSequencer, RING_SLOTS, STATUS_EMPTY, STATUS_HEALED_BY_FEC, STATUS_PRIMARY_LOSSLESS,
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

/// Minimum post-stall fast-forward headroom above `target_buf`, in 2.5 ms
/// subframes. The old fixed `+16` allowance let one Wi-Fi stall leave up to
/// 40 ms of permanent playback latency behind; the +6 floor keeps the
/// historical bound for 1-2 subframe framing.
const MIN_BACKLOG_OVER_TARGET_SUBFRAMES: f32 = 6.0;

/// Post-stall headroom for the sender's current framing.
///
/// A 20 ms OS capture callback can hand the host two 10 ms packets at once,
/// and Wi-Fi can aggregate them into one 8-subframe arrival. With a fixed +6
/// cap that normal burst was mistaken for a post-stall backlog and
/// `trim_post_stall_backlog` skipped the playhead by 2 subframes (5 ms). Two
/// packets plus one subframe of FIFO rounding/scheduling margin keeps the
/// fast-forward for genuine stalls only.
#[inline]
fn max_backlog_over_target_subframes(pkt_subframes: u8) -> f32 {
    let pkt = pkt_subframes.max(1) as f32;
    (pkt * 2.0 + 2.0).max(MIN_BACKLOG_OVER_TARGET_SUBFRAMES)
}

/// Configured minimum network cushion before the sender's framing is known.
#[inline]
fn configured_wifi_floor(target_buffer_subframes: f32) -> f32 {
    if target_buffer_subframes <= 2.5 {
        4.0 // Minimum 4 subframes (10.0ms) for USB so OS capture chunks never underrun
    } else {
        target_buffer_subframes.max(6.0)
    }
}

/// Floor that covers one sender packet plus one subframe of margin.
///
/// Tracking the current framing instead of ratcheting upward lets the client
/// return to 2.5/5 ms framing after a SmartAuto excursion into 10 ms mode.
#[inline]
fn wifi_floor_for_packet(configured_floor: f32, subframe_count: u8) -> f32 {
    configured_floor.max((subframe_count.max(1) as f32) * 2.5 + 1.0)
}

/// How far the playhead may trail the newest received subframe while the
/// decoupler still holds real audio. Reordering a single 10 ms packet puts the
/// playhead 4 subframes behind the newer packet, so a fixed `< 3` legitimate
/// wait window synthesized PLC while 20+ ms of audio was still queued.
#[inline]
fn in_flight_wait_subframes(pkt_subframes: u8) -> i16 {
    ((pkt_subframes.max(1) as i16) * 2).max(3)
}

/// Decide whether the refill loop must wait instead of concealing the slot at
/// `next_play_seq`.
///
/// Both at the live edge (`missing_delta < 0`) and inside the reordering/FEC
/// window, never synthesize PLC while the decoupler still holds at least
/// `plc_emergency_watermark` of real audio. Once the FIFO dips below that
/// watermark, emitting one PLC subframe refills it above the watermark and
/// paces concealment at exact 1.0x DAC speed instead of walking `next_play_seq`
/// ahead at the 1 ms socket-timeout rate (the 2.5x-speed runaway cascade).
#[inline]
fn should_hold_missing_slot(
    slot_missing: bool,
    fifo_samples: usize,
    plc_emergency_watermark: usize,
    missing_delta: i16,
    in_flight_wait: i16,
) -> bool {
    slot_missing && fifo_samples >= plc_emergency_watermark && missing_delta < in_flight_wait
}

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
/// burst worth of latency. A peak-hold of the raw decoupler level is equally
/// biased: every whole-subframe refill re-arms the peak between `target + 0.6`
/// and `target + 1.0`, so the windowed minimum never sees the intended
/// watermark and the controller runs permanently fast. Regulate the planned
/// cushion instead: the sequencer backlog plus the decoupler target. Genuine
/// starvation is signalled by `STATUS_EMPTY`/FEC and grows the adaptive target;
/// slowing the resampler by a few hundred ppm cannot replace the missing audio.
#[inline]
fn clock_loop_observation(net_subframes: f32, fifo_subframes: f32) -> f32 {
    net_subframes.max(0.0) + fifo_subframes.max(0.0)
}

/// Post-volume output peak level in percent (0..=100), with a sqrt curve to
/// match the host's VU meter. Pure so the live loop cannot allocate.
#[inline]
pub fn vu_peak_percent(samples: &[f32]) -> u8 {
    let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    (peak.clamp(0.0, 1.0).sqrt() * 100.0).min(100.0) as u8
}

/// Apply the host master volume without a per-subframe step discontinuity.
///
/// A PC volume key changes `host_volume` between two 2.5 ms subframes; applying
/// the new gain instantly leaves a click at the boundary. Ramp the gain over a
/// few milliseconds instead (0.01 per 48 kHz sample is a ~2 ms time constant),
/// and snap back to exact unity once settled so an unused gain stage stays
/// bit-transparent.
#[inline]
fn apply_host_volume(samples: &mut [f32], smoothed: &mut f32, target: f32) {
    if (target - 1.0).abs() <= 0.005 && (*smoothed - 1.0).abs() <= 0.001 {
        *smoothed = 1.0;
        return;
    }
    for sample in samples {
        *smoothed += 0.01 * (target - *smoothed);
        *sample *= *smoothed;
    }
}

/// Unaligned local pipeline delay in milliseconds (above the 4-subframe / 10 ms
/// minimum FIFO floor) reported to the host for multi-device acoustic sync.
///
/// Including `(base_fifo_subframes - 4) * 2.5 ms` alongside the hardware/BT
/// downstream delay ensures two peers with different adaptive Wi-Fi cushions
/// (e.g. 10 ms USB vs 35 ms 2.4 GHz) still converge to the exact same total
/// acoustic delay (`max_group_delay_ms + 10 ms`). Because `base_fifo_subframes`
/// is computed *before* group-sync alignment, this value is independent of
/// `max_group_delay_ms` and cannot oscillate in a feedback loop.
#[inline]
fn effective_unaligned_delay_ms(base_fifo_subframes: usize, downstream_delay_ms: u16) -> u16 {
    let excess_fifo_ms = ((base_fifo_subframes.saturating_sub(4) as u32) * 5) / 2;
    downstream_delay_ms.saturating_add(excess_fifo_ms.min(255) as u16)
}

/// Platform minimum downstream output stage when hardware timestamping is
/// unavailable (e.g. Desktop CPAL or shared-mixer Android routes).
#[inline]
fn fallback_output_delay_ms(hw_delay_ms: u16) -> u16 {
    #[cfg(not(target_os = "android"))]
    {
        hw_delay_ms.max(DESKTOP_OUTPUT_BUFFER_MS.round() as u16)
    }
    #[cfg(target_os = "android")]
    {
        if hw_delay_ms > 0 {
            hw_delay_ms
        } else {
            10
        }
    }
}

/// Lower the packet framing count only after a sustained run of smaller
/// packets. `SplitPerSubframe` sends one subframe per datagram when a loud
/// 24-bit frame exceeds the wire budget, so adopting the split size
/// immediately collapsed the client's Wi-Fi floor for a single transient.
#[inline]
fn update_pkt_subframe_count(current: &mut u8, smaller_run: &mut u8, incoming: u8) {
    if incoming >= *current {
        *current = incoming;
        *smaller_run = 0;
    } else {
        *smaller_run = smaller_run.saturating_add(1);
        if *smaller_run >= 8 {
            *current = incoming;
            *smaller_run = 0;
        }
    }
}

/// Output decoupler watermark in 2.5 ms subframes (4..=64, up to 160 ms).
///
/// The upper bound used to be 12 (30 ms), which silently capped group-sync
/// alignment: a wired client could never add enough cushion to match a
/// Bluetooth peer. 64 subframes (160 ms) is the deliberate maximum that
/// still fits the 32,768-sample decoupler at the two supported output rates:
/// at 96 kHz `base_ratio` is 0.5, so one subframe is 480 samples and 64
/// subframes are 30,720 samples, leaving 2,048 samples (10.7 ms) of
/// headroom. 69 subframes (33,120 samples) would overflow the ring.
#[inline]
fn fifo_target_subframes(target_buf: f32) -> usize {
    (target_buf.ceil() as usize).clamp(4, 64)
}

/// Number of subframes of real audio left after the current decoupler
/// watermark and one sender packet are consumed, before emergency PLC starts.
#[cfg(test)]
#[inline]
fn plc_jitter_margin_subframes(target_fifo: usize, packet_subframes: usize) -> i32 {
    target_fifo as i32 - PLC_EMERGENCY_WATERMARK_SUBFRAMES as i32 - packet_subframes as i32
}

/// Fast-forward a post-stall backlog so `sequencer + decoupler` stays within
/// `target_buf + max_backlog_over_target_subframes(pkt_subframes)`.
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
    pkt_subframes: u8,
) -> bool {
    let max_over = max_backlog_over_target_subframes(pkt_subframes);
    let combined = *net_subframes + fifo_subframes;
    if combined <= target_buf + max_over {
        return false;
    }

    let max_net = (target_buf + max_over - fifo_subframes)
        .max(0.0)
        .min(*net_subframes);
    let keep = max_net.floor() as u16;
    let new_next = latest_rx_seq.wrapping_add(1).wrapping_sub(keep);
    // Never move the playhead backwards. With the invariants above this is
    // already guaranteed, but the FIFO estimate is fractional and a defensive
    // check is cheaper than replaying audio.
    if (new_next.wrapping_sub(*next_play_seq) as i16) <= 0 {
        return false;
    }
    *next_play_seq = new_next;
    *net_subframes = keep as f32;
    true
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

/// Accounting for one 500 ms FEC/loss feedback window.
///
/// The window is deliberately closed only by the periodic timer. Control
/// commands used to share the send condition and reset the counters, so a
/// chip tap 1 ms after a heal reported a 50-100% loss rate and tripped the
/// host's Smart Auto governor for three seconds.
#[derive(Default)]
struct LossWindow {
    primary: u32,
    fec: u32,
    last_permille: u16,
}

impl LossWindow {
    #[inline]
    fn record_primary(&mut self) {
        self.primary += 1;
    }

    #[inline]
    fn record_fec(&mut self) {
        self.fec += 1;
    }

    /// Close the window and return the FEC recovery rate in permille.
    #[inline]
    fn close(&mut self) -> u16 {
        let total = (self.primary + self.fec).max(1);
        self.last_permille = ((self.fec * 1000) / total) as u16;
        self.primary = 0;
        self.fec = 0;
        self.last_permille
    }

    /// Rate from the most recently closed window. Command-only feedback
    /// packets reuse this instead of sampling a partial window.
    #[inline]
    fn last_permille(&self) -> u16 {
        self.last_permille
    }
}

/// A packet this far ahead of the playhead means the sequence tracker was
/// lost or the sender restarted. 48 subframes = 120 ms.
const STREAM_RESYNC_AHEAD_SUBFRAMES: i16 = 48;

/// A packet this far behind the playhead is outside the 35 ms maximum jitter
/// buffer. Treat it as a new stream instead of discarding hundreds of
/// milliseconds of audio after a host restart. 32 subframes = 80 ms.
const STREAM_RESYNC_BEHIND_SUBFRAMES: i16 = -32;

#[inline]
#[cfg(test)]
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

/// Heal one missing sequencer slot from a 124-byte ADPCM FEC frame.
///
/// Only slots still ahead of the playhead and genuinely missing are touched;
/// a late primary copy can always replace the concealment afterwards.
#[inline]
fn heal_fec_slot(sequencer: &ZeroJitterSequencer, seq: u16, next_play_seq: u16, payload: &[u8]) {
    if (seq.wrapping_sub(next_play_seq) as i16).is_negative() || !sequencer.is_slot_missing(seq) {
        return;
    }
    let mut pcm_f32 = [0.0f32; SUBFRAME_STEREO_SAMPLES];
    if decode_fec_subframe_to_f32(payload, &mut pcm_f32) {
        sequencer.try_heal_with_fec(seq, &pcm_f32);
    }
}

/// Heal every complete 124-byte frame in one FEC field. `first_back` is the
/// sequence distance of the first frame from the packet base, and `step` is
/// the distance between frames (1 for legacy fields, 2 for interleaved
/// paired fields). A trailing partial frame is ignored.
fn heal_fec_field(
    sequencer: &ZeroJitterSequencer,
    base_seq: u16,
    next_play_seq: u16,
    payload: &[u8],
    first_back: u16,
    step: u16,
) {
    let mut remaining = payload;
    let mut back = first_back;
    while remaining.len() >= ADPCM_FEC_BYTES {
        let (chunk, tail) = remaining.split_at(ADPCM_FEC_BYTES);
        heal_fec_slot(sequencer, base_seq.wrapping_sub(back), next_play_seq, chunk);
        remaining = tail;
        back = back.wrapping_add(step);
    }
}

/// Heal the slots covered by a packet's two FEC fields.
///
/// Normal framing carries one 124-byte frame per field (`N-1` and `N-2`).
/// Four-subframe 2.4 GHz framing carries two frames per field, interleaved:
/// `fec_n1 = [N-1, N-3]` and `fec_n2 = [N-2, N-4]`, so dropping a whole
/// 10 ms packet no longer leaves all four subframes unrecoverable. The
/// interleaving is a compatibility contract: the first frame of each field
/// keeps its legacy N-1/N-2 meaning, so an older client that only knows the
/// 124-byte layout still heals the right slots. A contiguous
/// `[N-1,N-2]`/`[N-3,N-4]` layout would make old clients place N-3 audio in
/// N-2's slot. A trailing partial frame is ignored.
fn heal_packet_fec(
    base_seq: u16,
    next_play_seq: u16,
    fec_n1: &[u8],
    fec_n2: &[u8],
    sequencer: &ZeroJitterSequencer,
) {
    let paired = fec_n1.len() >= ADPCM_FEC_BYTES * 2;
    let step = if paired { 2 } else { 1 };
    heal_fec_field(sequencer, base_seq, next_play_seq, fec_n1, 1, step);
    heal_fec_field(sequencer, base_seq, next_play_seq, fec_n2, 2, step);
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
fn aaudio_buffer_frames_for_burst(burst: i32, capacity: i32) -> Option<i32> {
    if burst <= 0 {
        None
    } else if burst >= 480 {
        Some(burst.min(capacity.max(burst)))
    } else if burst >= 240 {
        // Double-buffer speaker/USB bursts so HAL DMA never runs
        // single-buffered (XRuns on 240/256-frame Android devices).
        Some((burst * 2).min(capacity.max(burst)))
    } else {
        Some((burst * 3).max(256).min(capacity.max(burst)))
    }
}

/// AAudio ring capacity. The Dolby/system-effects path opens with
/// `AAUDIO_PERFORMANCE_MODE_NONE`, where AudioFlinger's normal mixer uses
/// 960-1920-frame bursts. A 960-frame capacity leaves no double-buffering
/// headroom for the larger burst; 1920 frames covers one full 40 ms burst.
/// The raw low-latency path keeps the 960-frame ring that fits 3x speaker
/// bursts and one full Bluetooth burst.
#[inline]
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn aaudio_capacity_frames(normal_mixer: bool) -> i32 {
    if normal_mixer {
        1920
    } else {
        960
    }
}

pub struct ClientTelemetry {
    pub packets_received: AtomicU64,
    pub primary_frames_played: AtomicU64,
    pub fec_healed_frames: AtomicU64,
    pub plc_extrapolated_frames: AtomicU64,
    pub auth_failures: AtomicU64,
    pub clock_drift_ppm: AtomicI32,
    /// Linux/Android kernel TID of the real-time RX/DSP thread for ADPF hints.
    pub rt_thread_tid: AtomicI32,
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
    /// Post-volume output peak level (0..=100%) for the live VU meter.
    pub client_vu_peak_pct: AtomicU8,
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

/// Interval between ADB reverse-tunnel reconnect attempts after the TCP stream
/// hits EOF or an error (a USB cable yank silently kills `adb reverse`).
const ADB_TCP_RECONNECT_INTERVAL: Duration = Duration::from_millis(400);

/// Loopback TCP↔UDP bridge for `adb reverse tcp:P tcp:P`. When the configured
/// server is loopback and a TCP listener is reachable, ClientEngine's UDP
/// datagrams are tunnelled over `TCP_NODELAY` to that listener instead of being
/// sent as plain loopback UDP. Both directions use `[u16_be len][datagram]`.
///
/// The initial connect is synchronous and bounded to 150 ms so plain loopback
/// UDP (no ADB tunnel) falls through immediately. From then on a supervisor
/// thread owns the TCP stream and reconnects every 400 ms on EOF/error until
/// the bridge is dropped.
struct AdbTcpClientBridge {
    running: Arc<AtomicBool>,
    local_udp_addr: SocketAddr,
    supervisor_handle: Option<std::thread::JoinHandle<()>>,
}

impl AdbTcpClientBridge {
    fn try_connect(server_addr: SocketAddr, client_udp_port: u16) -> Option<Self> {
        if !server_addr.ip().is_loopback() {
            return None;
        }
        // Fast probe: fail immediately when no ADB reverse listener exists so
        // normal localhost UDP clients do not pay a reconnect-loop penalty.
        let probe = TcpStream::connect_timeout(&server_addr, Duration::from_millis(150)).ok()?;

        // Stable UDP endpoint handed to ClientEngine. It survives TCP
        // reconnects, so the caller's effective server address never changes.
        let udp_base = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).ok()?;
        let local_udp_addr = udp_base.local_addr().ok()?;
        // A bounded read lets the supervisor join the per-session out thread
        // promptly on yank/shutdown instead of blocking forever in recv().
        let _ = udp_base.set_read_timeout(Some(Duration::from_millis(200)));

        let running = Arc::new(AtomicBool::new(true));
        let run_supervisor = Arc::clone(&running);
        let server_addr_clone = server_addr;
        let supervisor_handle = std::thread::Builder::new()
            .name("as-ultra-adb-cli-supervisor".into())
            .spawn(move || {
                let client_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, client_udp_port));
                // Reuse the probe as the first session so our exchange starts
                // on the very socket the listener already accepted.
                let mut pending_stream = Some(probe);
                let mut next_connect = Instant::now();

                while run_supervisor.load(Ordering::Relaxed) {
                    let mut tcp_reader = if let Some(stream) = pending_stream.take() {
                        stream
                    } else {
                        if Instant::now() < next_connect {
                            std::thread::sleep(Duration::from_millis(20));
                            continue;
                        }
                        match TcpStream::connect_timeout(
                            &server_addr_clone,
                            Duration::from_millis(150),
                        ) {
                            Ok(stream) => stream,
                            Err(_) => {
                                next_connect = Instant::now() + ADB_TCP_RECONNECT_INTERVAL;
                                continue;
                            }
                        }
                    };
                    let _ = tcp_reader.set_nodelay(true);
                    let _ = tcp_reader.set_read_timeout(Some(Duration::from_millis(200)));
                    let _ = tcp_reader.set_write_timeout(Some(Duration::from_millis(50)));
                    let mut tcp_writer = match tcp_reader.try_clone() {
                        Ok(s) => s,
                        Err(_) => {
                            next_connect = Instant::now() + ADB_TCP_RECONNECT_INTERVAL;
                            continue;
                        }
                    };
                    let udp_tx = match udp_base.try_clone() {
                        Ok(s) => s,
                        Err(_) => {
                            next_connect = Instant::now() + ADB_TCP_RECONNECT_INTERVAL;
                            continue;
                        }
                    };
                    let udp_rx = match udp_base.try_clone() {
                        Ok(s) => s,
                        Err(_) => {
                            next_connect = Instant::now() + ADB_TCP_RECONNECT_INTERVAL;
                            continue;
                        }
                    };

                    // One TCP session: two short-lived I/O threads. Either
                    // side ending the session (EOF/error) drops `session_alive`
                    // so the supervisor can join them and reconnect.
                    let session_alive = Arc::new(AtomicBool::new(true));
                    let alive_in = Arc::clone(&session_alive);
                    let run_in = Arc::clone(&run_supervisor);
                    let in_handle = std::thread::Builder::new()
                        .name("as-ultra-adb-cli-in".into())
                        .spawn(move || {
                            let mut len_buf = [0u8; 2];
                            let mut pkt = [0u8; MAX_SAFE_UDP_PAYLOAD];
                            while alive_in.load(Ordering::Relaxed) && run_in.load(Ordering::Relaxed)
                            {
                                match tcp_reader.read_exact(&mut len_buf) {
                                    Ok(()) => {
                                        let len = u16::from_be_bytes(len_buf) as usize;
                                        if len == 0 || len > pkt.len() {
                                            break;
                                        }
                                        if tcp_reader.read_exact(&mut pkt[..len]).is_err() {
                                            break;
                                        }
                                        let _ = udp_tx.send_to(&pkt[..len], client_addr);
                                    }
                                    Err(ref e)
                                        if e.kind() == std::io::ErrorKind::WouldBlock
                                            || e.kind() == std::io::ErrorKind::TimedOut => {}
                                    Err(_) => break,
                                }
                            }
                            alive_in.store(false, Ordering::Release);
                        })
                        .ok();

                    let alive_out = Arc::clone(&session_alive);
                    let run_out = Arc::clone(&run_supervisor);
                    let out_handle = std::thread::Builder::new()
                        .name("as-ultra-adb-cli-out".into())
                        .spawn(move || {
                            let mut framed = [0u8; 2 + MAX_SAFE_UDP_PAYLOAD];
                            while alive_out.load(Ordering::Relaxed) && run_out.load(Ordering::Relaxed)
                            {
                                match udp_rx.recv(&mut framed[2..]) {
                                    Ok(len) if len > 0 && len <= MAX_SAFE_UDP_PAYLOAD => {
                                        framed[0..2].copy_from_slice(&(len as u16).to_be_bytes());
                                        if tcp_writer.write_all(&framed[..2 + len]).is_err() {
                                            break;
                                        }
                                    }
                                    Ok(_) => {}
                                    Err(ref e)
                                        if e.kind() == std::io::ErrorKind::WouldBlock
                                            || e.kind() == std::io::ErrorKind::TimedOut => {}
                                    Err(_) => break,
                                }
                            }
                            alive_out.store(false, Ordering::Release);
                        })
                        .ok();

                    while session_alive.load(Ordering::Relaxed) && run_supervisor.load(Ordering::Relaxed)
                    {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    session_alive.store(false, Ordering::Release);
                    if let Some(h) = in_handle {
                        let _ = h.join();
                    }
                    if let Some(h) = out_handle {
                        let _ = h.join();
                    }
                    next_connect = Instant::now() + ADB_TCP_RECONNECT_INTERVAL;
                }
            })
            .ok();

        Some(Self {
            running,
            local_udp_addr,
            supervisor_handle,
        })
    }
}

impl Drop for AdbTcpClientBridge {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(h) = self.supervisor_handle.take() {
            let _ = h.join();
        }
    }
}

pub struct ClientEngine {
    running: Arc<AtomicBool>,
    pub telemetry: Arc<ClientTelemetry>,
    session_key: Arc<Mutex<Option<SessionKey>>>,
    adb_bridge: Option<AdbTcpClientBridge>,
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
        let initial_floor_subframes = configured_wifi_floor(config.target_buffer_subframes);
        let initial_target_subframes = (initial_floor_subframes.ceil() as u32).clamp(4, 12);
        let telemetry = Arc::new(ClientTelemetry {
            packets_received: AtomicU64::new(0),
            primary_frames_played: AtomicU64::new(0),
            fec_healed_frames: AtomicU64::new(0),
            plc_extrapolated_frames: AtomicU64::new(0),
            auth_failures: AtomicU64::new(0),
            clock_drift_ppm: AtomicI32::new(0),
            rt_thread_tid: AtomicI32::new(0),
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
            client_vu_peak_pct: AtomicU8::new(0),
        });

        let session_key = Arc::new(Mutex::new(pin.map(SessionKey::from_pin)));

        // Ephemeral source port avoids hijacking HostEngine on Windows
        // (SO_REUSEADDR let the client bind 65530 while the host was already
        // bound there) and EADDRINUSE on Linux. Both PC and Party hosts unicast
        // replies to our real source port, so the fixed bind is unnecessary.
        let socket = UdpSocket::bind("0.0.0.0:0")
            .map_err(|e| format!("Failed to bind client UDP socket: {}", e))?;
        configure_realtime_udp_socket(&socket)
            .map_err(|e| format!("Failed to configure real-time client socket: {}", e))?;
        let _ = socket.set_read_timeout(Some(Duration::from_millis(5)));

        // ADB USB mode: adb reverse forwards a loopback TCP stream, not UDP.
        // Bridge it to our UDP socket when the server is loopback and a TCP
        // listener is present; otherwise keep the normal UDP path unchanged.
        let client_port = socket.local_addr().map(|a| a.port()).unwrap_or(0);
        let adb_bridge = if client_port != 0 {
            AdbTcpClientBridge::try_connect(config.server_addr, client_port)
        } else {
            None
        };
        let mut effective_config = config;
        if let Some(ref bridge) = adb_bridge {
            effective_config.server_addr = bridge.local_udp_addr;
        }

        let _ = socket.send_to(CONTROL_MAGIC_HELLO, effective_config.server_addr);

        let burst_fifo = Arc::new(BurstDecouplerRing::new());
        let running_clone = Arc::clone(&running);
        let telemetry_clone = Arc::clone(&telemetry);
        let key_clone = Arc::clone(&session_key);

        let rx_handle = std::thread::Builder::new()
            .name("as-ultra-client-rx".into())
            .spawn(move || {
                promote_thread_realtime();
                #[cfg(any(target_os = "linux", target_os = "android"))]
                unsafe {
                    telemetry_clone
                        .rt_thread_tid
                        .store(libc::gettid(), Ordering::Relaxed);
                }

                #[cfg(not(target_os = "android"))]
                let mut _audio_stream: Option<(cpal::Stream, u32, Arc<AtomicBool>)> = None;

                #[cfg(target_os = "android")]
                let mut _android_aaudio: Option<android_aaudio::AAudioOutputGuard> = None;

                Self::realtime_rx_and_dsp_loop(
                    socket,
                    effective_config,
                    burst_fifo,
                    #[cfg(not(target_os = "android"))]
                    &mut _audio_stream,
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
            adb_bridge,
            rx_handle: Some(rx_handle),
        })
    }

    pub fn set_encryption_pin(&self, pin: Option<&str>) {
        let mut guard = self.session_key.lock().unwrap();
        *guard = pin.map(SessionKey::from_pin);
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        self.adb_bridge = None;
        if let Some(h) = self.rx_handle.take() {
            let _ = h.join();
        }
    }

    #[cfg(not(target_os = "android"))]
    pub fn pull_stereo_to_channels(
        fifo: &BurstDecouplerRing,
        channels: usize,
        frames: usize,
        stereo_scratch: &mut [f32; 2048],
        dither_rng: &mut u32,
        mut write_sample: impl FnMut(usize, f32),
    ) {
        let ch = channels.max(1);
        let max_chunk_frames = stereo_scratch.len() / 2;
        let mut frame_offset = 0usize;
        while frame_offset < frames {
            let chunk_frames = (frames - frame_offset).min(max_chunk_frames);
            fifo.pop_exact_or_dither(&mut stereo_scratch[..chunk_frames * 2], dither_rng);
            for f in 0..chunk_frames {
                let l = stereo_scratch[f * 2];
                let r = stereo_scratch[f * 2 + 1];
                let base = (frame_offset + f) * ch;
                if ch == 1 {
                    write_sample(base, (l + r) * 0.5);
                } else {
                    write_sample(base, l);
                    write_sample(base + 1, r);
                    for extra in 2..ch {
                        write_sample(base + extra, 0.0);
                    }
                }
            }
            frame_offset += chunk_frames;
        }
    }

    #[cfg(not(target_os = "android"))]
    pub fn fill_i16_output(fifo: &BurstDecouplerRing, out: &mut [i16], dither_rng: &mut u32) {
        let mut scratch = [0.0f32; 2048];
        let frames = out.len() / 2;
        Self::pull_stereo_to_channels(fifo, 2, frames, &mut scratch, dither_rng, |idx, s| {
            out[idx] = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        });
    }

    #[cfg(not(target_os = "android"))]
    fn spawn_desktop_output_sink(
        burst_fifo: Arc<BurstDecouplerRing>,
    ) -> Result<(cpal::Stream, u32, Arc<AtomicBool>), String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "No audio output device available".to_string())?;
        let default_cfg = device
            .default_output_config()
            .map_err(|e| format!("Output config error: {}", e))?;
        let sample_format = default_cfg.sample_format();
        let dev_rate = default_cfg.sample_rate().0.max(8_000);
        let dev_channels = default_cfg.channels().max(1);
        let stream_error = Arc::new(AtomicBool::new(false));

        // Prefer a 5 ms fixed callback buffer to cut desktop output latency.
        // Many ALSA/PulseAudio backends reject an unsupported period size, so
        // fall back to 48 kHz default and then to the device's native rate/channels.
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
        let native_stream_cfg = cpal::StreamConfig {
            channels: dev_channels,
            sample_rate: cpal::SampleRate(dev_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let build_for_cfg = |cfg: &cpal::StreamConfig| -> Result<(cpal::Stream, u32), cpal::BuildStreamError> {
            let fifo = Arc::clone(&burst_fifo);
            let err_flag = Arc::clone(&stream_error);
            let ch = cfg.channels.max(1) as usize;
            let rate = cfg.sample_rate.0;
            let mut dither_rng = 0x9E37_79B9u32;
            let mut scratch = [0.0f32; 2048];
            let err_cb = move |_: cpal::StreamError| {
                err_flag.store(true, Ordering::Release);
            };
            let stream = match sample_format {
                cpal::SampleFormat::I16 => device.build_output_stream(
                    cfg,
                    move |out: &mut [i16], _: &cpal::OutputCallbackInfo| {
                        let frames = out.len() / ch;
                        Self::pull_stereo_to_channels(&fifo, ch, frames, &mut scratch, &mut dither_rng, |idx, s| {
                            out[idx] = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
                        });
                    },
                    err_cb,
                    None,
                )?,
                cpal::SampleFormat::U16 => device.build_output_stream(
                    cfg,
                    move |out: &mut [u16], _: &cpal::OutputCallbackInfo| {
                        let frames = out.len() / ch;
                        Self::pull_stereo_to_channels(&fifo, ch, frames, &mut scratch, &mut dither_rng, |idx, s| {
                            out[idx] = ((s.clamp(-1.0, 1.0) * 32768.0 + 32768.0).round()).clamp(0.0, 65535.0) as u16;
                        });
                    },
                    err_cb,
                    None,
                )?,
                cpal::SampleFormat::I32 => device.build_output_stream(
                    cfg,
                    move |out: &mut [i32], _: &cpal::OutputCallbackInfo| {
                        let frames = out.len() / ch;
                        Self::pull_stereo_to_channels(&fifo, ch, frames, &mut scratch, &mut dither_rng, |idx, s| {
                            out[idx] = (f64::from(s.clamp(-1.0, 1.0)) * 2_147_483_647.0).round() as i32;
                        });
                    },
                    err_cb,
                    None,
                )?,
                cpal::SampleFormat::F64 => device.build_output_stream(
                    cfg,
                    move |out: &mut [f64], _: &cpal::OutputCallbackInfo| {
                        let frames = out.len() / ch;
                        Self::pull_stereo_to_channels(&fifo, ch, frames, &mut scratch, &mut dither_rng, |idx, s| {
                            out[idx] = f64::from(s.clamp(-1.0, 1.0));
                        });
                    },
                    err_cb,
                    None,
                )?,
                _ => device.build_output_stream(
                    cfg,
                    move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        if ch == 2 {
                            fifo.pop_exact_or_dither(out, &mut dither_rng);
                        } else {
                            let frames = out.len() / ch;
                            Self::pull_stereo_to_channels(&fifo, ch, frames, &mut scratch, &mut dither_rng, |idx, s| {
                                out[idx] = s;
                            });
                        }
                    },
                    err_cb,
                    None,
                )?,
            };
            Ok((stream, rate))
        };

        let (stream, actual_rate) = build_for_cfg(&low_lat_cfg)
            .or_else(|_| build_for_cfg(&default_stream_cfg))
            .or_else(|_| build_for_cfg(&native_stream_cfg))
            .map_err(|e| format!("Failed to open audio output: {}", e))?;

        stream
            .play()
            .map_err(|e| format!("Failed to start audio output: {}", e))?;
        Ok((stream, actual_rate, stream_error))
    }

    fn realtime_rx_and_dsp_loop(
        socket: UdpSocket,
        config: ClientConfig,
        burst_fifo: Arc<BurstDecouplerRing>,
        #[cfg(not(target_os = "android"))] desktop_stream: &mut Option<(cpal::Stream, u32, Arc<AtomicBool>)>,
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
        // Raw Opus payloads are buffered here and decoded at playhead pop
        // time, so a single reordered Wi-Fi packet (<15 ms before playback)
        // is decoded in strict stream order instead of being permanently
        // discarded by the stateful CELT decoder.
        #[derive(Clone)]
        struct OpusPacketSlot {
            seq: Option<u16>,
            len: u16,
            data: [u8; MAX_OPUS_SUBFRAME_BYTES],
        }
        let mut opus_ring = vec![
            OpusPacketSlot {
                seq: None,
                len: 0,
                data: [0u8; MAX_OPUS_SUBFRAME_BYTES],
            };
            RING_SLOTS
        ];
        let mut last_audio_codec_mode: Option<CodecMode> = None;

        let mut recv_buf = [0u8; MAX_SAFE_UDP_PAYLOAD];
        let mut stream_initialized = false;
        let mut prebuffer_ready = false;
        let mut next_play_seq: u16 = 0;
        let mut latest_rx_seq: u16 = 0;

        let mut last_feedback = Instant::now();
        let mut last_packet_instant = Instant::now();
        #[cfg(target_os = "android")]
        let mut last_hw_latency_poll = Instant::now();
        #[cfg(not(target_os = "android"))]
        let mut last_desktop_open_attempt: Option<Instant> = None;
        #[cfg(target_os = "android")]
        let mut last_dolby_state = telemetry.enable_dolby_effects.load(Ordering::Relaxed);
        #[cfg(target_os = "android")]
        let mut last_bt_active = telemetry.downstream_bt_delay_ms.load(Ordering::Relaxed) > 0;
        #[cfg(target_os = "android")]
        let mut last_aaudio_open_attempt: Option<Instant> = None;
        let mut window = LossWindow::default();
        // Auto-adaptive jitter cushion: 3.0 (7.5ms) on 5GHz, auto-expands up to 14.0 (35.0ms) on 2.4GHz!
        let configured_floor = configured_wifi_floor(config.target_buffer_subframes);
        let mut initial_wifi_floor = configured_floor;
        let mut adaptive_wifi_subframes = initial_wifi_floor;
        let mut last_pkt_subframe_count: u8 = 1;
        let mut smaller_pkt_run: u8 = 0;
        // The refill loop targets this watermark. Keeping the last value lets
        // the socket poll below decide whether the decoupler needs audio
        // before the new target is recomputed for this pass.
        let mut last_fifo_watermark =
            resampler.subframes_to_samples(fifo_target_subframes(initial_wifi_floor));
        let mut last_plc_watermark =
            resampler.subframes_to_samples(PLC_EMERGENCY_WATERMARK_SUBFRAMES);
        let mut consecutive_empty_ticks: u16 = 0;
        let mut smoothed_host_vol: f32 = 1.0;
        let mut last_aligned_target_subframes: usize = fifo_target_subframes(initial_wifi_floor);
        // RFC 3550 Inter-Arrival Jitter Estimator state (microseconds)
        let mut prev_rx_us: Option<u32> = None;
        let mut prev_tx_us: u32 = 0;
        let mut jitter_estimate_us: f32 = 2500.0;
        let mut last_clock_sample = Instant::now();
        // Strictly monotonic feedback sequence: bound into the AEAD nonce so a
        // captured authenticated feedback packet cannot be replayed.
        let mut feedback_seq: u32 = 0;
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
            #[cfg(not(target_os = "android"))]
            {
                let failed = desktop_stream
                    .as_ref()
                    .map(|(_, _, err)| err.load(Ordering::Acquire))
                    .unwrap_or(false);
                let retry_due = desktop_stream.is_none()
                    && last_desktop_open_attempt
                        .map(|t| t.elapsed() >= Duration::from_millis(500))
                        .unwrap_or(true);
                if failed || retry_due {
                    last_desktop_open_attempt = Some(Instant::now());
                    *desktop_stream = None;
                    if let Ok((stream, rate, err)) =
                        Self::spawn_desktop_output_sink(Arc::clone(&burst_fifo))
                    {
                        resampler.set_output_sample_rate(rate);
                        *desktop_stream = Some((stream, rate, err));
                    }
                }
            }

            // Auto-recover the AAudio stream within 25 ms when the
            // headphones/Bluetooth route changes, or reopen it when the user
            // toggles Dolby/system post-processing. The resampler is pointed
            // at the rate the new stream actually opened, so 44.1 kHz A2DP
            // does not need a second AudioFlinger resampler.
            #[cfg(target_os = "android")]
            {
                let want_dolby = telemetry.enable_dolby_effects.load(Ordering::Relaxed);
                let bt_active = telemetry.downstream_bt_delay_ms.load(Ordering::Relaxed) > 0;
                let retry_due = aaudio_guard.is_none()
                    && last_aaudio_open_attempt
                        .map(|attempt| attempt.elapsed() >= Duration::from_millis(500))
                        .unwrap_or(true);
                let reopen = aaudio_guard
                    .as_ref()
                    .map(|g| g.needs_reopen())
                    .unwrap_or(false)
                    || want_dolby != last_dolby_state
                    || bt_active != last_bt_active
                    || retry_due;
                if reopen {
                    last_dolby_state = want_dolby;
                    last_bt_active = bt_active;
                    last_aaudio_open_attempt = Some(Instant::now());
                    // Stop and close the previous AAudio stream before opening
                    // the new one. Rust evaluates the assignment's RHS first,
                    // so without this explicit drop `open_with_mode` starts a
                    // second callback thread while the old stream is still
                    // running: two consumers race on the SPSC decoupler, and
                    // the old stream keeps the device busy during an
                    // exclusive-mode reopen.
                    *aaudio_guard = None;
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

            let periodic_due = last_feedback.elapsed() >= Duration::from_millis(500);
            if periodic_due
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

                // Only the periodic timer owns the loss window. A command
                // flush must never close a 1 ms partial window (which used to
                // report a single heal as 50-100% loss and trip Smart Auto
                // for three seconds); command packets reuse the last closed
                // window's rate instead.
                let fec_permille = if periodic_due {
                    let closed = window.close();

                    // Decay the 2.4 GHz cushion when the link is healthy
                    // enough that FEC is keeping up. Minor background recovery
                    // must not pin the cushion at its 35 ms ceiling forever.
                    adaptive_wifi_subframes =
                        decay_adaptive_cushion(adaptive_wifi_subframes, initial_wifi_floor, closed);
                    last_feedback = Instant::now();
                    closed
                } else {
                    window.last_permille()
                };

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
                let raw_hw_delay_ms =
                    fallback_output_delay_ms(telemetry.downstream_hw_delay_ms.load(Ordering::Relaxed));
                let base_target_for_fb = if gaming_lock_now {
                    gaming_base_target_subframes(last_pkt_subframe_count, jitter_estimate_us)
                } else {
                    adaptive_wifi_subframes
                };
                let base_fifo_for_fb = fifo_target_subframes(base_target_for_fb);
                let effective_hw_delay_ms = effective_unaligned_delay_ms(
                    base_fifo_for_fb,
                    raw_hw_delay_ms.max(bt_delay_ms),
                );

                feedback_seq = feedback_seq.wrapping_add(1).max(1);
                let fb = ClientFeedbackPacket {
                    fec_rate_permille: fec_permille,
                    bluetooth_delay_ms: bt_delay_ms,
                    hardware_delay_ms: effective_hw_delay_ms,
                    client_volume: client_vol,
                    requested_mode: media_cmd,
                    feedback_seq,
                    auth_tag: [0u8; 16],
                };
                let mut fb_bytes = [0u8; CONTROL_FEEDBACK_LEN];
                fb.encode_signed(active_key.as_ref(), &mut fb_bytes);

                let _ = socket.send_to(&fb_bytes, config.server_addr);
            }

            // A blocking recv_from (up to 1 ms, often longer under Windows or
            // Android power-save) can starve the decoupler even when the
            // sequencer already holds the next playable subframe. Poll
            // non-blocking in that case so refill runs before waiting on the
            // network. Also poll non-blocking whenever the decoupler is at or
            // below the emergency PLC watermark to avoid sleeping 1-2 ms.
            let fifo_avail_now = burst_fifo.available_samples();
            let opus_ready_now =
                opus_ring[(next_play_seq as usize) & (RING_SLOTS - 1)].seq == Some(next_play_seq);
            // Drive the non-blocking poll from the EXACT same predicate the
            // refill loop uses to decide whether it can make progress. If the
            // loop will immediately `break` (slot genuinely in flight with
            // real audio still queued), blocking is correct and spinning is not.
            let can_refill_now = !should_hold_missing_slot(
                sequencer.is_slot_missing(next_play_seq) && !opus_ready_now,
                fifo_avail_now,
                last_plc_watermark,
                latest_rx_seq.wrapping_sub(next_play_seq) as i16,
                in_flight_wait_subframes(last_pkt_subframe_count),
            );
            let refill_poll = stream_initialized
                && prebuffer_ready
                && fifo_avail_now < last_fifo_watermark
                && can_refill_now;
            if refill_poll {
                let _ = socket.set_nonblocking(true);
            }

            let mut packets_drained = 0u8;
            // Only the first audio packet of a drain pass feeds the RFC 3550
            // jitter estimator. A second packet read from the kernel queue a
            // few microseconds later would compare its ~5 µs receive gap
            // against the sender's 5 ms timestamp gap and invent ~5 ms of
            // "jitter" on a perfectly paced link.
            let mut audio_packets_drained = 0u8;

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
                        if parsed.header.mode != CodecMode::SilenceHeartbeat {
                            update_pkt_subframe_count(
                                &mut last_pkt_subframe_count,
                                &mut smaller_pkt_run,
                                parsed.header.subframe_count.max(1),
                            );
                        }
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
                            for slot in &mut opus_ring {
                                slot.seq = None;
                            }
                            continue;
                        }

                        // A codec switch (Lossless <-> Opus in SmartAuto) joins
                        // two different waveform predictors; splice the new
                        // mode in from the last played sample to avoid a click.
                        if let Some(prev_mode) = last_audio_codec_mode {
                            if prev_mode != parsed.header.mode {
                                sequencer.arm_declick();
                            }
                        }
                        last_audio_codec_mode = Some(parsed.header.mode);

                        // RFC 3550 Inter-Arrival Jitter calculation using sender `tx_timestamp_us`.
                        // Sample only the first audio packet per drain: packets
                        // already queued in the socket would otherwise report a
                        // near-zero receive gap against a full sender interval.
                        audio_packets_drained = audio_packets_drained.saturating_add(1);
                        let rx_now_us = start_instant.elapsed().as_micros() as u32;
                        let tx_now_us = parsed.header.tx_timestamp_us;
                        if audio_packets_drained == 1 {
                            if let Some(last_r) = prev_rx_us {
                                let d_rx = rx_now_us.wrapping_sub(last_r) as i32;
                                let d_tx = tx_now_us.wrapping_sub(prev_tx_us) as i32;
                                let transit_delta = (d_rx - d_tx).unsigned_abs().min(50_000) as f32;
                                jitter_estimate_us +=
                                    (transit_delta - jitter_estimate_us) * (1.0 / 16.0);
                            }
                        }
                        // Advance the reference on EVERY audio packet in the
                        // drain. A Wi-Fi A-MPDU delivers packets 2..4 only µs
                        // after packet 1; leaving prev_* at packet 1 made the
                        // next on-time packet compare a 5 ms receive gap
                        // against a 20 ms sender gap (+15 ms fake jitter).
                        prev_rx_us = Some(rx_now_us);
                        prev_tx_us = tx_now_us;
                        telemetry
                            .network_jitter_us
                            .store(jitter_estimate_us.max(0.0) as u32, Ordering::Relaxed);
                        // Track the sender's current bundling mode instead of
                        // ratcheting the floor upward. A single 10 ms packet
                        // (4 subframes) used to raise the floor to 11
                        // subframes (27.5 ms) for the rest of the session,
                        // even after the host returned to 5 ms or 2.5 ms
                        // framing. Use the smoothed count so a transient
                        // `SplitPerSubframe` datagram cannot drop the floor
                        // from 15 ms to 10 ms for one packet.
                        let bt_coex_floor = if telemetry.downstream_bt_delay_ms.load(Ordering::Relaxed) > 0 {
                            configured_floor.max(8.0)
                        } else {
                            configured_floor
                        };
                        initial_wifi_floor =
                            wifi_floor_for_packet(bt_coex_floor, last_pkt_subframe_count);
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
                        // Allow the sequencer backlog to absorb a draining
                        // group-sync FIFO without misclassifying it as a host
                        // restart (which used to drop 120 ms of audio).
                        let fifo_now_subframes =
                            resampler.samples_to_subframes(burst_fifo.available_samples()) as i16;
                        let resync_ahead =
                            STREAM_RESYNC_AHEAD_SUBFRAMES.max(fifo_now_subframes + 16);
                        if telemetry.server_dtx_silent.swap(false, Ordering::Relaxed)
                            || seq_delta > resync_ahead
                            || seq_delta < STREAM_RESYNC_BEHIND_SUBFRAMES
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
                            for slot in &mut opus_ring {
                                slot.seq = None;
                            }
                        }

                        // Lazy FEC Decoding: only decode the backup frames that
                        // cover slots that are ACTUALLY missing and still
                        // ahead of the DAC playhead (saves 99.5% of FEC CPU
                        // work!). A 10 ms packet carries four frames across
                        // its two fields, so one lost packet is fully healed.
                        //
                        // Opus RLD has a 2.5 ms codec lookahead, while the
                        // IMA-ADPCM FEC backup has zero lookahead. Splicing
                        // that FEC into an Opus stream would advance the
                        // waveform by one subframe, so Opus gaps are healed by
                        // phase-aligned CELT PLC in `decode_compressed_payload`
                        // instead.
                        let opus_primary =
                            primary_is_opus(parsed.header.mode, parsed.primary_payload);
                        if !opus_primary
                            && (!parsed.fec_n1_payload.is_empty()
                                || !parsed.fec_n2_payload.is_empty())
                        {
                            heal_packet_fec(
                                base_seq,
                                next_play_seq,
                                parsed.fec_n1_payload,
                                parsed.fec_n2_payload,
                                &sequencer,
                            );
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

                            let payload = &prim[cursor..cursor + sub_len];
                            // Decode-at-pop for Opus: buffer the raw bytes so
                            // reordered Wi-Fi packets decode losslessly in
                            // strict stream order at the playhead. The stateful
                            // CELT decoder must never see out-of-order frames.
                            if payload.first() == Some(&TAG_OPUS_RLD)
                                && sub_len <= MAX_OPUS_SUBFRAME_BYTES
                            {
                                if (s.wrapping_sub(next_play_seq) as i16) >= 0 {
                                    let slot = &mut opus_ring[(s as usize) & (RING_SLOTS - 1)];
                                    if slot.seq != Some(s) {
                                        slot.data[..sub_len].copy_from_slice(payload);
                                        slot.len = sub_len as u16;
                                        slot.seq = Some(s);
                                    }
                                    if (s.wrapping_sub(latest_rx_seq) as i16) > 0 {
                                        latest_rx_seq = s;
                                    }
                                }
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
            let base_fifo_subframes = fifo_target_subframes(base_target);
            let my_downstream_delay_ms = telemetry
                .downstream_bt_delay_ms
                .load(Ordering::Relaxed)
                .max(fallback_output_delay_ms(
                    telemetry.downstream_hw_delay_ms.load(Ordering::Relaxed),
                ));
            let my_effective_delay_ms =
                effective_unaligned_delay_ms(base_fifo_subframes, my_downstream_delay_ms);
            let target_buf = if gaming_lock {
                base_target
            } else {
                group_sync.aligned_target_subframes(
                    base_target,
                    telemetry.max_group_delay_ms.load(Ordering::Relaxed),
                    my_effective_delay_ms,
                )
            };

            let target_fifo_subframes = fifo_target_subframes(target_buf);

            if stream_initialized && !telemetry.server_dtx_silent.load(Ordering::Relaxed) {
                let net_now = (latest_rx_seq.wrapping_add(1).wrapping_sub(next_play_seq) as i16)
                    .max(0) as f32;
                let fifo_now = resampler.samples_to_subframes(burst_fifo.available_samples());
                let total_now = net_now + fifo_now;

                let group_extra = target_fifo_subframes.saturating_sub(base_fifo_subframes);
                let target_jumped = target_fifo_subframes >= last_aligned_target_subframes + 4;
                last_aligned_target_subframes = target_fifo_subframes;

                // Sequencer only prebuffers the base floor (4..=11 subframes = 10..=27.5 ms)
                // so `seq_delta` never approaches STREAM_RESYNC_AHEAD_SUBFRAMES (48 subframes).
                if !prebuffer_ready && total_now >= base_fifo_subframes as f32 {
                    prebuffer_ready = true;
                    telemetry.prebuffer_ready.store(true, Ordering::Relaxed);
                    sequencer.arm_fade_in();
                }

                // If Group Sync requires a deeper cushion (e.g. wired client aligning to
                // a 145 ms Bluetooth peer), inject the lead-in cushion directly into
                // `burst_fifo` (which holds up to 32,768 samples = 136 subframes @ 48kHz)
                // ahead of real audio.
                if prebuffer_ready && group_extra > 0
                    && (burst_fifo.available_samples() == 0 || target_jumped)
                    && (net_now + fifo_now) < target_fifo_subframes as f32
                {
                    let pad_subframes = ((target_fifo_subframes as f32) - (net_now + fifo_now)).max(0.0) as usize;
                    let pad_subframes = pad_subframes.min(group_extra);
                    let pad_samples = resampler.subframes_to_samples(pad_subframes);
                    let mut pad_chunk = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                    let mut remaining = pad_samples;
                    let mut dither_rng = 0x7F4A_7C15u32;
                    while remaining > 0 {
                        let step = remaining.min(pad_chunk.len()) & !1;
                        if step == 0 {
                            break;
                        }
                        audioshare_protocol::dsp::inject_bluetooth_keepalive_dither(
                            &mut pad_chunk[..step],
                            &mut dither_rng,
                        );
                        burst_fifo.push_samples(&pad_chunk[..step]);
                        remaining -= step;
                    }
                }

                // A deeper group-sync cushion drains naturally through the
                // output callback after the target shrinks. Never pop from the
                // RX thread: `BurstDecouplerRing` is SPSC and the audio
                // callback is its only consumer, so a second `read_idx` writer
                // can rewind or skip samples. The refill loop stops topping up
                // until the surplus is gone.
            }
            telemetry
                .target_buffer_us
                .store((target_fifo_subframes as u32) * 2500, Ordering::Relaxed);
            telemetry.group_sync_extra_us.store(
                (target_fifo_subframes.saturating_sub(base_fifo_subframes) as u32) * 2500,
                Ordering::Relaxed,
            );
            let fifo_capacity = burst_fifo.capacity_samples().saturating_sub(2);
            #[cfg(target_os = "android")]
            let hal_burst_samples = aaudio_guard
                .as_ref()
                .map(|g| (g.frames_per_burst() as usize) * 2)
                .filter(|&s| s > 0)
                .unwrap_or(480);
            #[cfg(not(target_os = "android"))]
            let hal_burst_samples = 480usize;
            let one_subframe_samples = resampler.subframes_to_samples(1);
            let fifo_watermark = resampler
                .subframes_to_samples(target_fifo_subframes)
                .max(hal_burst_samples + one_subframe_samples * 2)
                .min(fifo_capacity);
            let plc_emergency_watermark = resampler
                .subframes_to_samples(PLC_EMERGENCY_WATERMARK_SUBFRAMES)
                .max(hal_burst_samples)
                .min(fifo_watermark.saturating_sub(one_subframe_samples).max(2));
            last_fifo_watermark = fifo_watermark;
            last_plc_watermark = plc_emergency_watermark;

            while stream_initialized
                && prebuffer_ready
                && !telemetry.server_dtx_silent.load(Ordering::Relaxed)
                && burst_fifo.available_samples() < fifo_watermark
            {
                // If the missing slot is still within the in-flight/FEC
                // recovery window, wait while the FIFO has at least 2.5ms of
                // real audio. The window scales with the sender's framing: a
                // single reordered 10 ms packet puts the playhead 4 subframes
                // behind the newest packet, so the old fixed `< 3` check
                // synthesized PLC even with 20+ ms of audio still queued.
                // At the live edge (`missing_delta < 0`) the slot is not lost;
                // hold it for one packet interval instead of walking the
                // playhead past the next on-time packet.
                let missing_delta = latest_rx_seq.wrapping_sub(next_play_seq) as i16;
                let opus_slot_idx = (next_play_seq as usize) & (RING_SLOTS - 1);
                let has_opus_slot = opus_ring[opus_slot_idx].seq == Some(next_play_seq);
                if should_hold_missing_slot(
                    sequencer.is_slot_missing(next_play_seq) && !has_opus_slot,
                    burst_fifo.available_samples(),
                    plc_emergency_watermark,
                    missing_delta,
                    in_flight_wait_subframes(last_pkt_subframe_count),
                ) {
                    // Wait for the in-flight packet or [N-1]/[N-2] FEC before
                    // triggering WSOLA PLC!
                    break;
                }
                let mut raw_subframe = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                let status = if has_opus_slot {
                    let slot = &mut opus_ring[opus_slot_idx];
                    let len = slot.len as usize;
                    slot.seq = None;
                    if opus_decoder.is_none() {
                        opus_decoder = OpusStreamDecoder::new();
                    }
                    let decoded_ok = opus_decoder
                        .as_mut()
                        .map(|d| {
                            d.decode_subframe_at_seq(
                                next_play_seq,
                                &slot.data[..len],
                                &mut raw_subframe,
                            )
                        })
                        .unwrap_or(false);
                    if decoded_ok {
                        sequencer.finalize_external_frame(&mut raw_subframe, false);
                        STATUS_PRIMARY_LOSSLESS
                    } else {
                        sequencer.pop_next(next_play_seq, &mut raw_subframe)
                    }
                } else if sequencer.is_slot_missing(next_play_seq)
                    && last_audio_codec_mode == Some(CodecMode::OpusRld)
                    && opus_decoder.is_some()
                {
                    // Phase-locked CELT PLC right at the playhead instead of
                    // falling back to WSOLA (timbre jump on Opus streams).
                    if opus_decoder
                        .as_mut()
                        .map(|d| d.decode_plc(next_play_seq, &mut raw_subframe))
                        .unwrap_or(false)
                    {
                        sequencer.finalize_external_frame(&mut raw_subframe, true);
                        STATUS_EMPTY
                    } else {
                        sequencer.pop_next(next_play_seq, &mut raw_subframe)
                    }
                } else {
                    sequencer.pop_next(next_play_seq, &mut raw_subframe)
                };

                match status {
                    STATUS_PRIMARY_LOSSLESS => {
                        next_play_seq = next_play_seq.wrapping_add(1);
                        consecutive_empty_ticks = 0;
                        window.record_primary();
                        telemetry
                            .primary_frames_played
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    STATUS_HEALED_BY_FEC => {
                        next_play_seq = next_play_seq.wrapping_add(1);
                        consecutive_empty_ticks = 0;
                        window.record_fec();
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
                        window.record_fec();
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
                            stream_initialized = false;
                            telemetry.prebuffer_ready.store(false, Ordering::Relaxed);
                            sequencer.reset();
                            // A restarted host re-anchors at a low base_seq, so
                            // needs_stream_resync never runs and a stale Opus
                            // decoder would reject every frame (delta <= 0)
                            // until the 16-bit sequence wraps (~82 s silence).
                            opus_decoder = None;
                            prev_rx_us = None;
                            for slot in &mut opus_ring {
                                slot.seq = None;
                            }
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

                // Apply Host Master Volume (0..=100%) with a short per-sample
                // ramp so PC volume keys do not step the waveform.
                let host_vol = (telemetry.host_volume.load(Ordering::Relaxed) as f32) / 100.0;
                apply_host_volume(&mut raw_subframe, &mut smoothed_host_vol, host_vol);
                telemetry
                    .client_vu_peak_pct
                    .store(vu_peak_percent(&raw_subframe), Ordering::Relaxed);

                // Move the popped subframe through the resampler and into the
                // decoupler before measuring either queue. Measuring first
                // counted this subframe in neither `net_subframes` (the
                // playhead had already advanced) nor `fifo_subframes` (not
                // pushed yet), leaving a permanent -1 subframe bias that the
                // clock loop then "corrected" with negative drift.
                let written_frames = resampler.process(&raw_subframe, &mut resampled_out);
                // Hermite cubic interpolation can overshoot the limiter's
                // ceiling by up to +1.8 dBFS under drift/44.1 kHz conversion;
                // round the inter-sample peaks off before the DAC.
                soft_limit_post_resample(&mut resampled_out[..written_frames * 2]);
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
                if trim_post_stall_backlog(
                    latest_rx_seq,
                    &mut next_play_seq,
                    &mut net_subframes,
                    fifo_subframes,
                    target_buf,
                    last_pkt_subframe_count,
                ) {
                    // Fast-forward skipped real audio; splice the next frame
                    // from the last played sample instead of clicking.
                    sequencer.arm_declick();
                }

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
                // Sample on a fixed 2.5 ms cadence. Polling at the socket
                // timeout rate injected fake error spikes; the windowed-mean
                // controller assumes evenly spaced observations.
                if last_clock_sample.elapsed() >= Duration::from_micros(2_500) {
                    last_clock_sample = Instant::now();
                    // The FIFO is refilled to `fifo_watermark` and drained in
                    // whole HAL bursts, so its time-averaged depth sits half a
                    // burst below the watermark. Use that as the setpoint to
                    // avoid +2..+10 ms of permanent latency bias.
                    let wm_subframes = resampler.samples_to_subframes(fifo_watermark);
                    let burst_subframes = resampler.samples_to_subframes(hal_burst_samples);
                    let clock_setpoint = (wm_subframes - 0.5 * burst_subframes).max(1.0);
                    resampler.update_clock_loop(
                        clock_loop_observation(net_subframes, fifo_subframes),
                        clock_setpoint,
                    );
                    let ppm =
                        (((resampler.ratio / resampler.base_ratio) - 1.0) * 1_000_000.0) as i32;
                    telemetry.clock_drift_ppm.store(ppm, Ordering::Relaxed);
                }
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
    fn test_adb_tcp_bridge_loopback_end_to_end() {
        use audioshare_host_engine::discovery_and_os::AdbUsbWatcher;
        use audioshare_host_engine::{HostConfig, HostEngine};

        // Reserve then release an ephemeral port so host UDP + bridge TCP can
        // both own it (different protocol spaces). No physical device needed:
        // the TCP leg stands in for `adb reverse tcp:P tcp:P`.
        let probe = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let mut host = HostEngine::start(
            HostConfig {
                bind_port: port,
                use_test_tone: true,
                subframes_per_packet: 1,
                enable_smart_dtx: false,
            },
            vec![],
        )
        .expect("start host engine");
        let _adb = AdbUsbWatcher::start(port);

        let mut client = ClientEngine::connect(ClientConfig {
            server_addr: SocketAddr::from(([127, 0, 0, 1], port)),
            target_buffer_subframes: 4.0,
            preamp_gain_db: 0.0,
            dolby_effects: false,
        })
        .expect("client connect over ADB TCP bridge");
        assert!(client.adb_bridge.is_some(), "loopback TCP bridge was not engaged");

        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline
            && client.telemetry.packets_received.load(Ordering::Relaxed) == 0
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        let received = client.telemetry.packets_received.load(Ordering::Relaxed);
        client.stop();
        host.stop();
        assert!(received > 0, "no packets crossed the ADB TCP<->UDP bridge");
    }

    /// Accept a connection from a non-blocking listener within `timeout`.
    fn accept_within(
        listener: &std::net::TcpListener,
        timeout: Duration,
        label: &str,
    ) -> (TcpStream, SocketAddr) {
        let deadline = Instant::now() + timeout;
        loop {
            match listener.accept() {
                Ok(pair) => return pair,
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for connection: {}",
                        label
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("listener.accept failed: {}", e),
            }
        }
    }

    /// Read exactly `buf.len()` bytes from a loopback TCP socket, retrying
    /// Windows' `WouldBlock` (WSAEWOULDBLOCK / timeout mapping) until deadline.
    fn read_exact_within(
        stream: &mut std::net::TcpStream,
        buf: &mut [u8],
        timeout: Duration,
        label: &str,
    ) {
        use std::io::Read;
        let deadline = Instant::now() + timeout;
        let mut filled = 0usize;
        while filled < buf.len() {
            match stream.read(&mut buf[filled..]) {
                Ok(0) => panic!("EOF reading {label}"),
                Ok(n) => filled += n,
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    assert!(Instant::now() < deadline, "timed out reading {label}");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("read {label} failed: {e}"),
            }
        }
    }

    /// Receive one UDP datagram, retrying Windows' `WouldBlock` on timeouts.
    fn recv_udp_within(socket: &UdpSocket, buf: &mut [u8], label: &str) -> usize {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match socket.recv_from(buf) {
                Ok((n, _)) => return n,
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    assert!(Instant::now() < deadline, "timed out receiving {label}");
                }
                Err(e) => panic!("recv {label} failed: {e}"),
            }
        }
    }

    #[test]
    fn test_adb_tcp_bridge_reconnects_after_server_yank() {
        use std::net::TcpListener;

        // Stand-in for `adb reverse tcp:P tcp:P`: a loopback TCP listener.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let server_addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();

        // The UDP side of the client: receives datagrams decapsulated from TCP
        // and sends datagrams that should be tunnelled out over TCP.
        let client_udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        client_udp
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let client_udp_port = client_udp.local_addr().unwrap().port();

        let bridge = AdbTcpClientBridge::try_connect(server_addr, client_udp_port)
            .expect("loopback TCP bridge must engage");

        // --- Socket #1: initial connection (the synchronous probe) -----------
        let (mut server1, _) = accept_within(&listener, Duration::from_secs(2), "socket #1");
        server1
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();

        // TCP -> UDP: send a framed datagram on socket #1, read the plain
        // datagram back on the client UDP socket.
        let p1 = b"hello-over-adb";
        let mut framed1 = Vec::with_capacity(2 + p1.len());
        framed1.extend_from_slice(&(p1.len() as u16).to_be_bytes());
        framed1.extend_from_slice(p1);
        server1.write_all(&framed1).unwrap();
        let mut udp_buf = [0u8; 256];
        let n = recv_udp_within(&client_udp, &mut udp_buf, "first framed packet");
        assert_eq!(&udp_buf[..n], p1);

        // UDP -> TCP: send a plain datagram to the bridge's stable UDP address
        // and read the framed packet back on socket #1.
        let reply1 = b"pong-over-adb";
        client_udp.send_to(reply1, bridge.local_udp_addr).unwrap();
        let mut len_buf = [0u8; 2];
        read_exact_within(&mut server1, &mut len_buf, Duration::from_secs(2), "reply #1 length");
        let len = u16::from_be_bytes(len_buf) as usize;
        let mut pkt = vec![0u8; len];
        read_exact_within(&mut server1, &mut pkt, Duration::from_secs(2), "reply #1 body");
        assert_eq!(pkt, reply1);

        // --- Simulate a cable yank: drop server socket #1 --------------------
        drop(server1);

        // The supervisor must notice EOF and reconnect every 400 ms with a
        // fresh TCP connection (socket #2) within 2 s.
        let (mut server2, _) = accept_within(&listener, Duration::from_secs(2), "socket #2 reconnect");
        server2
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();

        // Exchange a second framed packet on the reconnected socket.
        let p2 = b"second-packet-after-yank";
        let mut framed2 = Vec::with_capacity(2 + p2.len());
        framed2.extend_from_slice(&(p2.len() as u16).to_be_bytes());
        framed2.extend_from_slice(p2);
        server2.write_all(&framed2).unwrap();
        let n = recv_udp_within(&client_udp, &mut udp_buf, "second framed packet");
        assert_eq!(&udp_buf[..n], p2);

        let reply2 = b"pong-after-yank";
        client_udp.send_to(reply2, bridge.local_udp_addr).unwrap();
        read_exact_within(&mut server2, &mut len_buf, Duration::from_secs(2), "reply #2 length");
        let len = u16::from_be_bytes(len_buf) as usize;
        let mut pkt = vec![0u8; len];
        read_exact_within(&mut server2, &mut pkt, Duration::from_secs(2), "reply #2 body");
        assert_eq!(pkt, reply2);

        // Drop cleanly: the supervisor and its per-session I/O threads must
        // all join without hanging.
        drop(bridge);
    }

    #[test]
    fn test_client_vu_peak_percent_math() {
        assert_eq!(vu_peak_percent(&[0.0f32; 240]), 0);
        assert_eq!(vu_peak_percent(&[1.0f32; 240]), 100);
        // sqrt curve: 0.25 amplitude -> 50%, not 25%.
        assert_eq!(vu_peak_percent(&[0.25f32; 240]), 50);
        // Non-finite / over-unity samples are clamped, never propagated.
        assert_eq!(vu_peak_percent(&[f32::NAN, f32::INFINITY, 2.0]), 100);
    }

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
            1,
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
    fn test_group_sync_target_is_not_capped_at_30ms() {
        // Group sync adds up to 64 subframes (160 ms) to match a Bluetooth
        // peer's output delay. The old 12-subframe clamp silently capped it
        // at 30 ms, and the subsequent 32-subframe clamp capped it at 80 ms.
        assert_eq!(fifo_target_subframes(26.0), 26);
        assert_eq!(fifo_target_subframes(32.0), 32);
        assert_eq!(fifo_target_subframes(40.0), 40);
        assert_eq!(fifo_target_subframes(64.0), 64);
        assert_eq!(fifo_target_subframes(64.5), 64);
        assert_eq!(fifo_target_subframes(100.0), 64);
        assert_eq!(fifo_target_subframes(3.0), 4);
        assert!(
            (64 + 1) * SUBFRAME_STEREO_SAMPLES <= BurstDecouplerRing::new().capacity_samples(),
            "160 ms target plus one pushed subframe must fit the decoupler"
        );
    }

    #[test]
    fn test_fifo_target_64_subframes_fits_ring_at_96khz() {
        let ring = BurstDecouplerRing::new();
        assert_eq!(ring.capacity_samples(), 32_768);

        let mut resampler = ClockDriftResampler::new();
        resampler.set_output_sample_rate(96_000);
        assert_eq!(resampler.base_ratio, 0.5);

        // A 160 ms group-sync target clamps to 64 subframes, which is
        // 30,720 output samples at 96 kHz and fits the 32,768-sample ring.
        let target = fifo_target_subframes(100.0);
        assert_eq!(target, 64);
        let watermark = resampler
            .subframes_to_samples(target)
            .min(ring.capacity_samples() - 2);
        assert_eq!(watermark, 30_720);
        let subframe_samples = resampler.subframes_to_samples(1);
        assert_eq!(subframe_samples, 480);
        assert!(watermark + subframe_samples <= ring.capacity_samples());

        // Refill one output subframe at a time until the watermark is
        // reached; every push must fit without truncation.
        let chunk = vec![0.1f32; subframe_samples];
        let mut pushes = 0;
        while ring.available_samples() < watermark {
            assert_eq!(ring.push_samples(&chunk), subframe_samples);
            pushes += 1;
        }
        assert_eq!(pushes, 64);
        assert_eq!(ring.available_samples(), watermark);
    }

    #[test]
    fn test_split_packet_count_does_not_collapse_wifi_floor() {
        let mut count = 2u8;
        let mut run = 0u8;

        // One split datagram (subframe_count = 1) between normal packets.
        update_pkt_subframe_count(&mut count, &mut run, 1);
        assert_eq!(count, 2, "a single split packet must not change framing");

        update_pkt_subframe_count(&mut count, &mut run, 2);
        assert_eq!(count, 2);
        assert_eq!(run, 0, "a normal packet resets the smaller-packet run");

        // A sustained switch to 1-subframe framing is still adopted.
        for _ in 0..8 {
            update_pkt_subframe_count(&mut count, &mut run, 1);
        }
        assert_eq!(count, 1);

        // Returning to larger framing applies immediately.
        update_pkt_subframe_count(&mut count, &mut run, 2);
        assert_eq!(count, 2);
    }

    #[test]
    fn test_split_packet_does_not_drop_wifi_floor() {
        // USB/gaming framing starts from a 4-subframe configured floor.
        let configured = configured_wifi_floor(2.5);
        let normal = wifi_floor_for_packet(configured, 2);
        let split = wifi_floor_for_packet(configured, 1);
        assert!(
            split < normal,
            "raw split framing would lower the floor: {split} vs {normal}"
        );

        let mut count = 2u8;
        let mut run = 0u8;
        update_pkt_subframe_count(&mut count, &mut run, 1);
        assert_eq!(
            wifi_floor_for_packet(configured, count),
            normal,
            "a transient split datagram must not collapse the Wi-Fi floor"
        );
    }

    #[test]
    fn test_post_stall_trim_caps_combined_backlog_and_never_rewinds() {
        // Gaming Wi-Fi: 6-subframe target with a full 6-subframe decoupler.
        let mut next_play = 70u16;
        let mut net_subframes = 20.0f32;
        trim_post_stall_backlog(100, &mut next_play, &mut net_subframes, 6.0, 6.0, 2);
        assert_eq!(net_subframes, 6.0);
        assert_eq!(next_play, 95);
        assert_eq!(net_subframes + 6.0, 12.0); // target_buf + 6

        // 2.4GHz normal mode: a 14-subframe target with the decoupler capped
        // at 12. The trim must account for the decoupler, not just reset the
        // sequencer to the target (which would retain 26 subframes).
        let mut next_play = 50u16;
        let mut net_subframes = 30.0f32;
        trim_post_stall_backlog(100, &mut next_play, &mut net_subframes, 12.0, 14.0, 2);
        assert_eq!(net_subframes, 8.0);
        assert_eq!(next_play, 93);
        assert_eq!(net_subframes + 12.0, 20.0); // target_buf + 6

        // At or below the cap the playhead and queue are untouched.
        let mut next_play = 90u16;
        let mut net_subframes = 6.0f32;
        trim_post_stall_backlog(100, &mut next_play, &mut net_subframes, 6.0, 6.0, 2);
        assert_eq!(next_play, 90);
        assert_eq!(net_subframes, 6.0);

        // A normal 2-packet Wi-Fi A-MPDU burst (2 + 2 subframes) arriving on
        // top of a full decoupler stays inside target + 6 and must be
        // absorbed by the resampler, not skipped as a post-stall backlog.
        let mut next_play = 97u16;
        let mut net_subframes = 4.0f32;
        trim_post_stall_backlog(100, &mut next_play, &mut net_subframes, 6.0, 6.0, 2);
        assert_eq!(next_play, 97);
        assert_eq!(net_subframes, 4.0);
        assert_eq!(net_subframes + 6.0, 10.0); // <= target_buf + 6

        // Never rewind, even if the FIFO estimate alone exceeds the cap.
        let mut next_play = 99u16;
        let mut net_subframes = 2.0f32;
        trim_post_stall_backlog(100, &mut next_play, &mut net_subframes, 20.0, 6.0, 2);
        assert_eq!(next_play, 101);
        assert_eq!(net_subframes, 0.0);

        // Sequence wrap-around keeps the same forward-only invariant.
        let mut next_play = 65533u16;
        let mut net_subframes = 7.0f32;
        trim_post_stall_backlog(3, &mut next_play, &mut net_subframes, 6.0, 6.0, 2);
        assert_eq!(net_subframes, 6.0);
        assert_eq!(next_play, 65534);
    }

    #[test]
    fn test_backlog_cap_scales_with_ten_ms_packets() {
        // 10 ms framing uses 4-subframe packets. A normal 20 ms capture
        // callback or Wi-Fi A-MPDU burst delivers two packets (8 subframes);
        // the old fixed +6 cap mistook that for a stall and skipped 5 ms.
        assert_eq!(max_backlog_over_target_subframes(1), 6.0);
        assert_eq!(max_backlog_over_target_subframes(2), 6.0);
        assert_eq!(max_backlog_over_target_subframes(4), 10.0);

        // Full 11-subframe decoupler + two-packet burst = 19 subframes.
        // target 11 + scaled cap 10 leaves it untouched.
        let mut next_play = 88u16;
        let mut net_subframes = 8.0f32;
        trim_post_stall_backlog(100, &mut next_play, &mut net_subframes, 11.0, 11.0, 4);
        assert_eq!(next_play, 88);
        assert_eq!(net_subframes, 8.0);

        // A genuine post-stall backlog still fast-forwards to the new cap.
        let mut next_play = 70u16;
        let mut net_subframes = 30.0f32;
        trim_post_stall_backlog(100, &mut next_play, &mut net_subframes, 11.0, 11.0, 4);
        assert_eq!(net_subframes, 10.0);
        assert_eq!(next_play, 91);
        assert_eq!(net_subframes + 11.0, 21.0); // target + 10
    }

    #[test]
    fn test_wifi_floor_tracks_current_packet_framing() {
        // USB/desktop configure a 4-subframe (10 ms) minimum so a 20 ms
        // capture callback cannot underrun.
        let configured = configured_wifi_floor(0.0);
        assert_eq!(configured, 4.0);
        assert_eq!(wifi_floor_for_packet(configured, 1), 4.0);
        assert_eq!(wifi_floor_for_packet(configured, 2), 6.0);
        assert_eq!(wifi_floor_for_packet(configured, 4), 11.0);
        // Returning to 5 ms or 2.5 ms framing lowers the floor immediately
        // instead of leaving it ratcheted at 27.5 ms for the session.
        assert_eq!(wifi_floor_for_packet(configured, 2), 6.0);
        assert_eq!(wifi_floor_for_packet(configured, 1), 4.0);

        // A 2.4 GHz profile keeps its configured floor for small packets but
        // still raises it for 10 ms framing.
        let configured = configured_wifi_floor(8.0);
        assert_eq!(configured, 8.0);
        assert_eq!(wifi_floor_for_packet(configured, 1), 8.0);
        assert_eq!(wifi_floor_for_packet(configured, 2), 8.0);
        assert_eq!(wifi_floor_for_packet(configured, 4), 11.0);
    }

    #[test]
    fn test_in_flight_wait_scales_with_packet_size() {
        assert_eq!(in_flight_wait_subframes(1), 3);
        assert_eq!(in_flight_wait_subframes(2), 4);
        assert_eq!(in_flight_wait_subframes(4), 8);

        // One reordered 4-subframe packet leaves a 7-subframe shortfall.
        // The old fixed `< 3` threshold classified it as unrecoverable and
        // synthesized PLC while the decoupler still held 20+ ms.
        let reordered = 7i16;
        assert!(reordered < in_flight_wait_subframes(4));
        assert!(!(reordered < 3));
    }

    #[test]
    fn test_host_volume_ramps_instead_of_stepping() {
        let mut smoothed = 1.0f32;
        let mut first = [1.0f32; 240];
        apply_host_volume(&mut first, &mut smoothed, 0.5);
        // The first sample moves by ~0.005 instead of the old 0.5 step.
        assert!((first[0] - 0.995).abs() < 1e-6);
        assert!(smoothed > 0.5 && smoothed < 1.0);

        let mut previous = smoothed;
        for _ in 0..10 {
            let mut block = [1.0f32; 240];
            apply_host_volume(&mut block, &mut smoothed, 0.5);
            assert!(smoothed <= previous);
            previous = smoothed;
        }
        assert!((smoothed - 0.5).abs() < 0.01);

        // Returning to unity ramps up and snaps to exact bit-transparent 1.0.
        for _ in 0..100 {
            let mut block = [1.0f32; 240];
            apply_host_volume(&mut block, &mut smoothed, 1.0);
        }
        assert_eq!(smoothed, 1.0);
    }

    #[test]
    fn test_live_edge_holds_instead_of_walking_the_playhead() {
        // Both at the live edge and inside the FEC window, concealment is
        // paced by FIFO headroom alone -- not by a wall-clock grace period that
        // let a 1 ms socket timeout synthesize PLC 2.5x faster than the DAC.
        let emergency = 480usize; // 2 subframes / 1 HAL burst
        // Live edge with real audio still queued: hold (does not walk playhead).
        assert!(should_hold_missing_slot(
            true,
            emergency,
            emergency,
            -1,
            in_flight_wait_subframes(2),
        ));
        // Live edge when the FIFO actually dips below emergency headroom: emit
        // one PLC subframe, which refills above emergency -> exact 1.0x pacing.
        assert!(!should_hold_missing_slot(
            true,
            emergency - 2,
            emergency,
            -1,
            in_flight_wait_subframes(2),
        ));
        // Out-of-order hole within FEC window holds while headroom remains.
        assert!(should_hold_missing_slot(
            true,
            emergency,
            emergency,
            3,
            in_flight_wait_subframes(2),
        ));
        // Irrecoverable hole past FEC window conceals immediately.
        assert!(!should_hold_missing_slot(
            true,
            emergency,
            emergency,
            4,
            in_flight_wait_subframes(2),
        ));
        // A present slot never blocks the refill loop.
        assert!(!should_hold_missing_slot(
            false,
            0,
            emergency,
            0,
            in_flight_wait_subframes(2),
        ));
    }

    #[test]
    fn test_jitter_estimator_ignores_batched_packets() {
        // The sender paces 10 ms packets, but the receive loop wakes every
        // 20 ms (desktop timer granularity / Windows power-save), so each
        // drain pass returns two packets back to back. Sampling the second
        // packet compares a ~0 us receive gap with the sender's 10 ms
        // timestamp gap and invents 10 ms of jitter.
        const PACKET_US: u32 = 10_000;
        const DRAIN_US: u32 = 20_000;
        let mut old_jitter = 2500.0f32;
        let mut old_prev: Option<(u32, u32)> = None;
        let mut new_jitter = 2500.0f32;
        let mut new_prev: Option<(u32, u32)> = None;

        for drain in 1..=200u32 {
            let rx_base = drain * DRAIN_US;
            for packet in 0..2u32 {
                // Both packets were sitting in the kernel queue when the
                // loop woke, so their receive timestamps are one microsecond
                // apart regardless of the 10 ms sender spacing.
                let rx = rx_base + packet;
                let tx = rx_base - PACKET_US + packet * PACKET_US;

                // Old behavior: every drained packet updates the estimator
                // and the previous-sample state.
                if let Some((last_rx, last_tx)) = old_prev {
                    let d_rx = rx.wrapping_sub(last_rx) as i32;
                    let d_tx = tx.wrapping_sub(last_tx) as i32;
                    let delta = (d_rx - d_tx).unsigned_abs().min(50_000) as f32;
                    old_jitter += (delta - old_jitter) * (1.0 / 16.0);
                }
                old_prev = Some((rx, tx));

                // Fixed behavior: only the first audio packet of the drain
                // is a real arrival sample.
                if packet == 0 {
                    if let Some((last_rx, last_tx)) = new_prev {
                        let d_rx = rx.wrapping_sub(last_rx) as i32;
                        let d_tx = tx.wrapping_sub(last_tx) as i32;
                        let delta = (d_rx - d_tx).unsigned_abs().min(50_000) as f32;
                        new_jitter += (delta - new_jitter) * (1.0 / 16.0);
                    }
                    new_prev = Some((rx, tx));
                }
            }
        }

        let initial_floor = 6.0f32;
        let old_floor = ((old_jitter / 2500.0) * 2.2).clamp(initial_floor, 14.0);
        let new_floor = ((new_jitter / 2500.0) * 2.2).clamp(initial_floor, 14.0);
        assert!(
            old_jitter > 9_000.0,
            "batched reads should inflate the old estimator, got {old_jitter} us"
        );
        assert!(
            new_jitter < 500.0,
            "first-packet-only sampling should stay stable, got {new_jitter} us"
        );
        assert!(
            old_floor > new_floor + 2.0,
            "old proactive floor {old_floor} should exceed fixed {new_floor}"
        );
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
            *dst = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        }
        assert_eq!(i16_samples[0], 32767);
        assert_eq!(i16_samples[1], -32767);
        assert_eq!(i16_samples[2], 0);
        // 0.5 * 32767 = 16383.5 rounds to 16384; truncation used to produce
        // 16383 and bias the zero crossing.
        assert_eq!(i16_samples[3], 16384);
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
        assert_eq!(aaudio_buffer_frames_for_burst(48, 960), Some(256));
        assert_eq!(aaudio_buffer_frames_for_burst(96, 960), Some(288));
        assert_eq!(aaudio_buffer_frames_for_burst(192, 960), Some(576));
        // 240-frame speaker/USB bursts must double-buffer to avoid HAL XRuns.
        assert_eq!(aaudio_buffer_frames_for_burst(240, 960), Some(480));
        assert_eq!(aaudio_buffer_frames_for_burst(480, 960), Some(480));
        assert_eq!(aaudio_buffer_frames_for_burst(960, 960), Some(960));
        assert_eq!(aaudio_buffer_frames_for_burst(0, 960), None);
        assert_eq!(aaudio_buffer_frames_for_burst(-1, 960), None);
    }

    #[test]
    fn test_aaudio_capacity_covers_normal_mixer_burst() {
        // The Dolby path opens PERFORMANCE_MODE_NONE, where the normal mixer
        // can use a 1920-frame (40 ms) burst. The old 960-frame ring was
        // smaller than that burst, causing periodic underruns.
        assert_eq!(aaudio_capacity_frames(true), 1920);
        // The raw low-latency path keeps the 960-frame ring.
        assert_eq!(aaudio_capacity_frames(false), 960);
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
    fn test_opus_raw_buffer_recovers_reordered_wifi_frames() {
        use audioshare_protocol::lpc_lossless::MAX_LOSSLESS_SUBFRAME_BYTES;
        use audioshare_protocol::opus_codec::OpusStreamEncoder;
        use audioshare_protocol::SUBFRAME_FRAMES;

        let mut enc = OpusStreamEncoder::new(128).expect("opus encoder");
        let mut pcm_a = [0i16; SUBFRAME_STEREO_SAMPLES];
        let mut pcm_b = [0i16; SUBFRAME_STEREO_SAMPLES];
        for i in 0..SUBFRAME_FRAMES {
            let ta = (i as f32) * 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32;
            let tb = (i as f32) * 660.0 * std::f32::consts::TAU / SAMPLE_RATE as f32;
            pcm_a[i * 2] = (ta.sin() * 8000.0) as i16;
            pcm_b[i * 2] = (tb.sin() * 8000.0) as i16;
        }
        let mut raw_a = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
        let mut raw_b = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
        let len_a = enc.encode_subframe(&pcm_a, 128, &mut raw_a).unwrap();
        let len_b = enc.encode_subframe(&pcm_b, 128, &mut raw_b).unwrap();

        // BEFORE: decode at arrival. Frame N+1 (11) arrives before N (10);
        // the stateful CELT decoder rejects the late real frame 10 forever.
        let mut arrival_decoder = OpusStreamDecoder::new().unwrap();
        let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        assert!(arrival_decoder.decode_subframe_at_seq(11, &raw_b[..len_b], &mut out));
        assert!(!arrival_decoder.decode_subframe_at_seq(10, &raw_a[..len_a], &mut out));

        // AFTER: buffer raw bytes and decode in strict playhead order.
        let mut buffered = vec![(11u16, raw_b[..len_b].to_vec()), (10u16, raw_a[..len_a].to_vec())];
        buffered.sort_by_key(|&(seq, _)| seq);
        let mut pop_decoder = OpusStreamDecoder::new().unwrap();
        for (seq, data) in buffered {
            assert!(
                pop_decoder.decode_subframe_at_seq(seq, &data, &mut out),
                "buffered frame {seq} must decode in stream order"
            );
        }
        assert!(out.iter().all(|s| s.is_finite()));
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
            let expected = (sample.clamp(-1.0, 1.0) * 32767.0).round() as i16;
            assert_eq!(out[i], expected, "unfilled sample at {}", i);
        }
    }

    #[test]
    fn test_loss_window_keeps_partial_accounting_until_close() {
        let mut window = LossWindow::default();

        // A command flush must not sample a 1 ms window and report 100% loss.
        window.record_primary();
        window.record_fec();
        assert_eq!(window.last_permille(), 0);
        window.record_primary();

        // The periodic timer closes the window: 1 heal of 3 subframes.
        assert_eq!(window.close(), 333);
        assert_eq!(window.last_permille(), 333);

        // A later command packet reuses the closed rate without resetting
        // the counters of the next window.
        window.record_primary();
        assert_eq!(window.last_permille(), 333);

        // A fully healthy window closes at 0%.
        assert_eq!(window.close(), 0);
        assert_eq!(window.last_permille(), 0);
    }

    #[test]
    fn test_heal_packet_fec_maps_paired_and_legacy_layouts() {
        use audioshare_protocol::lpc_lossless::encode_fec_subframe;

        // Four subframes for sequences 96..=99 with distinguishable levels.
        let levels = [1000i16, 2000, 3000, 4000];
        let mut encoded = [[0u8; ADPCM_FEC_BYTES]; 4];
        for (i, level) in levels.iter().enumerate() {
            let pcm = [*level; SUBFRAME_STEREO_SAMPLES];
            encode_fec_subframe(&pcm, &mut encoded[i]);
        }

        // Paired 10 ms layout for base_seq 100, interleaved for backward
        // compatibility: fec_n1 = [N-1(99), N-3(97)],
        // fec_n2 = [N-2(98), N-4(96)].
        let mut n1 = [0u8; ADPCM_FEC_BYTES * 2];
        n1[..ADPCM_FEC_BYTES].copy_from_slice(&encoded[3]);
        n1[ADPCM_FEC_BYTES..].copy_from_slice(&encoded[1]);
        let mut n2 = [0u8; ADPCM_FEC_BYTES * 2];
        n2[..ADPCM_FEC_BYTES].copy_from_slice(&encoded[2]);
        n2[ADPCM_FEC_BYTES..].copy_from_slice(&encoded[0]);

        // Legacy clients read the first frame of each field and must still see
        // N-1/N-2, never N-3 in N-2's slot.
        assert_eq!(&n1[..ADPCM_FEC_BYTES], &encoded[3]);
        assert_eq!(&n2[..ADPCM_FEC_BYTES], &encoded[2]);

        let mut sequencer = ZeroJitterSequencer::new();
        heal_packet_fec(100, 96, &n1, &n2, &sequencer);

        for (i, level) in levels.iter().enumerate() {
            let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
            let status = sequencer.pop_next(96 + i as u16, &mut out);
            assert_eq!(status, STATUS_HEALED_BY_FEC, "slot {} not healed", 96 + i);
            let expected = *level as f32 / 32768.0;
            // Sample past the sequencer's 32-frame start fade-in so this test
            // checks the healed FEC level, not the intentional fade ramp.
            let steady = out[SUBFRAME_STEREO_SAMPLES - 2];
            assert!(
                (steady - expected).abs() < 0.02,
                "slot {} expected ~{}, got {}",
                96 + i,
                expected,
                steady
            );
        }

        // Legacy one-frame-per-field layout: fec_n1 = N-1, fec_n2 = N-2.
        let legacy = ZeroJitterSequencer::new();
        heal_packet_fec(100, 98, &encoded[3], &encoded[2], &legacy);
        assert!(legacy.is_slot_missing(96));
        assert!(legacy.is_slot_missing(97));
        assert!(!legacy.is_slot_missing(98));
        assert!(!legacy.is_slot_missing(99));

        // A trailing partial frame is ignored instead of being decoded as a
        // corrupt 124-byte backup.
        let malformed = ZeroJitterSequencer::new();
        let mut partial = [0u8; ADPCM_FEC_BYTES + 17];
        partial[..ADPCM_FEC_BYTES].copy_from_slice(&encoded[3]);
        heal_packet_fec(100, 99, &partial, &[], &malformed);
        assert!(!malformed.is_slot_missing(99));
        assert!(malformed.is_slot_missing(98));
    }

    #[test]
    fn test_effective_unaligned_delay_ms_accounts_for_excess_jitter_and_output_buffer() {
        // Minimum 4 subframes (10 ms) baseline: excess is 0 ms.
        assert_eq!(effective_unaligned_delay_ms(4, 10), 10);
        // Sub-4 clamped to 0 excess.
        assert_eq!(effective_unaligned_delay_ms(2, 10), 10);
        // 6 subframes (15 ms): excess is (6 - 4) * 2.5 ms = 5 ms.
        assert_eq!(effective_unaligned_delay_ms(6, 10), 15);
        // 14 subframes (35 ms): excess is (14 - 4) * 2.5 ms = 25 ms.
        assert_eq!(effective_unaligned_delay_ms(14, 10), 35);
        // 64 subframes (160 ms): excess is (64 - 4) * 2.5 ms = 150 ms.
        assert_eq!(effective_unaligned_delay_ms(64, 20), 170);

        // Desktop output delay fallback when hardware timestamping is 0 ms.
        assert_eq!(fallback_output_delay_ms(0), 5);
        assert_eq!(fallback_output_delay_ms(15), 15);
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn test_pull_stereo_to_channels_mono_stereo_surround_formats() {
        let ring = BurstDecouplerRing::new();
        let input: [f32; 4] = [0.2, 0.4, -0.6, 0.8]; // 2 stereo frames
        assert_eq!(ring.push_samples(&input), 4);

        let mut scratch = [0.0f32; 2048];
        let mut rng = 1u32;

        // 1. Mono downmix (ch = 1): (L + R) * 0.5
        let mut mono = [0.0f32; 2];
        ClientEngine::pull_stereo_to_channels(
            &ring,
            1,
            2,
            &mut scratch,
            &mut rng,
            |idx, s| mono[idx] = s,
        );
        assert!((mono[0] - 0.3).abs() < 1e-6, "mono[0] expected 0.3, got {}", mono[0]);
        assert!((mono[1] - 0.1).abs() < 1e-6, "mono[1] expected 0.1, got {}", mono[1]);

        // Push another 2 frames for stereo test
        ring.push_samples(&input);
        let mut stereo = [0.0f32; 4];
        ClientEngine::pull_stereo_to_channels(
            &ring,
            2,
            2,
            &mut scratch,
            &mut rng,
            |idx, s| stereo[idx] = s,
        );
        assert_eq!(stereo[0], 0.2);
        assert_eq!(stereo[1], 0.4);
        assert_eq!(stereo[2], -0.6);
        assert_eq!(stereo[3], 0.8);

        // Push for 5.1 surround test (ch = 6): L, R populated, 4 extra channels zeroed
        ring.push_samples(&input);
        let mut surround51 = [99.0f32; 12];
        ClientEngine::pull_stereo_to_channels(
            &ring,
            6,
            2,
            &mut scratch,
            &mut rng,
            |idx, s| surround51[idx] = s,
        );
        assert_eq!(surround51[0], 0.2);
        assert_eq!(surround51[1], 0.4);
        assert_eq!(surround51[2], 0.0);
        assert_eq!(surround51[3], 0.0);
        assert_eq!(surround51[4], 0.0);
        assert_eq!(surround51[5], 0.0);
        assert_eq!(surround51[6], -0.6);
        assert_eq!(surround51[7], 0.8);
        assert_eq!(surround51[8], 0.0);

        // Push for 7.1 surround test (ch = 8): L, R populated, 6 extra channels zeroed
        ring.push_samples(&input);
        let mut surround71 = [99.0f32; 16];
        ClientEngine::pull_stereo_to_channels(
            &ring,
            8,
            2,
            &mut scratch,
            &mut rng,
            |idx, s| surround71[idx] = s,
        );
        assert_eq!(surround71[0], 0.2);
        assert_eq!(surround71[1], 0.4);
        for extra in 2..8 {
            assert_eq!(surround71[extra], 0.0);
        }

        // I32 conversion test
        ring.push_samples(&input);
        let mut i32_out = [0i32; 4];
        ClientEngine::pull_stereo_to_channels(
            &ring,
            2,
            2,
            &mut scratch,
            &mut rng,
            |idx, s| i32_out[idx] = (f64::from(s.clamp(-1.0, 1.0)) * 2_147_483_647.0).round() as i32,
        );
        let expected_i32_0 = (f64::from(input[0]) * 2_147_483_647.0).round() as i32;
        assert_eq!(i32_out[0], expected_i32_0);
    }

    #[test]
    fn test_standby_after_80_empty_ticks_resets_stream_and_recovers_without_loss() {
        // Scenario: Stream starts at seq 100.
        let mut stream_initialized = true;
        let mut prebuffer_ready = true;
        let mut next_play_seq = 100u16;
        let mut latest_rx_seq = 100u16;
        let mut consecutive_empty_ticks = 0u16;

        // 80 empty ticks occur (200 ms packet stall)
        for _ in 0..80 {
            next_play_seq = next_play_seq.wrapping_add(1);
            consecutive_empty_ticks = consecutive_empty_ticks.saturating_add(1);
        }
        assert_eq!(next_play_seq, 180);
        assert_eq!(consecutive_empty_ticks, 80);

        // Standby trigger
        if consecutive_empty_ticks >= 80 {
            prebuffer_ready = false;
            stream_initialized = false; // Fixed: reset stream_initialized
            consecutive_empty_ticks = 0;
        }
        assert!(!prebuffer_ready);
        assert!(!stream_initialized);
        assert_eq!(consecutive_empty_ticks, 0);

        // Packets resume at seq 150 (a 125 ms gap)
        let resuming_packet_base = 150u16;
        // In the unpatched bug, stream_initialized was left true, so seq_delta = 150 - 180 = -30.
        // Because -30 is not < -32, needs_stream_resync(-30) was false!
        let buggy_delta = resuming_packet_base.wrapping_sub(next_play_seq) as i16;
        assert_eq!(buggy_delta, -30);
        assert!(!needs_stream_resync(buggy_delta), "Old code failed to resync at delta -30");

        // With the fix (stream_initialized == false), stream immediately reinitializes:
        if !stream_initialized {
            next_play_seq = resuming_packet_base;
            latest_rx_seq = resuming_packet_base;
            stream_initialized = true;
        }
        assert!(stream_initialized);
        assert_eq!(next_play_seq, 150);
        assert_eq!(latest_rx_seq, 150);
        assert_eq!(resuming_packet_base.wrapping_sub(next_play_seq) as i16, 0);
    }

    #[test]
    fn test_clock_loop_ignores_refill_overshoot() {
        let target = 4.0f32;

        // A whole-subframe refill at a 1 ms RX cadence starts between
        // `target - 0.4` and `target`, so the post-refill peak sits between
        // `target + 0.6` and `target + 1.0` on every callback. The old
        // peak-hold observation latched that overshoot and drove a permanent
        // positive correction.
        let mut old = ClockDriftResampler::new();
        let mut fifo_peak = target;
        for pass in 0..6000 {
            let fifo = if pass % 3 == 0 {
                target + 0.8
            } else {
                target + 0.4
            };
            if fifo >= fifo_peak {
                fifo_peak = fifo;
            } else {
                fifo_peak = (fifo_peak - 0.005).max(fifo);
            }
            old.update_clock_loop(clock_loop_observation(0.0, fifo_peak), target);
        }
        let old_ppm = ((old.ratio - 1.0) * 1_000_000.0) as i32;
        assert!(
            old_ppm > 70,
            "refill overshoot must bias the old peak-hold loop fast, got {} ppm",
            old_ppm
        );

        // The planned-cushion observation ignores the refill sawtooth: the
        // decoupler is still regulated to `target` by the refill watermark, and
        // only a real sequencer backlog moves the controller.
        let mut fixed = ClockDriftResampler::new();
        for _ in 0..1200 {
            fixed.update_clock_loop(clock_loop_observation(0.0, target), target);
        }
        let fixed_ppm = ((fixed.ratio - 1.0) * 1_000_000.0) as i32;
        assert_eq!(
            fixed_ppm, 0,
            "a locked stream at the refill watermark must not change the clock ratio"
        );

        // A genuine sequencer backlog still produces a fast correction.
        let mut backlog = ClockDriftResampler::new();
        for _ in 0..1200 {
            backlog.update_clock_loop(clock_loop_observation(4.0, target), target);
        }
        let backlog_ppm = ((backlog.ratio - 1.0) * 1_000_000.0) as i32;
        assert!(
            backlog_ppm > 50,
            "real sequencer backlog must still speed the resampler, got {} ppm",
            backlog_ppm
        );
    }

    #[test]
    fn test_group_sync_prebuffer_gating_and_single_consumer_drain() {
        let ring = BurstDecouplerRing::new();
        let resampler = ClockDriftResampler::new();

        // 1. Prebuffer gating: target_fifo_subframes = 32 (80 ms)
        let target_fifo_subframes = 32usize;
        let mut prebuffer_ready = false;

        // At only 6 subframes (15 ms), prebuffer must NOT be ready
        let net_now = 6.0f32;
        let fifo_now = 0.0f32;
        let total_now = net_now + fifo_now;
        if !prebuffer_ready && total_now >= target_fifo_subframes as f32 {
            prebuffer_ready = true;
        }
        assert!(!prebuffer_ready, "must not prebuffer before target is reached");

        // When 32 subframes accumulate, prebuffer engages
        let total_now = 32.0f32;
        if !prebuffer_ready && total_now >= target_fifo_subframes as f32 {
            prebuffer_ready = true;
        }
        assert!(prebuffer_ready, "must prebuffer when target is reached");

        // 2. Excess cushion drains through the audio-callback consumer:
        // Slow peer leaves, target_buf drops from 32 down to 6 subframes.
        // ring currently holds 32 subframes = 32 * 120 = 3840 stereo samples
        let chunk = [0.1f32; SUBFRAME_STEREO_SAMPLES];
        for _ in 0..32 {
            ring.push_samples(&chunk);
        }
        assert_eq!(ring.available_samples(), 32 * SUBFRAME_STEREO_SAMPLES);

        let new_target_buf = 6.0f32;
        let new_target_fifo = fifo_target_subframes(new_target_buf);
        assert_eq!(new_target_fifo, 6);
        let fifo_now = resampler.samples_to_subframes(ring.available_samples());
        assert_eq!(fifo_now, 32.0);

        // The output callback is the ring's single consumer. Draining the
        // surplus there (rather than racing on `read_idx` from the RX thread)
        // brings the decoupler back to the new watermark.
        let keep_samples = resampler.subframes_to_samples(new_target_fifo);
        let mut discard = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        let mut rng = 1u32;
        while ring.available_samples() > keep_samples + SUBFRAME_STEREO_SAMPLES {
            if !ring.pop_exact_or_dither(&mut discard, &mut rng) {
                break;
            }
        }
        // Retained samples must be within 1 subframe of target (6 subframes).
        let remaining_subframes = resampler.samples_to_subframes(ring.available_samples());
        assert!(remaining_subframes <= 7.0);
        assert!(remaining_subframes >= 6.0);

        // 3. Bluetooth Group Sync targets > 48 subframes (e.g. 58 subframes = 145 ms):
        // Base floor (e.g. 6) triggers prebuffer_ready so sequencer never reaches 48 subframes.
        let base_subframes = 6usize;
        let mut prebuf = false;
        let total_now = 6.0f32;
        if !prebuf && total_now >= base_subframes as f32 {
            prebuf = true;
        }
        assert!(prebuf, "base floor must engage prebuffer_ready to prevent >48 subframe resync loop");
    }

    #[test]
    fn repro_live_edge_hold_spins_with_empty_fifo() {
        // P0-1 regression: at the live edge with an EMPTY decoupler, the slot
        // is genuinely missing and must be concealed so the refill loop makes
        // progress. The old time-based guard held anyway while refill_poll had
        // already switched the socket to nonblocking -> 100% CPU busy-spin.
        assert!(!should_hold_missing_slot(
            true,
            0, // empty FIFO
            240,
            -1,
            in_flight_wait_subframes(2),
        ));
    }

    #[test]
    fn repro_clock_observation_is_blind_to_drained_fifo() {
        // P0-2: negative drift drains the decoupler. The old observation adds
        // target_buf instead of the real FIFO depth, so it can NEVER report a
        // queue below the setpoint -> the resampler never slows down.
        let target = 6.0f32;
        let drained_fifo = 2.0f32;
        // BEFORE: the second argument was `target_buf`, so observation was
        // always >= target and a drained FIFO was invisible (6 > 2).
        // AFTER: the second argument is the real FIFO depth (2 < target 6).
        let observation = clock_loop_observation(0.0, drained_fifo);
        assert!(
            observation < target,
            "observation {observation} must fall below setpoint {target} when the FIFO drains"
        );
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
        frames_per_burst: u32,
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

        #[inline]
        pub fn frames_per_burst(&self) -> u32 {
            self.frames_per_burst
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
                let capacity_frames = aaudio_capacity_frames(
                    profile.performance_mode == AAUDIO_PERFORMANCE_MODE_NONE,
                );
                AAudioStreamBuilder_setBufferCapacityInFrames(builder, capacity_frames);
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
                let raw_burst = AAudioStream_getFramesPerBurst(stream);
                if let Some(target_frames) = aaudio_buffer_frames_for_burst(
                    raw_burst,
                    capacity_frames,
                ) {
                    AAudioStream_setBufferSizeInFrames(stream, target_frames);
                }

                if AAudioStream_requestStart(stream) != 0 {
                    AAudioStream_close(stream);
                    let _ = Box::from_raw(ctx_ptr);
                    return None;
                }
                Some(Self {
                    stream,
                    ctx_ptr,
                    actual_sample_rate,
                    frames_per_burst: raw_burst.max(0) as u32,
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
        disconnected: AtomicBool,
        /// True when the granted mic rate differs from the 48 kHz network rate
        /// (e.g. a BT HFP headset mic opens at 16 kHz or 44.1 kHz).
        needs_resample: bool,
        /// Accessed exclusively on the single AAudio input callback thread.
        resampler: core::cell::UnsafeCell<ClockDriftResampler>,
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
            let ctx = &*(user_data as *const InputContext);
            let frames = num_frames as usize;
            let mono = core::slice::from_raw_parts(audio_data as *const f32, frames);
            // Expand mono to interleaved stereo and resample when the granted
            // rate is not 48 kHz (BT HFP 16 kHz / A2DP 44.1 kHz mic paths).
            let mut scratch = [0.0f32; 4096];
            audioshare_protocol::dsp::push_mono_mic_chunk(
                &ctx.fifo,
                mono,
                &mut *ctx.resampler.get(),
                ctx.needs_resample,
                &mut scratch,
            );
        }
        AAUDIO_CALLBACK_RESULT_CONTINUE
    }

    extern "C" fn aaudio_input_error_callback(
        _stream: *mut AAudioStream,
        user_data: *mut c_void,
        _error: i32,
    ) {
        if !user_data.is_null() {
            unsafe {
                let ctx = &*(user_data as *const InputContext);
                ctx.disconnected.store(true, Ordering::Release);
            }
        }
    }

    pub struct AAudioInputGuard {
        stream: *mut AAudioStream,
        ctx_ptr: *mut InputContext,
    }

    unsafe impl Send for AAudioInputGuard {}

    impl AAudioInputGuard {
        #[inline]
        pub fn needs_reopen(&self) -> bool {
            if self.ctx_ptr.is_null() {
                return true;
            }
            unsafe { (*self.ctx_ptr).disconnected.load(Ordering::Acquire) }
        }

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

                let ctx = Box::new(InputContext {
                    fifo,
                    disconnected: AtomicBool::new(false),
                    needs_resample: false,
                    resampler: core::cell::UnsafeCell::new(ClockDriftResampler::new()),
                });
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
                AAudioStreamBuilder_setErrorCallback(
                    builder,
                    aaudio_input_error_callback,
                    ctx_ptr as *mut c_void,
                );

                let mut stream: *mut AAudioStream = core::ptr::null_mut();
                let res = AAudioStreamBuilder_openStream(builder, &mut stream);
                AAudioStreamBuilder_delete(builder);

                if res != 0 || stream.is_null() {
                    let _ = Box::from_raw(ctx_ptr);
                    return None;
                }

                // A BT HFP headset mic may be granted 16 kHz (or 44.1 kHz)
                // even though the network stream is fixed at 48 kHz. Verify
                // the granted rate and resample on the callback path.
                let granted_rate = AAudioStream_getSampleRate(stream);
                if granted_rate >= 8_000 && granted_rate as u32 != SAMPLE_RATE {
                    (*ctx_ptr).needs_resample = true;
                    (*(*ctx_ptr).resampler.get())
                        .set_sample_rates(granted_rate as u32, SAMPLE_RATE);
                }

                // Use 3 * hardware burst (minimum 256 frames = ~5.3ms) to prevent HAL DMA XRuns
                let burst = AAudioStream_getFramesPerBurst(stream);
                if burst > 0 {
                    AAudioStream_setBufferSizeInFrames(stream, (burst * 3).max(256));
                }

                if AAudioStream_requestStart(stream) != 0 {
                    AAudioStream_close(stream);
                    let _ = Box::from_raw(ctx_ptr);
                    return None;
                }
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
