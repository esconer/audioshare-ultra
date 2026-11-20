#[cfg(target_os = "linux")]
use std::io::Write;
use std::net::{SocketAddr, UdpSocket};
#[cfg(target_os = "linux")]
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use audioshare_protocol::crypto::SessionKey;
use audioshare_protocol::dsp::{BurstDecouplerRing, ClockDriftResampler, TruePeakLimiter};
use audioshare_protocol::latency::DESKTOP_OUTPUT_BUFFER_MS;
use audioshare_protocol::lpc_lossless::{
    decode_fec_subframe_to_f32, decode_lossless_pcm24_to_f32, decode_lossless_subframe,
    decode_vbr_subframe_to_f32, ADPCM_FEC_BYTES, TAG_VBR_BFP,
};
use audioshare_protocol::opus_codec::{
    unpack_opus_red_blob, OpusStreamDecoder, MAX_OPUS_SUBFRAME_BYTES, TAG_OPUS_RED, TAG_OPUS_RLD,
};
use audioshare_protocol::sequencer::{
    ZeroJitterSequencer, RING_SLOTS, STATUS_EMPTY, STATUS_HEALED_BY_FEC, STATUS_PRIMARY_LOSSLESS,
};
use audioshare_protocol::sync_and_eq::GroupSyncEstimator;
use audioshare_protocol::wire::{
    ClientFeedbackPacket, CodecMode, ParsedPacket, CONTROL_FEEDBACK_LEN,
};
use audioshare_protocol::{MAX_SAFE_UDP_PAYLOAD, SUBFRAME_FRAMES, SUBFRAME_STEREO_SAMPLES};

use crate::{configure_realtime_udp_socket, promote_thread_realtime};

pub const PHONE_MIC_PORT: u16 = 65532;

/// Minimum decoupler cushion (6 subframes = 15 ms); scales up automatically if
/// the OS output callback period is larger (e.g. 1024 frames = 8.5 subframes).
const MIC_FIFO_MIN_TARGET_SUBFRAMES: f32 = 6.0;
/// Upper bound so a pathologically large callback cannot add unbounded call
/// latency. 20 subframes = 50 ms.
const MIC_FIFO_MAX_TARGET_SUBFRAMES: f32 = 20.0;

/// Compute the decoupler cushion from the native output callback size,
/// expressed in 48 kHz-equivalent frames.
///
/// Covering one full OS callback plus 3 subframes (7.5 ms) of network jitter
/// margin keeps the FIFO from underrunning on the next pull. The 240-frame
/// (5 ms) low-latency callback stays at the 15 ms floor; 1024/2048-frame
/// desktop callbacks grow to ~11.5/20 subframes instead of draining dry.
#[inline]
fn mic_fifo_target_subframes(observed_cb_frames: usize) -> f32 {
    let cb_subframes = observed_cb_frames as f32 / SUBFRAME_FRAMES as f32;
    (cb_subframes + 3.0).clamp(MIC_FIFO_MIN_TARGET_SUBFRAMES, MIC_FIFO_MAX_TARGET_SUBFRAMES)
}

/// Unaligned local delay reported back to a Party Host phone in
/// `PlayOnPcSpeakers` mode so PC speakers and peer phones lock in phase.
/// `VirtualMicForCalls` always reports 0 ms and never adds room-alignment
/// delay to a video call.
#[inline]
fn pc_receiver_effective_delay_ms(mode: PhoneRxRoutingMode, base_target_subframes: f32) -> u16 {
    if mode != PhoneRxRoutingMode::PlayOnPcSpeakers {
        return 0;
    }
    // Each subframe beyond the 4-subframe (10 ms) base cushion adds 2.5 ms of
    // FIFO delay; the desktop output backend contributes a fixed buffer.
    let excess_fifo_ms = ((base_target_subframes.ceil() as u32).saturating_sub(4) * 5) / 2;
    let output_ms = DESKTOP_OUTPUT_BUFFER_MS.round() as u32;
    (excess_fifo_ms + output_ms).min(255) as u16
}

/// Apply Party Host group-sync alignment (`stream_id * 2` ms) for PC speaker
/// playback while keeping the video-call Virtual Mic latency untouched.
#[inline]
fn aligned_pc_receiver_target_subframes(
    mode: PhoneRxRoutingMode,
    base_target: f32,
    max_group_delay_ms: u16,
    local_delay_ms: u16,
) -> f32 {
    if mode != PhoneRxRoutingMode::PlayOnPcSpeakers || max_group_delay_ms == 0 {
        return base_target;
    }
    let sync = GroupSyncEstimator::new();
    sync.aligned_target_subframes(base_target, max_group_delay_ms, local_delay_ms)
        .clamp(MIC_FIFO_MIN_TARGET_SUBFRAMES, 64.0)
}

/// Rolling FEC/loss rate in permille for one feedback window. Recovered or
/// lost subframes count against the total number of played subframes.
#[inline]
fn fb_fec_rate_permille(primary_frames: u32, recovered_or_lost: u32) -> u16 {
    let total = (primary_frames + recovered_or_lost).max(1);
    ((recovered_or_lost * 1000) / total).min(1000) as u16
}

/// Build the signed AEAD v2 feedback datagram a PC `PlayOnPcSpeakers` receiver
/// sends back to the Party Host. Shared by the RX loop and the loopback test so
/// the wire encoding under test is exactly the one used in production.
fn build_pc_feedback(
    key: Option<&SessionKey>,
    feedback_seq: u32,
    fec_rate_permille: u16,
    hardware_delay_ms: u16,
    out: &mut [u8; CONTROL_FEEDBACK_LEN],
) {
    let packet = ClientFeedbackPacket {
        fec_rate_permille,
        bluetooth_delay_ms: 0,
        client_volume: 100,
        requested_mode: 0,
        hardware_delay_ms,
        feedback_seq,
        auth_tag: [0u8; 16],
    };
    packet.encode_signed(key, out);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhoneRxRoutingMode {
    /// Routes phone mic into a dedicated Virtual Microphone for Zoom / Discord / Google Meet calls (no self-echo!)
    VirtualMicForCalls = 0,
    /// Plays incoming phone audio directly on PC Speakers / Headphones (Phone-to-PC media streaming!)
    PlayOnPcSpeakers = 1,
}

/// Number of buffered subframes between the highest received sequence and the
/// next sequence to play, using wrapping 16-bit arithmetic.
fn buffered_subframes(latest_seq: u16, next_seq: u16) -> i16 {
    (latest_seq.wrapping_add(1).wrapping_sub(next_seq) as i16).max(0)
}

/// Phone-mic playhead rule: advance only when the slot contained audio or a
/// newer packet is already buffered.
///
/// The receiver ticks from the PC `Instant` clock. If the PC clock runs faster
/// than the phone (or a brief Wi-Fi pause leaves the timer running), blindly
/// incrementing `next_seq` walks the playhead ahead of `latest_seq`; every
/// later phone packet is then rejected as "past" even though it is the newest
/// audio available. Holding the playhead while the slot is empty and no newer
/// packet exists lets the phone sequence catch up.
#[inline]
fn should_advance_mic_playhead(status: u32, latest_seq: u16, next_seq: u16) -> bool {
    status != audioshare_protocol::sequencer::STATUS_EMPTY
        || (latest_seq.wrapping_sub(next_seq) as i16) >= 0
}

/// True when the tick loop should wait instead of queueing concealment into a
/// pull-driven phone-mic sink.
///
/// At the live edge (`latest_seq < next_seq`) no newer packet exists yet, so
/// the slot is late rather than lost. A hardware/CPAL sink pulls from the
/// decoupler, and one full subframe of real audio covers the next 2.5 ms tick;
/// hold the playhead instead of placing PLC in front of the packet that is
/// still in flight. Once less than one subframe remains, conceal so the sink
/// never starves.
#[inline]
fn should_hold_mic_live_edge(
    pull_sink_active: bool,
    slot_missing: bool,
    latest_seq: u16,
    next_seq: u16,
    fifo_samples: usize,
) -> bool {
    pull_sink_active
        && slot_missing
        && (latest_seq.wrapping_sub(next_seq) as i16) < 0
        && fifo_samples >= SUBFRAME_STEREO_SAMPLES
}

/// True when the first length-prefixed subframe in a primary bundle is Opus.
///
/// ADPCM FEC must never heal an Opus stream: Opus carries its own 2.5 ms
/// lookahead/PLC state, so splicing a zero-lookahead ADPCM frame into the
/// stream would jump the waveform by one subframe.
#[inline]
fn primary_bundle_is_opus(primary: &[u8]) -> bool {
    primary.get(2) == Some(&TAG_OPUS_RLD)
}

/// Heal one sequencer slot from a 124-byte ADPCM FEC frame. Only slots still
/// ahead of the playhead and genuinely missing are touched; a late primary
/// copy can always replace the concealment afterwards.
#[inline]
fn heal_fec_slot(sequencer: &ZeroJitterSequencer, seq: u16, next_seq: u16, payload: &[u8]) {
    if (seq.wrapping_sub(next_seq) as i16).is_negative() || !sequencer.is_slot_missing(seq) {
        return;
    }
    let mut pcm_f32 = [0.0f32; SUBFRAME_STEREO_SAMPLES];
    if decode_fec_subframe_to_f32(payload, &mut pcm_f32) {
        sequencer.try_heal_with_fec(seq, &pcm_f32);
    }
}

/// Heal every complete 124-byte frame in one FEC field. `first_back` is the
/// distance of the first frame from the packet base and `step` is the distance
/// between frames (1 for a legacy single-frame field, 2 for paired fields). A
/// trailing partial frame is ignored.
fn heal_fec_field(
    sequencer: &ZeroJitterSequencer,
    base_seq: u16,
    next_seq: u16,
    payload: &[u8],
    first_back: u16,
    step: u16,
) {
    let mut remaining = payload;
    let mut back = first_back;
    while remaining.len() >= ADPCM_FEC_BYTES {
        let (chunk, tail) = remaining.split_at(ADPCM_FEC_BYTES);
        heal_fec_slot(sequencer, base_seq.wrapping_sub(back), next_seq, chunk);
        remaining = tail;
        back = back.wrapping_add(step);
    }
}

/// Heal the slots covered by a packet's two FEC fields.
///
/// Normal framing carries one 124-byte frame per field (`N-1` and `N-2`).
/// Four-subframe 2.4 GHz framing carries 248 bytes per field, interleaved:
/// `fec_n1 = [N-1, N-3]` and `fec_n2 = [N-2, N-4]`, so losing one whole 10 ms
/// packet no longer leaves all four subframes unrecoverable. The interleaving
/// is a compatibility contract: the first frame of each field keeps its legacy
/// N-1/N-2 meaning, so older receivers still heal the correct slots.
fn heal_packet_fec(
    base_seq: u16,
    next_seq: u16,
    fec_n1: &[u8],
    fec_n2: &[u8],
    sequencer: &ZeroJitterSequencer,
) {
    let paired = fec_n1.len() >= ADPCM_FEC_BYTES * 2;
    let step = if paired { 2 } else { 1 };
    heal_fec_field(sequencer, base_seq, next_seq, fec_n1, 1, step);
    heal_fec_field(sequencer, base_seq, next_seq, fec_n2, 2, step);
}

/// Raw Opus payload buffered by sequence number. Decoding happens at playhead
/// pop time so a reordered Wi-Fi packet is never fed out of order into the
/// stateful CELT MDCT/PLC predictor (mirrors the client engine's `opus_ring`).
#[derive(Clone, Copy)]
struct OpusPacketSlot {
    seq: Option<u16>,
    len: u16,
    data: [u8; MAX_OPUS_SUBFRAME_BYTES],
    /// True when this payload was recovered from an Opus RED redundancy blob
    /// instead of arriving as a primary subframe. The playhead counts these as
    /// FEC-healed, never as primary audio.
    from_red: bool,
}

impl OpusPacketSlot {
    const EMPTY: Self = Self {
        seq: None,
        len: 0,
        data: [0u8; MAX_OPUS_SUBFRAME_BYTES],
        from_red: false,
    };
}

/// Status for a successfully decoded Opus slot at the playhead.
///
/// RED-recovered frames carry real encoded audio, but they are recovered
/// payloads, so they must be tallied as FEC-healed instead of primary. This
/// receiver's only pop-time telemetry is the `fb_primary_frames` /
/// `fb_recovered_or_lost` pair that feeds the Party Host FEC-permille feedback;
/// every non-primary status lands in `fb_recovered_or_lost`. If a dedicated
/// `HostTelemetry::fec_healed_frames` counter is added here later, `from_red`
/// at this pop site is where to increment it.
#[inline]
fn opus_pop_status(from_red: bool) -> u32 {
    if from_red {
        STATUS_HEALED_BY_FEC
    } else {
        STATUS_PRIMARY_LOSSLESS
    }
}

/// Heal missing `opus_ring` slots from one Opus RED blob carried in a
/// `WireHeader` FEC field.
///
/// Mirrors the client engine's receiver: only slots at or ahead of the
/// playhead, inside the ring window, not already backed by a primary frame,
/// and small enough for the fixed slot buffer are filled. Inserted slots are
/// flagged `from_red = true` so they are counted as FEC-healed, not primary,
/// when the playhead pops them.
fn heal_opus_red_slots(
    red_blob: &[u8],
    next_seq: u16,
    sequencer: &ZeroJitterSequencer,
    opus_ring: &mut [OpusPacketSlot],
) {
    if red_blob.first() != Some(&TAG_OPUS_RED) {
        return;
    }
    let _ = unpack_opus_red_blob(red_blob, |red_seq, red_payload| {
        let delta = red_seq.wrapping_sub(next_seq) as i16;
        if delta < 0
            || delta >= RING_SLOTS as i16
            || red_payload.len() > MAX_OPUS_SUBFRAME_BYTES
        {
            return;
        }
        let slot = &mut opus_ring[(red_seq as usize) & (RING_SLOTS - 1)];
        // A primary copy (or an earlier RED heal) already in the slot always
        // wins: never overwrite it or count the same recovery twice.
        if slot.seq == Some(red_seq) || sequencer.has_primary(red_seq) {
            return;
        }
        let len = red_payload.len();
        slot.data[..len].copy_from_slice(red_payload);
        slot.len = len as u16;
        slot.seq = Some(red_seq);
        slot.from_red = true;
    });
}

/// Track decoded PCM level and a returned latency-sonar chirp. Shared by the
/// arrival-time (non-Opus) and pop-time (Opus) decode paths so peak metering
/// stays correct when Opus frames are decoded at the playhead.
#[inline]
fn note_pcm_level(
    pcm: &[f32; SUBFRAME_STEREO_SAMPLES],
    peak: &std::sync::atomic::AtomicU64,
    chirp: &std::sync::atomic::AtomicU64,
) {
    let mut max_peak = 0.0f32;
    for &v in pcm.iter() {
        max_peak = max_peak.max(v.abs());
    }
    peak.store((max_peak * 100.0).min(100.0) as u64, Ordering::Relaxed);
    if audioshare_protocol::sync_and_eq::LatencySonar::detect_chirp_return(pcm) {
        let now_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;
        chirp.store(now_us, Ordering::Relaxed);
    }
}

/// Decode every length-prefixed subframe in a primary bundle.
///
/// The phone transmitter may bundle up to four 2.5 ms subframes per datagram.
/// Each decoded subframe is reported with its index so the caller can map it
/// to `base_seq + index`; reading only index 0 drops half of the audio in the
/// two-subframe Party Mode / PhoneAudioCaster path.
///
/// `base_seq` and `opus_decoder` are needed for the stateful Opus backend:
/// Opus must see frames in stream order, so the tagged payload is decoded with
/// `decode_subframe_with_plc`. The other codecs are self-identifying and stay
/// stateless.
///
/// `on_plc` receives every CELT concealment frame generated for a sequence gap,
/// phase-aligned with the Opus lookahead, so the caller can heal empty
/// sequencer slots. It must run before `on_subframe` for the frame that
/// exposed the gap; the packet's own audio is reported afterwards.
///
/// `on_opus` receives `(seq, raw_payload)` before any stateful decoding. When
/// it returns `true` the frame has been buffered for decode-at-pop (or is a
/// late duplicate that must be dropped) and is skipped here; returning `false`
/// keeps the original decode-immediately behaviour used by the unit tests.
fn decode_primary_bundle(
    primary: &[u8],
    subframe_count: u8,
    base_seq: u16,
    mut opus_decoder: Option<&mut Option<OpusStreamDecoder>>,
    mut on_plc: impl FnMut(u16, &[f32; SUBFRAME_STEREO_SAMPLES]),
    mut on_subframe: impl FnMut(usize, &[f32; SUBFRAME_STEREO_SAMPLES]),
    mut on_opus: impl FnMut(u16, &[u8]) -> bool,
) -> usize {
    let mut cursor = 0usize;
    let mut decoded = 0usize;
    for sub_idx in 0..subframe_count as usize {
        if cursor + 2 > primary.len() {
            break;
        }
        let sub_len = u16::from_be_bytes([primary[cursor], primary[cursor + 1]]) as usize;
        cursor += 2;
        if sub_len == 0 || cursor + sub_len > primary.len() {
            break;
        }

        let payload = &primary[cursor..cursor + sub_len];
        let mut pcm_f32 = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        let seq = base_seq.wrapping_add(sub_idx as u16);
        let ok = if payload.first() == Some(&TAG_OPUS_RLD) {
            // Decode-at-pop: hand raw Opus bytes to the caller before touching
            // the stateful CELT decoder. A buffered (or deliberately dropped
            // late) frame must never advance the MDCT predictor out of order.
            if on_opus(seq, payload) {
                cursor += sub_len;
                continue;
            }
            // Opus is stateful, so a decoder must be supplied. Without one,
            // treat the frame as undecodable instead of guessing a codec.
            if let Some(decoder_slot) = opus_decoder.as_deref_mut() {
                if decoder_slot.is_none() {
                    *decoder_slot = OpusStreamDecoder::new();
                }
                decoder_slot
                    .as_mut()
                    .map(|decoder| {
                        decoder.decode_subframe_with_plc(seq, payload, &mut pcm_f32, &mut on_plc)
                    })
                    .unwrap_or(false)
            } else {
                false
            }
        } else {
            // A non-Opus payload means the host switched back to VBR or
            // lossless. Drop the Opus decoder so a later switch starts with
            // clean MDCT/band-energy state instead of stale history.
            if let Some(decoder_slot) = opus_decoder.as_deref_mut() {
                *decoder_slot = None;
            }
            if payload.first() == Some(&TAG_VBR_BFP) {
                // The Party host switches between lossless and VBR without
                // changing the header's mode. A VBR subframe is
                // self-identifying through its tag, so sniff every payload
                // instead of trusting one codec mode for the whole stream.
                decode_vbr_subframe_to_f32(payload, &mut pcm_f32)
            } else if decode_lossless_pcm24_to_f32(payload, &mut pcm_f32) {
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
        if ok {
            on_subframe(sub_idx, &pcm_f32);
            decoded += 1;
        }
        cursor += sub_len;
    }
    decoded
}

#[allow(clippy::too_many_arguments)]
fn push_one_subframe(
    next_seq: &mut u16,
    latest_seq: u16,
    mode: PhoneRxRoutingMode,
    sequencer: &mut ZeroJitterSequencer,
    mic_gate: &mut audioshare_protocol::sync_and_eq::MicRumbleAndNoiseGate,
    limiter: &mut TruePeakLimiter,
    resampler: &mut ClockDriftResampler,
    resampled_out: &mut [f32],
    mic_fifo: &BurstDecouplerRing,
    opus_decoder: &mut Option<OpusStreamDecoder>,
    opus_ring: &mut [OpusPacketSlot],
    opus_stream_active: &mut bool,
    peak: &AtomicU64,
    chirp: &AtomicU64,
) -> u32 {
    let mut frame = [0.0f32; SUBFRAME_STEREO_SAMPLES];
    let play_seq = *next_seq;
    let opus_idx = (play_seq as usize) & (RING_SLOTS - 1);
    let has_opus_slot = opus_ring[opus_idx].seq == Some(play_seq);
    let status = if has_opus_slot {
        // Strict in-order decode at the playhead. Raw payloads were buffered
        // on arrival, so the stateful CELT decoder never sees a reordered or
        // duplicate frame.
        let slot = &mut opus_ring[opus_idx];
        let len = slot.len as usize;
        let from_red = slot.from_red;
        slot.seq = None;
        slot.from_red = false; // clear so a future primary reusing this slot is not mis-tallied
        if opus_decoder.is_none() {
            *opus_decoder = OpusStreamDecoder::new();
        }
        let decoded_ok = opus_decoder
            .as_mut()
            .map(|decoder| decoder.decode_subframe_at_seq(play_seq, &slot.data[..len], &mut frame))
            .unwrap_or(false);
        *opus_stream_active = true;
        if decoded_ok {
            note_pcm_level(&frame, peak, chirp);
            // Splice with the sequencer's predictor/history so a following
            // WSOLA or FEC frame does not click at the codec boundary.
            sequencer.finalize_external_frame(&mut frame, false);
            opus_pop_status(from_red)
        } else {
            sequencer.pop_next(play_seq, &mut frame)
        }
    } else if *opus_stream_active && sequencer.is_slot_missing(play_seq) && opus_decoder.is_some() {
        // Playhead miss on an Opus stream: phase-locked CELT PLC instead of
        // WSOLA, which would jump timbre on a CELT-encoded stream.
        let plc_ok = opus_decoder
            .as_mut()
            .map(|decoder| decoder.decode_plc(play_seq, &mut frame))
            .unwrap_or(false);
        if plc_ok {
            note_pcm_level(&frame, peak, chirp);
            sequencer.finalize_external_frame(&mut frame, true);
            STATUS_EMPTY
        } else {
            sequencer.pop_next(play_seq, &mut frame)
        }
    } else {
        // Existing single-codec rebuild path (lossless / VBR / ADPCM FEC):
        // the sequencer holds the decoded PCM produced on arrival.
        let st = sequencer.pop_next(play_seq, &mut frame);
        if st != STATUS_EMPTY {
            // Rebuilding a non-Opus frame closes the Opus epoch; drop stale
            // MDCT state so a later switch cannot hit the 16-bit wrap lockout.
            *opus_stream_active = false;
            *opus_decoder = None;
        }
        st
    };
    if should_advance_mic_playhead(status, latest_seq, play_seq) {
        *next_seq = play_seq.wrapping_add(1);
    }
    if mode == PhoneRxRoutingMode::VirtualMicForCalls {
        mic_gate.process_in_place(&mut frame);
    }
    limiter.process_in_place(&mut frame);

    let frames = resampler.process(&frame, resampled_out);
    if frames > 0 {
        mic_fifo.push_samples(&resampled_out[..frames * 2]);
    }
    // Reported so the caller can tally primary vs. recovered/lost subframes
    // for the periodic Party Host loss-feedback packet.
    status
}

/// Feeds the received phone-mic PCM into a PulseAudio/PipeWire null sink with
/// `pacat`, while `module-remap-source` exposes that sink's monitor as the
/// `AudioShare_Phone_Mic` recording device.
#[cfg(target_os = "linux")]
struct LinuxVirtualMicOutput {
    child: Child,
    writer: Option<std::thread::JoinHandle<()>>,
    running: Arc<AtomicBool>,
}

#[cfg(target_os = "linux")]
impl LinuxVirtualMicOutput {
    fn start(fifo: Arc<BurstDecouplerRing>, running: Arc<AtomicBool>) -> Option<Self> {
        let mut child = Command::new("pacat")
            .args([
                "--playback",
                "--device=AudioShare_Phone_Mic_Sink",
                "--format=float32le",
                "--rate=48000",
                "--channels=2",
                "--raw",
                "--latency-msec=8",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        let Some(mut stdin) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        };

        let writer_running = Arc::clone(&running);
        let writer = match std::thread::Builder::new()
            .name("as-ultra-phone-mic-sink".into())
            .spawn(move || {
                let mut rng = 0x2468_ACE0u32;
                let mut frame = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                let mut bytes = vec![0u8; SUBFRAME_STEREO_SAMPLES * 4];
                while writer_running.load(Ordering::Relaxed) {
                    if fifo.available_samples() >= SUBFRAME_STEREO_SAMPLES {
                        fifo.pop_exact_or_dither(&mut frame, &mut rng);
                        for (i, sample) in frame.iter().enumerate() {
                            bytes[i * 4..i * 4 + 4].copy_from_slice(&sample.to_le_bytes());
                        }
                        if stdin.write_all(&bytes).is_err() {
                            break;
                        }
                    } else {
                        std::thread::sleep(Duration::from_micros(500));
                    }
                }
            }) {
            Ok(handle) => handle,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        };

        Some(Self {
            child,
            writer: Some(writer),
            running,
        })
    }
}

#[cfg(target_os = "linux")]
impl Drop for LinuxVirtualMicOutput {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(handle) = self.writer.take() {
            let _ = handle.join();
        }
    }
}

#[inline]
fn session_key_from_pin(pin: Option<&str>) -> Option<SessionKey> {
    pin.map(str::trim)
        .filter(|pin| !pin.is_empty())
        .map(SessionKey::from_pin)
}

pub struct PhoneMicReceiver {
    running: Arc<AtomicBool>,
    /// Shared with the RX thread so a PIN edit in the desktop UI applies to
    /// already-running receivers instead of only to the next start.
    session_key: Arc<Mutex<Option<SessionKey>>>,
    pub mic_packets_received: Arc<AtomicU64>,
    pub mic_peak_level_pct: Arc<AtomicU64>,
    pub last_chirp_rx_timestamp_us: Arc<AtomicU64>,
    #[cfg(target_os = "linux")]
    pactl_mic_module_id: Option<String>,
    #[cfg(target_os = "linux")]
    pactl_mic_source_module_id: Option<String>,
    #[cfg(target_os = "linux")]
    linux_output: Option<LinuxVirtualMicOutput>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl PhoneMicReceiver {
    pub fn start(port: u16) -> Result<Self, String> {
        Self::start_with_mode(port, PhoneRxRoutingMode::VirtualMicForCalls)
    }

    pub fn start_with_mode(port: u16, mode: PhoneRxRoutingMode) -> Result<Self, String> {
        Self::start_with_mode_and_pin(port, mode, None)
    }

    /// Start the receiver with an optional pairing PIN. When set, incoming
    /// phone-mic datagrams are ChaCha20-encrypted and MAC-authenticated, so
    /// the receiver must decrypt before parsing the codec payload.
    pub fn start_with_mode_and_pin(
        port: u16,
        mode: PhoneRxRoutingMode,
        pin: Option<&str>,
    ) -> Result<Self, String> {
        let session_key = Arc::new(Mutex::new(session_key_from_pin(pin)));
        let socket = UdpSocket::bind(("0.0.0.0", port))
            .map_err(|e| format!("Failed to bind Phone Mic UDP port {}: {}", port, e))?;
        configure_realtime_udp_socket(&socket)
            .map_err(|e| format!("Failed to configure Phone Mic socket: {}", e))?;
        let _ = socket.set_nonblocking(true);

        #[cfg(target_os = "linux")]
        let (pactl_mic_module_id, pactl_mic_source_module_id) =
            if mode == PhoneRxRoutingMode::VirtualMicForCalls {
                // 1. Create underlying null sink to receive the phone's audio stream silently
                let sink_id = Command::new("pactl")
                    .args([
                        "load-module",
                        "module-null-sink",
                        "sink_name=AudioShare_Phone_Mic_Sink",
                        "sink_properties=device.description=AudioShare_Phone_Sink",
                    ])
                    .output()
                    .ok()
                    .and_then(|o| {
                        if o.status.success() {
                            Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                        } else {
                            None
                        }
                    });

                // 2. Create a true non-monitor Virtual Microphone (`module-remap-source`)
                //    so Zoom, Discord, Google Meet, Teams & Chrome show it as a real Microphone Input!
                let remap_id = Command::new("pactl")
                    .args([
                        "load-module",
                        "module-remap-source",
                        "master=AudioShare_Phone_Mic_Sink.monitor",
                        "source_name=AudioShare_Phone_Mic",
                        "source_properties=device.description=AudioShare_Phone_Microphone",
                    ])
                    .output()
                    .ok()
                    .and_then(|o| {
                        if o.status.success() {
                            Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                        } else {
                            None
                        }
                    });

                let _ = Command::new("pactl")
                    .args(["set-default-source", "AudioShare_Phone_Mic"])
                    .output();
                (sink_id, remap_id)
            } else {
                (None, None)
            };

        let running = Arc::new(AtomicBool::new(true));
        let mic_packets_received = Arc::new(AtomicU64::new(0));
        let mic_peak_level_pct = Arc::new(AtomicU64::new(0));
        let last_chirp_rx_timestamp_us = Arc::new(AtomicU64::new(0));
        // Default to the low-latency 240-frame callback the sink asks for,
        // expressed in 48 kHz-equivalent frames. `ClientPreviewSink`
        // overwrites this with the backend's real callback duration as soon
        // as the first pull happens.
        let observed_cb_frames = Arc::new(AtomicUsize::new(240));

        let mic_fifo = Arc::new(BurstDecouplerRing::new());

        #[cfg(target_os = "linux")]
        let linux_output = if mode == PhoneRxRoutingMode::VirtualMicForCalls {
            LinuxVirtualMicOutput::start(Arc::clone(&mic_fifo), Arc::clone(&running))
        } else {
            None
        };
        #[cfg(target_os = "linux")]
        let linux_output_active = linux_output.is_some();

        let run_clone = Arc::clone(&running);
        let pkts_clone = Arc::clone(&mic_packets_received);
        let peak_clone = Arc::clone(&mic_peak_level_pct);
        let chirp_clone = Arc::clone(&last_chirp_rx_timestamp_us);
        let mic_fifo_thread = Arc::clone(&mic_fifo);
        let session_key_thread = Arc::clone(&session_key);

        let handle = std::thread::Builder::new()
            .name("as-ultra-phone-mic-rx".into())
            .spawn(move || {
                promote_thread_realtime();
                let mut sequencer = ZeroJitterSequencer::new();
                // +6 dB is a voice-mic pre-amp. Music cast to PC speakers must
                // stay at unity gain and only use the true-peak ceiling.
                let preamp_db = if mode == PhoneRxRoutingMode::VirtualMicForCalls {
                    6.0
                } else {
                    0.0
                };
                let mut limiter = TruePeakLimiter::new(preamp_db);
                let mut mic_gate = audioshare_protocol::sync_and_eq::MicRumbleAndNoiseGate::new();

                // On Linux the pacat writer consumes the FIFO and feeds the
                // AudioShare_Phone_Mic_Sink. On Windows, prefer VB-Cable's
                // "CABLE Input" endpoint; otherwise use the default output.
                // Do not retarget `PULSE_SINK`: mutating the process
                // environment from this thread races with libc getenv() in
                // every other thread (POSIX setenv is not thread-safe).
                #[cfg(not(target_os = "android"))]
                let _mic_out = {
                    #[cfg(target_os = "linux")]
                    {
                        if mode == PhoneRxRoutingMode::VirtualMicForCalls {
                            if linux_output_active {
                                None
                            } else {
                                crate::ClientPreviewSink::spawn_preferred_with_cb_frames(
                                    Arc::clone(&mic_fifo_thread),
                                    Some("AudioShare_Phone_Mic_Sink"),
                                    Some(Arc::clone(&observed_cb_frames)),
                                )
                                .ok()
                            }
                        } else {
                            crate::ClientPreviewSink::spawn_preferred_with_cb_frames(
                                Arc::clone(&mic_fifo_thread),
                                None,
                                Some(Arc::clone(&observed_cb_frames)),
                            )
                            .ok()
                        }
                    }
                    #[cfg(target_os = "windows")]
                    {
                        if mode == PhoneRxRoutingMode::VirtualMicForCalls {
                            crate::ClientPreviewSink::spawn_preferred_with_cb_frames(
                                Arc::clone(&mic_fifo_thread),
                                Some("CABLE Input"),
                                Some(Arc::clone(&observed_cb_frames)),
                            )
                            .ok()
                        } else {
                            crate::ClientPreviewSink::spawn_preferred_with_cb_frames(
                                Arc::clone(&mic_fifo_thread),
                                None,
                                Some(Arc::clone(&observed_cb_frames)),
                            )
                            .ok()
                        }
                    }
                    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
                    {
                        crate::ClientPreviewSink::spawn_preferred_with_cb_frames(
                            Arc::clone(&mic_fifo_thread),
                            None,
                            Some(Arc::clone(&observed_cb_frames)),
                        )
                        .ok()
                    }
                };

                #[cfg(not(target_os = "android"))]
                let pull_sink_active = _mic_out.is_some();
                #[cfg(target_os = "android")]
                let pull_sink_active = false;

                let mut recv_buf = [0u8; MAX_SAFE_UDP_PAYLOAD];
                let mut initialized = false;
                let mut prebuffer_ready = false;
                let mut dtx_silent = false;
                let mut next_seq: u16 = 0;
                let mut latest_seq: u16 = 0;
                let mut next_tick = Instant::now() + Duration::from_micros(2500);
                let mut resampler = ClockDriftResampler::new();
                let mut opus_decoder: Option<OpusStreamDecoder> = None;
                // Raw Opus payloads are buffered by sequence and decoded at
                // playhead pop time, mirroring ClientEngine, so Wi-Fi
                // reordering cannot desynchronise the stateful CELT decoder.
                let mut opus_ring = vec![OpusPacketSlot::EMPTY; RING_SLOTS];
                let mut opus_stream_active = false;
                // Peak meter falls back to zero once no audio has arrived for
                // a quarter second (DTX, disconnect, or stalled sender).
                let mut last_packet_arrival = Instant::now();
                let mut resampled_out = [0.0f32; SUBFRAME_STEREO_SAMPLES + 16];
                // Party Host feedback: remember where audio came from, the
                // group delay advertised in its stream_id, and the per-window
                // loss counters reported back every 500 ms.
                let mut last_sender_addr: Option<SocketAddr> = None;
                let mut max_group_delay_ms: u16 = 0;
                let mut last_feedback_sent = Instant::now();
                // Seed the feedback sequence from wall-clock 250 ms epoch ticks
                // so restarting the PC PhoneMicReceiver on static port 65532
                // never replays a Party Host's old-session sequence: ticks are
                // >2x the 500 ms feedback send rate, so every restart starts
                // strictly ahead of the host's replay high-water mark (wraps
                // every ~34 years).
                let wall_clock_ticks = (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    / 250) as u32;
                let mut feedback_seq: u32 = wall_clock_ticks.max(1);
                let mut fb_primary_frames: u32 = 0;
                let mut fb_recovered_or_lost: u32 = 0;

                while run_clone.load(Ordering::Relaxed) {
                    // PIN edits from the UI land between batches; snapshot once
                    // per 2.5 ms tick instead of locking on every datagram.
                    let key_for_batch = *session_key_thread
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner());
                    // Size the decoupler target to cover at least one full OS
                    // callback period plus 3 subframes (7.5 ms) of network
                    // jitter margin. The sink reports the backend's real
                    // callback size, so a 1024/2048-frame desktop callback
                    // automatically gets a deeper cushion than 5 ms AAudio.
                    let cb_frames = observed_cb_frames.load(Ordering::Relaxed);
                    let base_target_subframes = mic_fifo_target_subframes(cb_frames);
                    let local_delay_ms =
                        pc_receiver_effective_delay_ms(mode, base_target_subframes);
                    // PC speakers phase-align with the slowest peer phone;
                    // the call virtual mic keeps its fixed low-latency target.
                    let target_subframes = aligned_pc_receiver_target_subframes(
                        mode,
                        base_target_subframes,
                        max_group_delay_ms,
                        local_delay_ms,
                    );
                    let target_depth = target_subframes.ceil() as i16;
                    // BT Party group sync may inflate target_subframes up to 64,
                    // but only the base cushion belongs in the sequencer. The
                    // extra group-alignment depth is injected as a one-shot
                    // FIFO pad once prebuffer completes, mirroring ClientEngine.
                    let base_depth = base_target_subframes.ceil() as i16;
                    // In pull-sink mode the FIFO oscillates by one callback
                    // burst below `target_subframes`; center the clock-loop
                    // setpoint at the sawtooth midpoint so both +ppm and -ppm
                    // drift track without bias.
                    let cb_subframes = cb_frames as f32 / SUBFRAME_FRAMES as f32;
                    let clock_setpoint = if pull_sink_active {
                        (target_subframes - 0.5 * cb_subframes).max(1.0)
                    } else {
                        target_subframes
                    };
                    let fifo_watermark = resampler
                        .subframes_to_samples(target_subframes.ceil() as usize)
                        .min(mic_fifo_thread.capacity_samples());

                    while let Ok((len, src_addr)) = socket.recv_from(&mut recv_buf) {
                        if let Ok(parsed) = ParsedPacket::parse_and_decrypt(
                            &mut recv_buf[..len],
                            key_for_batch.as_ref(),
                        ) {
                            last_sender_addr = Some(src_addr);
                            max_group_delay_ms = (parsed.header.stream_id as u16) * 2;
                            pkts_clone.fetch_add(1, Ordering::Relaxed);
                            last_packet_arrival = Instant::now();
                            let base_seq = parsed.header.sequence;
                            if !initialized {
                                next_seq = base_seq;
                                latest_seq = base_seq;
                                initialized = true;
                            }

                            // DTX heartbeat: the host stopped sending audio
                            // but kept the NAT mapping and clock alive. Drop
                            // every held slot and park the playhead here, so
                            // the next real packet starts a fresh cushion
                            // instead of replaying stale concealment.
                            if parsed.header.mode == CodecMode::SilenceHeartbeat {
                                sequencer.reset();
                                opus_decoder = None;
                                for slot in &mut opus_ring {
                                    slot.seq = None;
                                }
                                opus_stream_active = false;
                                peak_clone.store(0, Ordering::Relaxed);
                                next_seq = base_seq;
                                latest_seq = base_seq;
                                prebuffer_ready = false;
                                dtx_silent = true;
                                continue;
                            }

                            // The phone transmitter can restart (or the phone
                            // can reconnect) with a new sequence base. Rebase
                            // immediately instead of waiting for the playhead
                            // to walk through hundreds of empty subframes.
                            let seq_delta = base_seq.wrapping_sub(next_seq) as i16;
                            // A BT peer with >130 ms group delay inflates the
                            // sequencer target to ~64 subframes. A fixed 48
                            // resync threshold would wipe the sequencer as soon
                            // as packet 49 arrives, while prebuffer is still
                            // waiting on base_depth -> permanent silence. Scale
                            // the threshold with the live FIFO depth, capped to
                            // the ring capacity so a stalled stream still
                            // rebases rather than drifting into a wrap.
                            let fifo_now_sf = resampler
                                .samples_to_subframes(mic_fifo_thread.available_samples())
                                as i16;
                            let resync_ahead = 48i16
                                .max(fifo_now_sf + 16)
                                .min((audioshare_protocol::sequencer::RING_SLOTS as i16) - 4);
                            if dtx_silent || seq_delta > resync_ahead || seq_delta < -32 {
                                dtx_silent = false;
                                sequencer.reset();
                                opus_decoder = None;
                                for slot in &mut opus_ring {
                                    slot.seq = None;
                                }
                                next_seq = base_seq;
                                latest_seq = base_seq;
                                prebuffer_ready = false;
                            }

                            // Opus carries its own lookahead/PLC state; an
                            // ADPCM FEC backup is phase-shifted by one
                            // subframe, so only heal VBR/lossless streams.
                            // Four-subframe framing sends 248-byte paired
                            // fields `[N-1,N-3]` / `[N-2,N-4]`; the legacy
                            // 124-byte layout heals just N-1/N-2.
                            if !primary_bundle_is_opus(parsed.primary_payload) {
                                heal_packet_fec(
                                    base_seq,
                                    next_seq,
                                    parsed.fec_n1_payload,
                                    parsed.fec_n2_payload,
                                    &sequencer,
                                );
                            }

                            // Decode every bundled subframe, not just index 0.
                            // Party Mode / PhoneAudioCaster sends two
                            // subframes per datagram; reading only the first
                            // one discarded 50% of the audio.
                            let bundle_is_opus = primary_bundle_is_opus(parsed.primary_payload);
                            opus_stream_active = bundle_is_opus;
                            let mut opus_latest = latest_seq;
                            decode_primary_bundle(
                                parsed.primary_payload,
                                parsed.header.subframe_count,
                                base_seq,
                                Some(&mut opus_decoder),
                                |missed_seq, plc_pcm| {
                                    // Opus PLC is phase-aligned with the
                                    // lookahead, so it can fill an empty slot
                                    // directly. A late primary copy still wins
                                    // because it arrives as a normal push.
                                    if (missed_seq.wrapping_sub(next_seq) as i16) >= 0
                                        && sequencer.is_slot_missing(missed_seq)
                                    {
                                        sequencer.try_heal_with_fec(missed_seq, plc_pcm);
                                    }
                                },
                                |sub_idx, pcm_f32| {
                                    note_pcm_level(pcm_f32, &peak_clone, &chirp_clone);
                                    let s = base_seq.wrapping_add(sub_idx as u16);
                                    if (s.wrapping_sub(next_seq) as i16) >= 0 {
                                        sequencer.push_primary(s, pcm_f32);
                                    }
                                    if (s.wrapping_sub(latest_seq) as i16) > 0 {
                                        latest_seq = s;
                                    }
                                },
                                |s, payload| {
                                    // Buffer raw Opus payloads by sequence for
                                    // strict in-order decode at the playhead.
                                    // A late packet must never rewind the CELT
                                    // predictor, so drop it here.
                                    if (s.wrapping_sub(next_seq) as i16) >= 0 {
                                        let slot = &mut opus_ring[(s as usize) & (RING_SLOTS - 1)];
                                        if slot.seq != Some(s) {
                                            let len = payload.len().min(MAX_OPUS_SUBFRAME_BYTES);
                                            slot.data[..len].copy_from_slice(&payload[..len]);
                                            slot.len = len as u16;
                                            slot.seq = Some(s);
                                            // A primary always wins over any RED
                                            // heal that previously occupied the slot.
                                            slot.from_red = false;
                                        }
                                        if (s.wrapping_sub(opus_latest) as i16) > 0 {
                                            opus_latest = s;
                                        }
                                    }
                                    // Opus is always handled here (buffered or
                                    // dropped), never decoded on arrival.
                                    true
                                },
                            );
                            if (opus_latest.wrapping_sub(latest_seq) as i16) > 0 {
                                latest_seq = opus_latest;
                            }

                            // Opus RED: the phone transmitter piggybacks the
                            // exact prior packets ([P-1]/[P-2]) in the FEC
                            // fields while OpusRld is active. Now that this
                            // packet's primaries are buffered, heal any still
                            // empty slots at or ahead of the playhead from
                            // both blobs. A late primary still wins, and the
                            // healed slots are flagged so they are counted as
                            // FEC-healed (never primary) at pop time.
                            if parsed.header.mode == CodecMode::OpusRld {
                                for red_blob in
                                    [parsed.fec_n1_payload, parsed.fec_n2_payload]
                                {
                                    heal_opus_red_slots(
                                        red_blob,
                                        next_seq,
                                        &sequencer,
                                        &mut opus_ring,
                                    );
                                }
                            }

                            // Keep only the planned cushion (one callback plus
                            // jitter margin) if a burst pushed the backlog far
                            // past it, then start playback once that cushion is
                            // actually buffered.
                            let buf_depth = buffered_subframes(latest_seq, next_seq);
                            // Only the base cushion is held in the sequencer;
                            // the BT group-sync extra is padded into the FIFO
                            // after prebuffer so the seq_delta resync window is
                            // never forced past the fixed 48-subframe guard.
                            let max_depth = base_depth + 8;
                            if buf_depth > max_depth {
                                next_seq = latest_seq
                                    .wrapping_add(1)
                                    .wrapping_sub(base_depth as u16);
                                sequencer.arm_declick();
                            }
                            if !prebuffer_ready && buf_depth >= base_depth {
                                prebuffer_ready = true;
                                sequencer.arm_fade_in();
                                let group_extra = (target_depth - base_depth).max(0) as usize;
                                if group_extra > 0 {
                                    let mut rem = resampler.subframes_to_samples(group_extra);
                                    let mut pad = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                                    let mut rng = 0x7F4A_7C15u32;
                                    while rem > 0 {
                                        let step = rem.min(pad.len()) & !1;
                                        if step == 0 {
                                            break;
                                        }
                                        audioshare_protocol::dsp::inject_bluetooth_keepalive_dither(
                                            &mut pad[..step], &mut rng,
                                        );
                                        let pushed = mic_fifo_thread.push_samples(&pad[..step]);
                                        rem = rem.saturating_sub(pushed);
                                        if pushed < step {
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Report PC speaker delay & link loss back to the Party Host
                    // phone every 500 ms so PC speakers stay phase-locked with
                    // peer phones and trigger adaptive bitrate/10 ms framing if
                    // Wi-Fi drops packets. The call virtual mic never sends it.
                    if mode == PhoneRxRoutingMode::PlayOnPcSpeakers
                        && initialized
                        && !dtx_silent
                        && last_feedback_sent.elapsed() >= Duration::from_millis(500)
                    {
                        last_feedback_sent = Instant::now();
                        if let Some(sender) = last_sender_addr {
                            let fec_permille =
                                fb_fec_rate_permille(fb_primary_frames, fb_recovered_or_lost);
                            fb_primary_frames = 0;
                            fb_recovered_or_lost = 0;
                            feedback_seq = feedback_seq.wrapping_add(1);
                            let mut fb_bytes = [0u8; CONTROL_FEEDBACK_LEN];
                            build_pc_feedback(
                                key_for_batch.as_ref(),
                                feedback_seq,
                                fec_permille,
                                local_delay_ms,
                                &mut fb_bytes,
                            );
                            let _ = socket.send_to(&fb_bytes, sender);
                        }
                    }

                    // A silent sender (or a disconnected phone) must not leave
                    // the peak meter frozen at the last real audio level.
                    if last_packet_arrival.elapsed() >= Duration::from_millis(250) {
                        peak_clone.store(0, Ordering::Relaxed);
                    }

                    let now = Instant::now();
                    // Do not let `next_tick` accumulate while the receiver is
                    // still prebuffering. Otherwise the first ready loop can
                    // walk every missed tick at CPU speed and fill the
                    // decoupler with PLC.
                    if !initialized || !prebuffer_ready {
                        next_tick = now + Duration::from_micros(2500);
                    }

                    if initialized && prebuffer_ready && pull_sink_active {
                        // When a hardware-clocked CPAL sink is pulling bursts
                        // from `mic_fifo_thread`, immediately transfer ready
                        // subframes so the decoupler holds the full
                        // `fifo_watermark` cushion before the next callback.
                        // Opus frames live in the decode-at-pop ring, not in
                        // the sequencer, so check both sources here.
                        while mic_fifo_thread.available_samples() < fifo_watermark
                            && (!sequencer.is_slot_missing(next_seq)
                                || opus_ring[(next_seq as usize) & (RING_SLOTS - 1)].seq
                                    == Some(next_seq))
                        {
                            let st = push_one_subframe(
                                &mut next_seq,
                                latest_seq,
                                mode,
                                &mut sequencer,
                                &mut mic_gate,
                                &mut limiter,
                                &mut resampler,
                                &mut resampled_out,
                                &mic_fifo_thread,
                                &mut opus_decoder,
                                &mut opus_ring,
                                &mut opus_stream_active,
                                &peak_clone,
                                &chirp_clone,
                            );
                            if st == STATUS_PRIMARY_LOSSLESS {
                                fb_primary_frames += 1;
                            } else {
                                fb_recovered_or_lost += 1;
                            }
                            let observed = buffered_subframes(latest_seq, next_seq) as f32
                                + resampler
                                    .samples_to_subframes(mic_fifo_thread.available_samples());
                            resampler.update_clock_loop(observed, clock_setpoint);
                            next_tick = now + Duration::from_micros(2500);
                        }
                    }

                    // A long stall (thread suspend, blocked sink, GC pause)
                    // must not release a catch-up burst of PLC subframes at
                    // CPU speed. Re-anchor instead of banking stale ticks.
                    if now.duration_since(next_tick) > Duration::from_millis(25) {
                        next_tick = now;
                    }

                    while now >= next_tick {
                        next_tick += Duration::from_micros(2500);
                        if initialized && prebuffer_ready {
                            if mic_fifo_thread.available_samples() >= fifo_watermark {
                                // Re-anchor rather than banking every tick the
                                // sink was full; a later dip must not release a
                                // catch-up burst.
                                next_tick = now + Duration::from_micros(2500);
                                break;
                            }
                            let play_slot_missing = sequencer.is_slot_missing(next_seq)
                                && opus_ring[(next_seq as usize) & (RING_SLOTS - 1)].seq
                                    != Some(next_seq);
                            if should_hold_mic_live_edge(
                                pull_sink_active,
                                play_slot_missing,
                                latest_seq,
                                next_seq,
                                mic_fifo_thread.available_samples(),
                            ) {
                                next_tick = now + Duration::from_micros(2500);
                                break;
                            }
                            let st = push_one_subframe(
                                &mut next_seq,
                                latest_seq,
                                mode,
                                &mut sequencer,
                                &mut mic_gate,
                                &mut limiter,
                                &mut resampler,
                                &mut resampled_out,
                                &mic_fifo_thread,
                                &mut opus_decoder,
                                &mut opus_ring,
                                &mut opus_stream_active,
                                &peak_clone,
                                &chirp_clone,
                            );
                            if st == STATUS_PRIMARY_LOSSLESS {
                                fb_primary_frames += 1;
                            } else {
                                fb_recovered_or_lost += 1;
                            }
                            let observed = buffered_subframes(latest_seq, next_seq) as f32
                                + resampler
                                    .samples_to_subframes(mic_fifo_thread.available_samples());
                            resampler.update_clock_loop(observed, clock_setpoint);
                        }
                    }

                    std::thread::sleep(Duration::from_micros(600));
                }
            })
            .map_err(|e| format!("Failed to spawn PhoneMicReceiver thread: {}", e))?;

        Ok(Self {
            running,
            session_key,
            mic_packets_received,
            mic_peak_level_pct,
            last_chirp_rx_timestamp_us,
            #[cfg(target_os = "linux")]
            pactl_mic_module_id,
            #[cfg(target_os = "linux")]
            pactl_mic_source_module_id,
            #[cfg(target_os = "linux")]
            linux_output,
            handle: Some(handle),
        })
    }

    /// Update the PIN used to decrypt phone-mic datagrams. The RX thread
    /// snapshots this value every tick, so toggling encryption takes effect
    /// within one subframe without restarting the receiver.
    pub fn set_encryption_pin(&self, pin: Option<&str>) {
        *self
            .session_key
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = session_key_from_pin(pin);
    }

    /// Install an already-derived session key (e.g. the single key shared by
    /// all engines from the GUI). The RX thread snapshots it once per tick, so
    /// this takes effect within one subframe without restarting the receiver.
    pub fn set_session_key(&self, key: Option<SessionKey>) {
        *self
            .session_key
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = key;
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        #[cfg(target_os = "linux")]
        {
            // Stop pacat first, then remove the remap source and sink.
            drop(self.linux_output.take());
            if let Some(id) = self.pactl_mic_source_module_id.take() {
                let _ = Command::new("pactl").args(["unload-module", &id]).output();
            }
            if let Some(id) = self.pactl_mic_module_id.take() {
                let _ = Command::new("pactl").args(["unload-module", &id]).output();
            }
        }
    }
}

impl Drop for PhoneMicReceiver {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audioshare_protocol::lpc_lossless::{
        encode_lossless_pcm24_subframe, encode_lossless_subframe, encode_vbr_subframe,
        MAX_LOSSLESS_PCM24_BYTES, MAX_LOSSLESS_SUBFRAME_BYTES, TAG_VBR_BFP,
    };
    use audioshare_protocol::opus_codec::{OpusStreamEncoder, MAX_OPUS_SUBFRAME_BYTES};
    use audioshare_protocol::wire::{pack_datagram_with_key, WireHeader};
    use audioshare_protocol::SAMPLE_RATE;

    #[test]
    fn test_pull_sink_holds_live_edge_until_real_audio_is_nearly_drained() {
        // No newer packet has arrived; the slot at `next_seq` is missing.
        assert!(should_hold_mic_live_edge(
            true,
            true,
            4,
            5,
            SUBFRAME_STEREO_SAMPLES
        ));
        // One sample short of a full subframe: conceal instead of starving.
        assert!(!should_hold_mic_live_edge(
            true,
            true,
            4,
            5,
            SUBFRAME_STEREO_SAMPLES - 1
        ));
        // Without a pull sink the timer is the only producer, so keep ticking.
        assert!(!should_hold_mic_live_edge(
            false,
            true,
            4,
            5,
            SUBFRAME_STEREO_SAMPLES
        ));
        // A newer packet is already buffered; the slot is recoverable through
        // the normal refill loop rather than being at the live edge.
        assert!(!should_hold_mic_live_edge(
            true,
            true,
            5,
            5,
            SUBFRAME_STEREO_SAMPLES
        ));
        // The slot is present and can simply be popped.
        assert!(!should_hold_mic_live_edge(
            true,
            false,
            4,
            5,
            SUBFRAME_STEREO_SAMPLES
        ));
    }

    #[test]
    fn test_session_key_from_pin_trims_and_ignores_blank_pins() {
        assert!(session_key_from_pin(None).is_none());
        assert!(session_key_from_pin(Some("")).is_none());
        assert!(session_key_from_pin(Some("   ")).is_none());
        assert!(session_key_from_pin(Some(" 4829 ")).is_some());
        assert!(session_key_from_pin(Some(" 4829 ")) == Some(SessionKey::from_pin("4829")));
    }

    #[test]
    fn test_decode_primary_bundle_returns_every_bundled_subframe() {
        let mut first = [0i16; SUBFRAME_STEREO_SAMPLES];
        let mut second = [0i16; SUBFRAME_STEREO_SAMPLES];
        for i in 0..SUBFRAME_STEREO_SAMPLES {
            first[i] = ((i as i32 * 97) % 30_000 - 15_000) as i16;
            second[i] = ((i as i32 * 53) % 20_000 - 10_000) as i16;
        }

        let mut enc1 = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
        let mut enc2 = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
        let len1 = encode_lossless_subframe(&first, &mut enc1);
        let len2 = encode_lossless_subframe(&second, &mut enc2);

        let mut bundle = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES * 2 + 4];
        let mut cursor = 0usize;
        for (enc, len) in [(&enc1, len1), (&enc2, len2)] {
            bundle[cursor..cursor + 2].copy_from_slice(&(len as u16).to_be_bytes());
            cursor += 2;
            bundle[cursor..cursor + len].copy_from_slice(&enc[..len]);
            cursor += len;
        }

        let mut decoded = Vec::new();
        let count = decode_primary_bundle(
            &bundle[..cursor],
            2,
            0,
            None,
            |_, _| {},
            |idx, pcm| {
                let mut samples = [0i16; SUBFRAME_STEREO_SAMPLES];
                for (dst, &s) in samples.iter_mut().zip(pcm.iter()) {
                    *dst = (s * 32768.0).round() as i16;
                }
                decoded.push((idx, samples));
            },
            |_, _| false,
        );

        assert_eq!(count, 2);
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].0, 0);
        assert_eq!(decoded[1].0, 1);
        assert_eq!(decoded[0].1, first);
        assert_eq!(decoded[1].1, second);
    }

    #[test]
    fn test_pin_encrypted_phone_mic_datagram_decrypts_and_decodes() {
        let key = SessionKey::from_pin("4829");
        let wrong_key = SessionKey::from_pin("0000");

        let pcm = [1234i16; SUBFRAME_STEREO_SAMPLES];
        let mut enc = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
        let enc_len = encode_lossless_subframe(&pcm, &mut enc);
        let mut bundle = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES + 2];
        bundle[0..2].copy_from_slice(&(enc_len as u16).to_be_bytes());
        bundle[2..2 + enc_len].copy_from_slice(&enc[..enc_len]);

        let header = WireHeader {
            mode: CodecMode::StudioLosslessLpc,
            subframe_count: 1,
            encrypted: true,
            sequence: 7,
            tx_timestamp_us: 42_000,
            primary_len: 0,
            fec_n1_len: 0,
            fec_n2_len: 0,
            host_volume: 100,
            stream_id: 0,
            header_crc16: 0,
        };
        let mut datagram = [0u8; MAX_SAFE_UDP_PAYLOAD];
        let len = pack_datagram_with_key(
            header,
            &bundle[..2 + enc_len],
            &[],
            &[],
            Some(&key),
            &mut datagram,
        )
        .unwrap();

        let mut forged = datagram;
        assert!(
            ParsedPacket::parse_and_decrypt(&mut forged[..len], Some(&wrong_key)).is_err(),
            "wrong PIN must not decrypt phone-mic audio"
        );

        let parsed =
            ParsedPacket::parse_and_decrypt(&mut datagram[..len], Some(&key)).expect("decrypt");
        let mut decoded = 0usize;
        decode_primary_bundle(
            parsed.primary_payload,
            1,
            7,
            None,
            |_, _| {},
            |_, _| decoded += 1,
            |_, _| false,
        );
        assert_eq!(decoded, 1);
    }

    #[test]
    fn test_decode_primary_bundle_rejects_truncated_length_prefix() {
        let mut short = [0u8; 4];
        short[0..2].copy_from_slice(&100u16.to_be_bytes());
        let count = decode_primary_bundle(
            &short,
            1,
            0,
            None,
            |_, _| {},
            |_, _| panic!("must not decode"),
            |_, _| false,
        );
        assert_eq!(count, 0);
    }

    #[test]
    fn test_primary_bundle_is_opus_reads_first_length_prefixed_tag() {
        let mut opus = [0u8; 3];
        opus[0..2].copy_from_slice(&1u16.to_be_bytes());
        opus[2] = TAG_OPUS_RLD;
        assert!(primary_bundle_is_opus(&opus));

        let vbr = [0u8, 1, TAG_VBR_BFP];
        assert!(!primary_bundle_is_opus(&vbr));
        assert!(!primary_bundle_is_opus(&[0, 1]));
        assert!(!primary_bundle_is_opus(&[]));
    }

    #[test]
    fn test_decode_primary_bundle_resets_opus_decoder_on_non_opus() {
        let mut pcm_i16 = [0i16; SUBFRAME_STEREO_SAMPLES];
        for i in 0..SUBFRAME_FRAMES {
            let t = i as f32 / SAMPLE_RATE as f32;
            pcm_i16[i * 2] = ((t * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
            pcm_i16[i * 2 + 1] = ((t * 554.37 * std::f32::consts::TAU).sin() * 8000.0) as i16;
        }

        let mut enc = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
        let len = encode_vbr_subframe(&pcm_i16, 256, &mut enc);
        let mut bundle = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES + 2];
        bundle[0..2].copy_from_slice(&(len as u16).to_be_bytes());
        bundle[2..2 + len].copy_from_slice(&enc[..len]);

        let mut decoder = Some(OpusStreamDecoder::new().expect("create Opus decoder"));
        let count = decode_primary_bundle(
            &bundle[..2 + len],
            1,
            0,
            Some(&mut decoder),
            |_, _| {},
            |_, _| {},
            |_, _| false,
        );
        assert_eq!(count, 1, "VBR payload must still decode");
        assert!(
            decoder.is_none(),
            "a non-Opus payload must drop stale Opus state"
        );
    }

    #[test]
    fn test_decode_primary_bundle_handles_tagged_vbr() {
        let mut pcm_i16 = [0i16; SUBFRAME_STEREO_SAMPLES];
        for i in 0..(SUBFRAME_STEREO_SAMPLES / 2) {
            let t = i as f32 / 48_000.0;
            pcm_i16[i * 2] = ((t * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
            pcm_i16[i * 2 + 1] = ((t * 554.37 * std::f32::consts::TAU).sin() * 8000.0) as i16;
        }

        let mut enc = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
        let len = encode_vbr_subframe(&pcm_i16, 256, &mut enc);
        assert_eq!(enc[0], TAG_VBR_BFP);

        let mut bundle = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES + 2];
        bundle[0..2].copy_from_slice(&(len as u16).to_be_bytes());
        bundle[2..2 + len].copy_from_slice(&enc[..len]);

        let mut peak = 0.0f32;
        let count = decode_primary_bundle(
            &bundle[..2 + len],
            1,
            0,
            None,
            |_, _| {},
            |idx, pcm| {
                assert_eq!(idx, 0);
                for &v in pcm {
                    peak = peak.max(v.abs());
                }
            },
            |_, _| false,
        );
        assert_eq!(count, 1, "tagged VBR payload must decode");
        assert!(peak > 0.05, "decoded music must not be silence");
    }

    #[test]
    fn test_decode_primary_bundle_handles_24bit_payload() {
        let mut pcm_f32 = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        for (i, sample) in pcm_f32.iter_mut().enumerate() {
            let t = i as f32 / SAMPLE_RATE as f32;
            // The tiny linear term guarantees a non-zero 8-bit LSB plane so
            // the encoder emits the true 24-bit extension instead of falling
            // back to the 16-bit payload.
            *sample = (t * 880.0 * std::f32::consts::TAU).sin() * 0.4 + (i as f32) * 0.000_000_1;
        }

        let mut enc = [0u8; MAX_LOSSLESS_PCM24_BYTES];
        let len = encode_lossless_pcm24_subframe(&pcm_f32, &mut enc);
        assert!(
            len > MAX_LOSSLESS_SUBFRAME_BYTES,
            "payload must carry the 24-bit LSB plane, len={len}"
        );

        let mut bundle = [0u8; MAX_LOSSLESS_PCM24_BYTES + 2];
        bundle[0..2].copy_from_slice(&(len as u16).to_be_bytes());
        bundle[2..2 + len].copy_from_slice(&enc[..len]);

        let mut decoded = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        let count = decode_primary_bundle(
            &bundle[..2 + len],
            1,
            42,
            None,
            |_, _| {},
            |idx, pcm| {
                assert_eq!(idx, 0);
                decoded = *pcm;
            },
            |_, _| false,
        );
        assert_eq!(count, 1, "24-bit payload must decode");
        for (i, (&got, &source)) in decoded.iter().zip(pcm_f32.iter()).enumerate() {
            let q = (source.clamp(-1.0, 1.0) * 8_388_608.0)
                .round()
                .clamp(-8_388_608.0, 8_388_607.0);
            let expected = q / 8_388_608.0;
            assert!(
                (got - expected).abs() < 1e-6,
                "sample {i}: decoded {got} != expected {expected}"
            );
        }
    }

    #[test]
    fn test_decode_primary_bundle_handles_opus_payload() {
        fn tone(frame_index: usize) -> [i16; SUBFRAME_STEREO_SAMPLES] {
            let mut pcm = [0i16; SUBFRAME_STEREO_SAMPLES];
            for i in 0..SUBFRAME_FRAMES {
                let t = ((frame_index * SUBFRAME_FRAMES + i) as f32) / (SAMPLE_RATE as f32);
                let l = (t * 440.0 * std::f32::consts::TAU).sin() * 0.25;
                let r = (t * 554.37 * std::f32::consts::TAU).sin() * 0.25;
                pcm[i * 2] = (l * 32767.0) as i16;
                pcm[i * 2 + 1] = (r * 32767.0) as i16;
            }
            pcm
        }

        let mut encoder = OpusStreamEncoder::new(128).expect("create Opus encoder");
        let mut payloads = [[0u8; MAX_OPUS_SUBFRAME_BYTES]; 2];
        let mut lens = [0usize; 2];
        for (frame_index, (payload, len)) in payloads.iter_mut().zip(lens.iter_mut()).enumerate() {
            *len = encoder
                .encode_subframe(&tone(frame_index), 128, payload)
                .expect("encode Opus subframe");
            assert_eq!(payload[0], TAG_OPUS_RLD);
        }

        let mut bundle = [0u8; (MAX_OPUS_SUBFRAME_BYTES + 2) * 2];
        let mut cursor = 0usize;
        for (payload, &len) in payloads.iter().zip(lens.iter()) {
            bundle[cursor..cursor + 2].copy_from_slice(&(len as u16).to_be_bytes());
            cursor += 2;
            bundle[cursor..cursor + len].copy_from_slice(&payload[..len]);
            cursor += len;
        }

        let mut decoder = Some(OpusStreamDecoder::new().expect("create Opus decoder"));
        let mut frames = Vec::new();
        let count = decode_primary_bundle(
            &bundle[..cursor],
            2,
            100,
            Some(&mut decoder),
            |_, _| {},
            |idx, pcm| frames.push((idx, *pcm)),
            |_, _| false,
        );
        assert_eq!(
            count, 2,
            "both Opus subframes must decode through the bundle"
        );
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].0, 0);
        assert_eq!(frames[1].0, 1);
        // Restricted-low-delay Opus has one 2.5 ms frame of lookahead, so the
        // second decoded frame reconstructs the first tone instead of silence.
        let peak = frames[1].1.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(
            peak > 0.02,
            "second Opus frame must carry real audio, peak={peak}"
        );
    }

    #[test]
    fn test_decode_primary_bundle_delivers_opus_plc_for_missing_slot() {
        fn tone(frame_index: usize) -> [i16; SUBFRAME_STEREO_SAMPLES] {
            let mut pcm = [0i16; SUBFRAME_STEREO_SAMPLES];
            for i in 0..SUBFRAME_FRAMES {
                let t = ((frame_index * SUBFRAME_FRAMES + i) as f32) / (SAMPLE_RATE as f32);
                let l = (t * 440.0 * std::f32::consts::TAU).sin() * 0.25;
                let r = (t * 554.37 * std::f32::consts::TAU).sin() * 0.25;
                pcm[i * 2] = (l * 32767.0) as i16;
                pcm[i * 2 + 1] = (r * 32767.0) as i16;
            }
            pcm
        }

        // Materialize frames 99, 100 and 101 so the decoder has real history
        // and a real future frame around the lost slot.
        let mut encoder = OpusStreamEncoder::new(128).expect("create Opus encoder");
        let mut payloads = [[0u8; MAX_OPUS_SUBFRAME_BYTES]; 3];
        let mut lens = [0usize; 3];
        for frame_index in 0..3 {
            lens[frame_index] = encoder
                .encode_subframe(&tone(frame_index), 128, &mut payloads[frame_index])
                .expect("encode Opus subframe");
        }

        let mut decoder = Some(OpusStreamDecoder::new().expect("create Opus decoder"));
        let mut primed = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        assert!(decoder.as_mut().unwrap().decode_subframe_at_seq(
            99,
            &payloads[0][..lens[0]],
            &mut primed
        ));

        // Packet 100 was lost; packet 101 arrives with the next frame.
        let mut bundle = [0u8; MAX_OPUS_SUBFRAME_BYTES + 2];
        bundle[0..2].copy_from_slice(&(lens[2] as u16).to_be_bytes());
        bundle[2..2 + lens[2]].copy_from_slice(&payloads[2][..lens[2]]);

        let mut plc = Vec::new();
        let mut decoded = Vec::new();
        let count = decode_primary_bundle(
            &bundle[..2 + lens[2]],
            1,
            101,
            Some(&mut decoder),
            |missed_seq, pcm| plc.push((missed_seq, *pcm)),
            |idx, pcm| decoded.push((idx, *pcm)),
            |_, _| false,
        );

        assert_eq!(count, 1);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].0, 0);
        assert_eq!(plc.len(), 1, "one lost slot must produce one PLC frame");
        assert_eq!(plc[0].0, 100, "PLC must land on the missing sequence");
        let plc_peak = plc[0].1.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(plc[0].1.iter().all(|sample| sample.is_finite()));
        assert!(
            plc_peak > 0.001,
            "PLC frame must not be silent, peak={plc_peak}"
        );
    }

    #[test]
    fn test_mic_clock_resampler_depth_conversion() {
        let resampler = ClockDriftResampler::new();
        let one = resampler.samples_to_subframes(SUBFRAME_STEREO_SAMPLES);
        assert!((one - 1.0).abs() < 1e-6);
        let target_subframes = mic_fifo_target_subframes(240);
        assert!((target_subframes - MIC_FIFO_MIN_TARGET_SUBFRAMES).abs() < 1e-6);
        let target_samples = SUBFRAME_STEREO_SAMPLES * target_subframes as usize;
        let target = resampler.samples_to_subframes(target_samples);
        assert!((target - target_subframes).abs() < 1e-6);
        // A 10 ms default desktop callback drains four subframes in one pull;
        // the cushion must cover at least that plus the poll interval.
        assert!(
            target_subframes >= 6.0,
            "mic FIFO target must absorb a 10-15 ms callback burst"
        );
    }

    #[test]
    fn test_mic_fifo_target_scales_with_callback_size() {
        // 5 ms low-latency callback: 2 subframes + 3 margin = 5, floored at 6.
        assert!((mic_fifo_target_subframes(240) - 6.0).abs() < 1e-6);
        // 1024 frames = 8.53 subframes -> 11.53 subframes of cushion.
        let expected_1024 = 1024.0 / SUBFRAME_FRAMES as f32 + 3.0;
        assert!((mic_fifo_target_subframes(1024) - expected_1024).abs() < 1e-6);
        // 2048 frames would ask for 20.07 subframes; clamp to the 50 ms ceiling.
        assert!((mic_fifo_target_subframes(2048) - MIC_FIFO_MAX_TARGET_SUBFRAMES).abs() < 1e-6);
        assert!((mic_fifo_target_subframes(8192) - MIC_FIFO_MAX_TARGET_SUBFRAMES).abs() < 1e-6);
        // The floor still applies to tiny callback sizes and the result is
        // monotonic between 240 and 1024 frames.
        assert!((mic_fifo_target_subframes(64) - MIC_FIFO_MIN_TARGET_SUBFRAMES).abs() < 1e-6);
        assert!(mic_fifo_target_subframes(512) <= mic_fifo_target_subframes(1024));
    }

    #[test]
    fn test_phone_mic_playhead_holds_when_pc_clock_runs_ahead() {
        use audioshare_protocol::sequencer::{
            STATUS_EMPTY, STATUS_HEALED_BY_FEC, STATUS_PRIMARY_LOSSLESS,
        };

        // The PC tick found an empty slot and no newer packet: hold so the
        // phone can catch up instead of running the playhead ahead forever.
        assert!(!should_advance_mic_playhead(STATUS_EMPTY, 99, 100));
        // A newer packet is already buffered: skip the hole.
        assert!(should_advance_mic_playhead(STATUS_EMPTY, 100, 100));
        // Real and FEC audio always advance.
        assert!(should_advance_mic_playhead(
            STATUS_PRIMARY_LOSSLESS,
            50,
            100
        ));
        assert!(should_advance_mic_playhead(STATUS_HEALED_BY_FEC, 50, 100));
        // 16-bit wrap: latest 65535 with next 0 means next is one ahead.
        assert!(!should_advance_mic_playhead(STATUS_EMPTY, 65535, 0));
    }

    #[test]
    fn test_buffered_subframes_handles_wraparound() {
        assert_eq!(buffered_subframes(0, 0), 1);
        assert_eq!(buffered_subframes(2, 0), 3);
        assert_eq!(buffered_subframes(0, 1), 0);
        assert_eq!(buffered_subframes(65535, 0), 0);
        assert_eq!(buffered_subframes(0, 65535), 2);
    }

    #[test]
    fn test_post_dtx_rebase_logic() {
        // Without dtx_silent, a gap of 20 subframes (seq_delta = 20) falls
        // within the normal window (seq_delta <= 48) and does NOT rebase.
        let mut dtx_silent = false;
        let base_seq = 180u16;
        let mut next_seq = 160u16;
        let mut latest_seq = 160u16;
        let mut prebuffer_ready = true;

        let seq_delta = base_seq.wrapping_sub(next_seq) as i16;
        assert_eq!(seq_delta, 20);
        let would_rebase_without_dtx = dtx_silent || seq_delta > 48 || seq_delta < -32;
        assert!(!would_rebase_without_dtx);

        // With dtx_silent set by SilenceHeartbeat, it MUST trigger rebase
        dtx_silent = true;
        let should_rebase = dtx_silent || seq_delta > 48 || seq_delta < -32;
        assert!(should_rebase);

        if should_rebase {
            dtx_silent = false;
            next_seq = base_seq;
            latest_seq = base_seq;
            prebuffer_ready = false;
        }

        assert!(!dtx_silent);
        assert_eq!(next_seq, 180);
        assert_eq!(latest_seq, 180);
        assert!(!prebuffer_ready);
    }

    #[test]
    fn test_pc_party_speaker_group_sync_aligns_while_call_mic_stays_zero_delay() {
        // 240-frame callback -> 6-subframe floor (excess 2.5 ms) + 5 ms output
        // buffer -> 10 ms of unaligned local delay.
        let local_ms = pc_receiver_effective_delay_ms(PhoneRxRoutingMode::PlayOnPcSpeakers, 6.0);
        assert_eq!(local_ms, 10);
        assert_eq!(
            pc_receiver_effective_delay_ms(PhoneRxRoutingMode::VirtualMicForCalls, 6.0),
            0
        );
        // Delay is capped at 255 ms for a pathologically deep FIFO.
        assert_eq!(
            pc_receiver_effective_delay_ms(PhoneRxRoutingMode::PlayOnPcSpeakers, 104.0),
            255
        );

        // A Bluetooth peer reports 150 ms via stream_id*2: the PC adds
        // (150 - 10) / 2.5 = 56 subframes on top of its 6-subframe base.
        let aligned = aligned_pc_receiver_target_subframes(
            PhoneRxRoutingMode::PlayOnPcSpeakers,
            6.0,
            150,
            local_ms,
        );
        assert!(
            (aligned - 62.0).abs() < 0.01,
            "expected 62 subframes, got {aligned}"
        );

        // With no group delay advertised the base target is unchanged.
        assert_eq!(
            aligned_pc_receiver_target_subframes(
                PhoneRxRoutingMode::PlayOnPcSpeakers,
                6.0,
                0,
                local_ms,
            ),
            6.0
        );

        // Video-call virtual mic must NEVER add room-sync delay.
        let call_target = aligned_pc_receiver_target_subframes(
            PhoneRxRoutingMode::VirtualMicForCalls,
            6.0,
            150,
            0,
        );
        assert_eq!(call_target, 6.0);
        assert_eq!(
            pc_receiver_effective_delay_ms(PhoneRxRoutingMode::VirtualMicForCalls, 64.0),
            0
        );

        // Per-window loss counters feed the FEC permille field.
        assert_eq!(fb_fec_rate_permille(95, 5), 50);
        assert_eq!(fb_fec_rate_permille(0, 0), 0);
        assert_eq!(fb_fec_rate_permille(0, 1), 1000);
    }

    #[test]
    fn test_pc_party_bt_group_sync_does_not_deadlock_resync() {
        // Hardware-free regression: exercises the pure arithmetic that used to
        // lock PC speakers in permanent silence when a BT earbud peer with a
        // >130 ms group delay joined a Party. The full UDP/tick loop needs a
        // live socket and audio sink, so only the decisive clamp/prebuffer
        // behaviour is unit-tested here.
        let ring_cap = audioshare_protocol::sequencer::RING_SLOTS as i16;
        assert_eq!(ring_cap, 256);

        let base_target = mic_fifo_target_subframes(240); // 6.0 (5 ms callback floor)
        let base_depth = base_target.ceil() as i16;
        assert_eq!(base_depth, 6);
        let local_ms =
            pc_receiver_effective_delay_ms(PhoneRxRoutingMode::PlayOnPcSpeakers, base_target);
        let target_depth = aligned_pc_receiver_target_subframes(
            PhoneRxRoutingMode::PlayOnPcSpeakers,
            base_target,
            150, // BT peer advertises 150 ms via stream_id * 2
            local_ms,
        )
        .ceil() as i16;
        assert_eq!(target_depth, 62);
        assert!(
            target_depth > 48,
            "regression precondition: the stale 48-subframe guard would fire"
        );

        // Only the base cushion lives in the sequencer; the BT alignment extra
        // is injected into the FIFO after prebuffer completes.
        let group_extra = (target_depth - base_depth).max(0) as usize;
        assert_eq!(group_extra, 56);

        // Prebuffer must flip at base_depth (6), well before the old target
        // (62) and before the 48-subframe resync guard could wipe the stream.
        let mut prebuffer_ready = false;
        for buf_depth in 0..=target_depth {
            if !prebuffer_ready && buf_depth >= base_depth {
                prebuffer_ready = true;
            }
            if buf_depth == base_depth || buf_depth == 48 {
                assert!(
                    prebuffer_ready,
                    "prebuffer must be ready by buf_depth={buf_depth}"
                );
            }
        }
        assert!(prebuffer_ready);

        // Clamped resync threshold: floor 48, scaled by the live FIFO depth,
        // capped at RING_SLOTS - 4 so a stalled stream still rebases before a
        // 16-bit sequence wrap.
        let resync_ahead = |fifo_now_sf: i16| -> i16 {
            48i16.max(fifo_now_sf + 16).min(ring_cap - 4)
        };
        assert_eq!(resync_ahead(0), 48);
        assert_eq!(resync_ahead(20), 48);
        assert_eq!(resync_ahead(40), 56);
        assert_eq!(resync_ahead(60), 76);
        assert!(
            resync_ahead(60) >= target_depth,
            "a deep BT cushion must not be resynced mid-prebuffer"
        );
        assert_eq!(resync_ahead(500), ring_cap - 4); // capped to ring capacity - 4

        // At the base cushion the guard stays at the 48 floor, far above the
        // 6-packet prebuffer, so the fill phase never trips a reset.
        assert!(base_depth < resync_ahead(base_depth));
    }

    #[test]
    fn test_pc_feedback_loopback_is_signed_and_replay_protected() {
        use audioshare_protocol::wire::{feedback_is_replay, ClientFeedbackPacket};

        // "Host phone" socket receives feedback; "PC receiver" socket sends it.
        let host = UdpSocket::bind("127.0.0.1:0").expect("bind loopback host socket");
        let pc = UdpSocket::bind("127.0.0.1:0").expect("bind loopback pc socket");
        host.set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");
        let host_addr = host.local_addr().expect("host local addr");

        // A single SessionKey instance is shared by signer and verifier: the
        // per-session nonce salt is drawn once at derivation time.
        let key = SessionKey::from_pin("4829");
        let feedback_seq = 7u32;
        let hardware_delay_ms =
            pc_receiver_effective_delay_ms(PhoneRxRoutingMode::PlayOnPcSpeakers, 6.0);

        let mut fb_bytes = [0u8; CONTROL_FEEDBACK_LEN];
        build_pc_feedback(
            Some(&key),
            feedback_seq,
            50,
            hardware_delay_ms,
            &mut fb_bytes,
        );
        pc.send_to(&fb_bytes, host_addr).expect("send feedback");

        let mut buf = [0u8; 64];
        let (len, from) = host.recv_from(&mut buf).expect("receive feedback");
        assert_eq!(from, pc.local_addr().expect("pc local addr"));
        assert_eq!(len, CONTROL_FEEDBACK_LEN);

        let decoded = ClientFeedbackPacket::decode_verified(&buf[..len], Some(&key))
            .expect("signed AEAD v2 feedback must verify");
        assert_eq!(decoded.feedback_seq, feedback_seq);
        assert_eq!(decoded.hardware_delay_ms, hardware_delay_ms);
        assert_eq!(decoded.fec_rate_permille, 50);
        assert_eq!(decoded.client_volume, 100);
        assert_eq!(decoded.bluetooth_delay_ms, 0);
        assert_eq!(decoded.requested_mode, 0);

        // A different PIN cannot forge/verify the tag.
        let wrong_key = SessionKey::from_pin("0000");
        assert!(ClientFeedbackPacket::decode_verified(&buf[..len], Some(&wrong_key)).is_none());

        // Replay gate: authenticated seq <= last_seq is rejected.
        assert!(!feedback_is_replay(
            true,
            decoded.feedback_seq,
            decoded.feedback_seq - 1
        ));
        assert!(feedback_is_replay(
            true,
            decoded.feedback_seq,
            decoded.feedback_seq
        ));
        assert!(feedback_is_replay(
            true,
            decoded.feedback_seq - 1,
            decoded.feedback_seq
        ));
        // Unencrypted LAN feedback carries no sequence guarantee.
        assert!(!feedback_is_replay(false, 1, 5));
    }

    #[test]
    fn test_heal_packet_fec_interleaved_paired_fields() {
        use audioshare_protocol::lpc_lossless::encode_fec_subframe;
        use audioshare_protocol::sequencer::STATUS_HEALED_BY_FEC;

        fn tone(seed: usize) -> [i16; SUBFRAME_STEREO_SAMPLES] {
            let mut pcm = [0i16; SUBFRAME_STEREO_SAMPLES];
            for i in 0..SUBFRAME_FRAMES {
                let t = i as f32 / SAMPLE_RATE as f32;
                let freq = 220.0 + seed as f32 * 110.0;
                let v = ((t * freq * std::f32::consts::TAU).sin() * 14_000.0) as i16;
                pcm[i * 2] = v;
                pcm[i * 2 + 1] = v;
            }
            pcm
        }

        let mut sequencer = ZeroJitterSequencer::new();
        // Playhead at 100, packet base 104: heal N-1..N-4 (103..100).
        let base_seq = 104u16;
        let next_seq = 100u16;
        let mut n1 = [0u8; ADPCM_FEC_BYTES * 2];
        let mut n2 = [0u8; ADPCM_FEC_BYTES * 2];
        let mut e3 = [0u8; ADPCM_FEC_BYTES];
        let mut e2 = [0u8; ADPCM_FEC_BYTES];
        let mut e1 = [0u8; ADPCM_FEC_BYTES];
        let mut e0 = [0u8; ADPCM_FEC_BYTES];
        encode_fec_subframe(&tone(3), &mut e3);
        encode_fec_subframe(&tone(2), &mut e2);
        encode_fec_subframe(&tone(1), &mut e1);
        encode_fec_subframe(&tone(0), &mut e0);
        // fec_n1 = [N-1, N-3] = [103, 101]
        n1[..ADPCM_FEC_BYTES].copy_from_slice(&e3);
        n1[ADPCM_FEC_BYTES..].copy_from_slice(&e1);
        // fec_n2 = [N-2, N-4] = [102, 100]
        n2[..ADPCM_FEC_BYTES].copy_from_slice(&e2);
        n2[ADPCM_FEC_BYTES..].copy_from_slice(&e0);

        heal_packet_fec(base_seq, next_seq, &n1, &n2, &sequencer);

        for seq in next_seq..base_seq {
            assert!(!sequencer.is_slot_missing(seq), "slot {seq} was not healed");
            let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
            assert_eq!(sequencer.pop_next(seq, &mut out), STATUS_HEALED_BY_FEC);
            let peak = out.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
            assert!(peak > 0.01, "healed slot {seq} must not be silent");
        }
    }

    #[test]
    fn test_decode_primary_bundle_defers_opus_payload_to_ring() {
        fn tone() -> [i16; SUBFRAME_STEREO_SAMPLES] {
            let mut pcm = [0i16; SUBFRAME_STEREO_SAMPLES];
            for i in 0..SUBFRAME_FRAMES {
                let t = i as f32 / SAMPLE_RATE as f32;
                pcm[i * 2] = ((t * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
                pcm[i * 2 + 1] = pcm[i * 2];
            }
            pcm
        }

        let mut encoder = OpusStreamEncoder::new(128).expect("create Opus encoder");
        let mut payload = [0u8; MAX_OPUS_SUBFRAME_BYTES];
        let len = encoder
            .encode_subframe(&tone(), 128, &mut payload)
            .expect("encode Opus subframe");
        let mut bundle = [0u8; MAX_OPUS_SUBFRAME_BYTES + 2];
        bundle[0..2].copy_from_slice(&(len as u16).to_be_bytes());
        bundle[2..2 + len].copy_from_slice(&payload[..len]);

        let mut buffered: Vec<(u16, Vec<u8>)> = Vec::new();
        let mut pcm_calls = 0usize;
        let decoded = decode_primary_bundle(
            &bundle[..2 + len],
            1,
            55,
            None,
            |_, _| {},
            |_, _| pcm_calls += 1,
            |seq, raw| {
                buffered.push((seq, raw.to_vec()));
                true
            },
        );
        assert_eq!(decoded, 0, "deferred Opus frame must not decode on arrival");
        assert_eq!(pcm_calls, 0);
        assert_eq!(buffered.len(), 1);
        assert_eq!(buffered[0].0, 55);
        assert_eq!(&buffered[0].1[..], &payload[..len]);
    }

    #[test]
    fn test_opus_red_slots_are_inserted_and_counted_as_fec_healed() {
        use audioshare_protocol::opus_codec::{pack_opus_red_blob, MAX_OPUS_RED_BYTES};

        // The insertion test only checks slot bookkeeping, so the payload bytes
        // need not be decodable Opus frames here.
        let sf1: &[u8] = &[TAG_OPUS_RLD, 0x11, 0x22, 0x33];
        let sf2: &[u8] = &[TAG_OPUS_RLD, 0x44, 0x55];
        let mut blob = [0u8; MAX_OPUS_RED_BYTES];
        let blob_len = pack_opus_red_blob(100, &[sf1, sf2], &mut blob)
            .expect("two small subframes must fit in a RED blob");
        assert_eq!(blob[0], TAG_OPUS_RED);

        let sequencer = ZeroJitterSequencer::new();
        let mut opus_ring = vec![OpusPacketSlot::EMPTY; RING_SLOTS];
        let next_seq = 100u16;

        heal_opus_red_slots(&blob[..blob_len], next_seq, &sequencer, &mut opus_ring);

        for (i, expected) in [sf1, sf2].into_iter().enumerate() {
            let seq = 100u16 + i as u16;
            let slot = &opus_ring[(seq as usize) & (RING_SLOTS - 1)];
            assert_eq!(slot.seq, Some(seq), "RED slot {seq} must be filled");
            assert!(slot.from_red, "RED slot {seq} must be flagged from_red");
            assert_eq!(&slot.data[..slot.len as usize], expected);
            // At pop time a RED slot maps to FEC-healed, never primary.
            assert_eq!(opus_pop_status(slot.from_red), STATUS_HEALED_BY_FEC);
        }
        assert_eq!(opus_pop_status(false), STATUS_PRIMARY_LOSSLESS);

        // A non-RED FEC field is ignored, and a slot already holding a healed
        // frame is never overwritten or double-counted.
        heal_opus_red_slots(&[0x7E, 0x00], next_seq, &sequencer, &mut opus_ring);
        assert_eq!(opus_ring[100 & (RING_SLOTS - 1)].seq, Some(100));
        heal_opus_red_slots(&blob[..blob_len], next_seq, &sequencer, &mut opus_ring);
        assert_eq!(opus_ring[100 & (RING_SLOTS - 1)].seq, Some(100));

        // A sequence behind the playhead is rejected: that slot has already
        // been played and must not be reinserted by a stale RED blob.
        let mut old_blob = [0u8; MAX_OPUS_RED_BYTES];
        let old_len = pack_opus_red_blob(99, &[sf2], &mut old_blob).unwrap();
        heal_opus_red_slots(&old_blob[..old_len], next_seq, &sequencer, &mut opus_ring);
        assert_ne!(opus_ring[99 & (RING_SLOTS - 1)].seq, Some(99));
    }
}
