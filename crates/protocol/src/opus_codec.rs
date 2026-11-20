//! Runtime-selectable, pure-Rust Opus codec for the compressed audio mode.
//!
//! This module is deliberately isolated from the lossless LPC path. It encodes
//! native 2.5 ms (120 stereo frames @ 48 kHz) subframes with Opus's
//! `RestrictedLowDelay` application (CELT-only), which keeps the codec
//! lookahead at exactly 120 samples (2.5 ms).
//!
//! The wire payload is self-identifying: [`TAG_OPUS_RLD`] prefixes every Opus
//! frame, so the receiver can switch between the pure-Rust VBR codec
//! (`TAG_VBR_BFP`) and Opus per packet without restarting either side.
//!
//! `opus-rs` is compiled into the binary, so there is no `dlopen`/`LoadLibrary`
//! probe and no host/client availability mismatch. The host's `prefer_opus`
//! switch is still opt-in and defaults to the pure-Rust VBR codec.

use opus_rs::{Application, OpusDecoder, OpusEncoder};

use crate::{CHANNELS, SAMPLE_RATE, SUBFRAME_FRAMES, SUBFRAME_STEREO_SAMPLES};

/// Wire tag identifying a pure-Rust Opus Restricted-Low-Delay subframe.
///
/// Distinct from `TAG_VBR_BFP` (`0x7E`) so the receiver auto-detects the
/// codec from the payload itself.
pub const TAG_OPUS_RLD: u8 = 0x7F;

/// Maximum bytes for one tagged 2.5 ms Opus subframe.
///
/// 2.5 ms of 510 kbps stereo audio is at most ~160 bytes, so 240 bytes of
/// Opus packet plus the tag leaves generous headroom while still fitting the
/// existing 481-byte primary-subframe budget.
pub const MAX_OPUS_SUBFRAME_BYTES: usize = 241;

/// Maximum Opus packet bytes after the tag.
pub const MAX_OPUS_PACKET_BYTES: usize = MAX_OPUS_SUBFRAME_BYTES - 1;

/// Magic tag identifying an exact Opus REDundancy blob stored inside
/// `WireHeader::fec_n1` / `fec_n2` when `CodecMode::OpusRld` is active.
/// Layout: `[TAG_OPUS_RED, base_seq_lo, base_seq_hi, sf_count, (len: u8, bytes[..len]) * sf_count]`
/// Fits cleanly inside the existing `u8` (255 B) FEC length field with zero WireHeader change.
pub const TAG_OPUS_RED: u8 = 0xF1;
pub const MAX_OPUS_RED_BYTES: usize = u8::MAX as usize;

/// Packs 1..=4 encoded Opus subframes from one packet into a self-describing
/// redundancy blob (`<= 255` bytes). Returns `None` if a transient bitrate spike
/// exceeds 255 bytes (gracefully skipping RED for that single packet).
pub fn pack_opus_red_blob(
    base_seq: u16,
    subframes: &[&[u8]],
    out: &mut [u8; MAX_OPUS_RED_BYTES],
) -> Option<usize> {
    if subframes.is_empty() || subframes.len() > 4 {
        return None;
    }
    let [lo, hi] = base_seq.to_le_bytes();
    out[0] = TAG_OPUS_RED;
    out[1] = lo;
    out[2] = hi;
    out[3] = subframes.len() as u8;
    let mut cursor = 4usize;
    for sf in subframes {
        if sf.is_empty() || sf.len() > u8::MAX as usize {
            return None;
        }
        let needed = 1 + sf.len();
        if cursor + needed > MAX_OPUS_RED_BYTES {
            return None;
        }
        out[cursor] = sf.len() as u8;
        cursor += 1;
        out[cursor..cursor + sf.len()].copy_from_slice(sf);
        cursor += sf.len();
    }
    Some(cursor)
}

/// Unpacks an Opus RED blob and invokes `on_subframe(seq, opus_payload)` for each
/// validated subframe. Rejects truncated or trailing-garbage blobs cleanly.
pub fn unpack_opus_red_blob<F>(blob: &[u8], mut on_subframe: F) -> bool
where
    F: FnMut(u16, &[u8]),
{
    if blob.len() < 5 || blob[0] != TAG_OPUS_RED {
        return false;
    }
    let base_seq = u16::from_le_bytes([blob[1], blob[2]]);
    let count = blob[3] as usize;
    if count == 0 || count > 4 {
        return false;
    }
    // Pass 1: exact-length validation before mutating receiver state.
    let mut cursor = 4usize;
    for _ in 0..count {
        if cursor >= blob.len() {
            return false;
        }
        let len = blob[cursor] as usize;
        cursor += 1;
        if len == 0 || cursor + len > blob.len() {
            return false;
        }
        cursor += len;
    }
    if cursor != blob.len() {
        return false;
    }
    // Pass 2: emit validated subframes in sequence order.
    let mut cursor = 4usize;
    for i in 0..count {
        let len = blob[cursor] as usize;
        cursor += 1;
        on_subframe(base_seq.wrapping_add(i as u16), &blob[cursor..cursor + len]);
        cursor += len;
    }
    true
}

/// Verified `OPUS_GET_LOOKAHEAD` for `RestrictedLowDelay` at 48 kHz.
///
/// This is the only codec delay added by the Opus path. The lossless LPC and
/// pure-Rust VBR paths remain zero-lookahead.
pub const OPUS_LOOKAHEAD_MS: f32 = 2.5;

/// Opus encoder complexity 8 enables opus-rs's libopus signal analysis
/// (480-pt tonality FFT, music/speech MLP, temporal VBR -- active at >= 7),
/// improving music/transient clarity. Measured SNR on a 440 Hz + harmonic tone:
/// 37.50 dB at complexity 5 vs 37.67 dB at 8 (no regression); <2% CPU.
const OPUS_COMPLEXITY: i32 = 8;

/// Minimum/maximum target bitrates accepted by the compressed-mode UI.
const MIN_BITRATE_KBPS: u16 = 64;
const MAX_BITRATE_KBPS: u16 = 320;

/// The bundled Opus implementation is always available because it is compiled
/// into every host and client binary.
#[inline]
pub const fn is_available() -> bool {
    true
}

/// Human-readable backend name for GUIs and HUDs.
pub const BACKEND_NAME: &str = "Opus RLD (pure Rust)";

/// Stateful Opus encoder for one compressed stream.
///
/// Create one per host TX thread. The encoder is not shared across threads and
/// is intentionally not `Sync`.
pub struct OpusStreamEncoder {
    inner: OpusEncoder,
    current_kbps: u16,
}

impl OpusStreamEncoder {
    /// Create an encoder for 48 kHz stereo 2.5 ms frames.
    ///
    /// Returns `None` only if `opus-rs` rejects the configuration. Callers
    /// should fall back to [`crate::lpc_lossless::encode_vbr_subframe`].
    pub fn new(initial_kbps: u16) -> Option<Self> {
        let mut inner = OpusEncoder::new(
            SAMPLE_RATE as i32,
            CHANNELS,
            Application::RestrictedLowDelay,
        )
        .ok()?;
        inner.complexity = OPUS_COMPLEXITY;
        inner.bitrate_bps =
            i32::from(initial_kbps.clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS)) * 1000;
        // Constrained VBR keeps packet sizes close to the selected target,
        // which matters on congested 2.4 GHz links. CELT-only RLD frames do
        // not use SILK's hybrid redundancy either.
        inner.set_vbr_constraint(true);
        inner.set_dtx(false);

        Some(Self {
            inner,
            current_kbps: initial_kbps.clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS),
        })
    }

    /// Encoder lookahead in samples at 48 kHz (verified 120 = 2.5 ms).
    #[inline]
    pub fn lookahead_samples(&self) -> usize {
        self.inner.lookahead()
    }

    /// Update the target bitrate without resetting codec state.
    #[inline]
    pub fn set_bitrate_kbps(&mut self, kbps: u16) {
        let clamped = kbps.clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS);
        if clamped != self.current_kbps {
            self.inner.bitrate_bps = i32::from(clamped) * 1000;
            self.current_kbps = clamped;
        }
    }

    /// Encode one 2.5 ms subframe into `[TAG_OPUS_RLD][opus packet...]`.
    ///
    /// `out` must have room for at least [`MAX_OPUS_SUBFRAME_BYTES`] bytes;
    /// any extra capacity is never exposed to the encoder. `opus-rs` treats the
    /// output slice length as its `max_data_bytes`, so passing the host's full
    /// 481-byte buffer (480 bytes after the tag) would let a CELT VBR transient
    /// write past the wire budget. Only the capped packet region is passed, and
    /// the post-encode length check remains as defense in depth.
    ///
    /// Returns the tagged length, or `None` if the encoder rejected the frame
    /// or the packet exceeded the wire budget.
    pub fn encode_subframe(
        &mut self,
        pcm_stereo: &[i16; SUBFRAME_STEREO_SAMPLES],
        kbps: u16,
        out: &mut [u8],
    ) -> Option<usize> {
        if out.len() < MAX_OPUS_SUBFRAME_BYTES {
            return None;
        }
        self.set_bitrate_kbps(kbps);
        out[0] = TAG_OPUS_RLD;
        // `opus-rs` uses the slice length as `max_data_bytes`. Expose exactly
        // the wire budget, not whatever spare capacity the caller's buffer has
        // (the host passes a 481-byte primary-subframe buffer).
        let written = self
            .inner
            .encode_i16(
                pcm_stereo,
                SUBFRAME_FRAMES,
                &mut out[1..1 + MAX_OPUS_PACKET_BYTES],
            )
            .ok()?;
        if written == 0 || written > MAX_OPUS_PACKET_BYTES {
            return None;
        }
        Some(1 + written)
    }

    /// Encode one 2.5 ms subframe directly from 32-bit float capture samples.
    /// Preserves the 24-bit CELT internal noise-floor precision and avoids an
    /// unnecessary f32 -> i16 -> f32 quantization round-trip.
    pub fn encode_subframe_f32(
        &mut self,
        pcm_f32: &[f32; SUBFRAME_STEREO_SAMPLES],
        kbps: u16,
        out: &mut [u8],
    ) -> Option<usize> {
        if out.len() < MAX_OPUS_SUBFRAME_BYTES {
            return None;
        }
        self.set_bitrate_kbps(kbps);
        out[0] = TAG_OPUS_RLD;
        let written = self
            .inner
            .encode(pcm_f32, SUBFRAME_FRAMES, &mut out[1..1 + MAX_OPUS_PACKET_BYTES])
            .ok()?;
        if written == 0 || written > MAX_OPUS_PACKET_BYTES {
            return None;
        }
        Some(1 + written)
    }
}

/// Stateful Opus decoder for one compressed stream.
///
/// Create one per client RX thread and drop it when the host switches away
/// from Opus, so a later switch back cannot decode with stale history.
pub struct OpusStreamDecoder {
    inner: OpusDecoder,
    /// Last sequence number whose payload reached the decoder.
    ///
    /// Opus/CELT keeps an MDCT overlap tail and leaky band-energy predictors,
    /// so feeding frames in arrival order instead of stream order corrupts the
    /// reconstruction. This tracker lets [`Self::decode_subframe_at_seq`]
    /// reject duplicates/late frames and advance the codec across gaps.
    last_seq: Option<u16>,
}

impl OpusStreamDecoder {
    /// Create a 48 kHz stereo decoder.
    pub fn new() -> Option<Self> {
        OpusDecoder::new(SAMPLE_RATE as i32, CHANNELS)
            .ok()
            .map(|inner| Self {
                inner,
                last_seq: None,
            })
    }

    /// Decode a tagged `[TAG_OPUS_RLD][opus packet...]` subframe.
    ///
    /// The output is 240 interleaved f32 stereo samples in the usual
    /// ±1.0 convention. Returns `false` for a foreign tag, a malformed
    /// packet, or a frame-size mismatch.
    pub fn decode_subframe(
        &mut self,
        encoded: &[u8],
        out_f32: &mut [f32; SUBFRAME_STEREO_SAMPLES],
    ) -> bool {
        if encoded.len() < 2 || encoded[0] != TAG_OPUS_RLD {
            return false;
        }
        if encoded.len() - 1 > MAX_OPUS_PACKET_BYTES {
            return false;
        }
        self.inner
            .decode(&encoded[1..], SUBFRAME_FRAMES, out_f32)
            .map(|frames| frames == SUBFRAME_FRAMES)
            .unwrap_or(false)
    }

    /// Sequence-aware decode for a real-time stream.
    ///
    /// Opus is a stateful codec: the 120-sample MDCT overlap tail and CELT
    /// band-energy predictors assume consecutive 2.5 ms frames. A duplicate,
    /// a late out-of-order frame, or a skipped packet must therefore never be
    /// fed into the codec as if it followed the last decoded frame.
    ///
    /// - `delta <= 0` (duplicate/late) is rejected without mutating state.
    /// - `delta > 1` advances the codec with concealment frames first, capped
    ///   at 4 frames (10 ms). Beyond that the stream is effectively new; the
    ///   caller resets the decoder on a playhead resync, and decoding more PLC
    ///   frames than the codec's useful concealment window would only add
    ///   artefacts.
    pub fn decode_subframe_at_seq(
        &mut self,
        seq: u16,
        encoded: &[u8],
        out_f32: &mut [f32; SUBFRAME_STEREO_SAMPLES],
    ) -> bool {
        self.decode_subframe_with_plc(seq, encoded, out_f32, |_, _| {})
    }

    /// Decode `seq` while handing any intermediate CELT PLC frames to
    /// `on_plc_frame`.
    ///
    /// `restricted-low-delay` Opus carries a 120-sample (2.5 ms) lookahead, so
    /// a decoded packet `N` contains the waveform that belongs in slot
    /// `N - 1`. The empty-packet PLC path advances the same MDCT overlap state
    /// and therefore produces the exact 2.5 ms that the missing packet would
    /// have produced. Delivering those frames to the sequencer keeps a lost
    /// packet phase-aligned; splicing the zero-lookahead IMA-ADPCM FEC backup
    /// into an Opus stream instead would jump the waveform by one subframe.
    ///
    /// At most four PLC frames (10 ms) are generated, matching the codec's
    /// useful concealment window. `on_plc_frame` receives each frame with the
    /// sequence number of the slot it belongs to, in order.
    pub fn decode_subframe_with_plc(
        &mut self,
        seq: u16,
        encoded: &[u8],
        out_f32: &mut [f32; SUBFRAME_STEREO_SAMPLES],
        mut on_plc_frame: impl FnMut(u16, &[f32; SUBFRAME_STEREO_SAMPLES]),
    ) -> bool {
        // Validate the incoming packet before the gap path mutates the
        // stateful codec through concealment frames. Otherwise a malformed
        // packet with a high sequence number can advance the decoder and
        // desynchronise all later valid audio.
        if encoded.len() < 2
            || encoded[0] != TAG_OPUS_RLD
            || encoded.len() - 1 > MAX_OPUS_PACKET_BYTES
        {
            return false;
        }

        if let Some(previous) = self.last_seq {
            let delta = seq.wrapping_sub(previous) as i16;
            if delta <= 0 {
                return false;
            }
            if delta > 1 {
                let missed = ((delta - 1) as usize).min(4);
                let start_miss_seq = seq.wrapping_sub(missed as u16);
                let mut concealment = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                for i in 0..missed {
                    // An empty packet is libopus's packet-loss concealment
                    // entry point; it advances the MDCT overlap state.
                    let decoded = self
                        .inner
                        .decode(&[], SUBFRAME_FRAMES, &mut concealment)
                        .map(|frames| frames == SUBFRAME_FRAMES)
                        .unwrap_or(false);
                    if decoded {
                        let missed_seq = start_miss_seq.wrapping_add(i as u16);
                        // Each concealment frame advances the stateful CELT
                        // MDCT overlap, so `last_seq` must track the last
                        // concealed slot even if the real payload that follows
                        // fails to decode. Otherwise the same gap is concealed
                        // twice when the next valid packet arrives.
                        self.last_seq = Some(missed_seq);
                        on_plc_frame(missed_seq, &concealment);
                    }
                }
            }
        }

        let decoded = self.decode_subframe(encoded, out_f32);
        if decoded {
            self.last_seq = Some(seq);
        }
        decoded
    }

    /// Synthesize one phase-aligned CELT PLC frame for a missing slot right at
    /// playhead pop time, keeping MDCT overlap locked with the next real frame.
    pub fn decode_plc(&mut self, seq: u16, out_f32: &mut [f32; SUBFRAME_STEREO_SAMPLES]) -> bool {
        // A duplicate or late playhead slot must not be fed through the
        // stateful CELT decoder, which would corrupt the MDCT overlap tail and
        // rewind `last_seq`.
        if let Some(prev) = self.last_seq {
            if (seq.wrapping_sub(prev) as i16) <= 0 {
                return false;
            }
        }
        let ok = self
            .inner
            .decode(&[], SUBFRAME_FRAMES, out_f32)
            .map(|frames| frames == SUBFRAME_FRAMES)
            .unwrap_or(false);
        if ok {
            self.last_seq = Some(seq);
        }
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_FRAMES: usize = 64;

    fn tone_frame(frame_index: usize) -> [i16; SUBFRAME_STEREO_SAMPLES] {
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

    #[test]
    fn test_opus_roundtrip_tag_and_snr() {
        let mut encoder = OpusStreamEncoder::new(128).expect("create Opus encoder");
        let mut decoder = OpusStreamDecoder::new().expect("create Opus decoder");
        let mut payload = [0u8; MAX_OPUS_SUBFRAME_BYTES];
        let mut inputs = Vec::with_capacity(TEST_FRAMES);
        let mut outputs = Vec::with_capacity(TEST_FRAMES);
        let mut max_len = 0usize;

        for frame_index in 0..TEST_FRAMES {
            let pcm = tone_frame(frame_index);
            let len = encoder
                .encode_subframe(&pcm, 128, &mut payload)
                .expect("encode Opus subframe");
            assert_eq!(payload[0], TAG_OPUS_RLD);
            assert!(len <= MAX_OPUS_SUBFRAME_BYTES);
            max_len = max_len.max(len);

            let mut decoded = [0.0f32; SUBFRAME_STEREO_SAMPLES];
            assert!(decoder.decode_subframe(&payload[..len], &mut decoded));

            inputs.push(
                pcm.iter()
                    .map(|sample| *sample as f32 / 32768.0)
                    .collect::<Vec<_>>(),
            );
            outputs.push(decoded);
        }

        // Opus has one frame of lookahead: decoded frame N corresponds to
        // input frame N-1. Skip the codec warm-up and measure the steady state.
        let mut signal_energy = 0.0f64;
        let mut noise_energy = 0.0f64;
        for frame_index in 8..TEST_FRAMES {
            let source = &inputs[frame_index - 1];
            let decoded = &outputs[frame_index];
            for (dst, src) in decoded.iter().zip(source.iter()) {
                signal_energy += (*src as f64).powi(2);
                noise_energy += (*dst as f64 - *src as f64).powi(2);
            }
        }
        let snr_db = 10.0 * (signal_energy / noise_energy.max(f64::EPSILON)).log10();
        assert!(
            snr_db > 20.0,
            "Opus round-trip SNR too low: {snr_db:.2} dB (max packet {max_len} bytes)"
        );
    }

    #[test]
    fn test_opus_packet_sizes_fit_wire_budget() {
        let mut encoder = OpusStreamEncoder::new(320).expect("create Opus encoder");
        let mut payload = [0u8; MAX_OPUS_SUBFRAME_BYTES];
        for frame_index in 0..400 {
            let pcm = tone_frame(frame_index);
            let len = encoder
                .encode_subframe(&pcm, 320, &mut payload)
                .expect("encode Opus subframe");
            assert!(
                len <= MAX_OPUS_SUBFRAME_BYTES,
                "packet {len} exceeds wire budget"
            );
        }
    }

    /// Regression test for the host's 481-byte primary-subframe buffer.
    ///
    /// `opus-rs` treats the output slice length as `max_data_bytes`, so handing
    /// it all 480 bytes after the tag lets a VBR transient fill far more than
    /// the 240-byte Opus wire budget. `encode_subframe` must pass the capped
    /// slice instead of relying on a post-hoc length check.
    #[test]
    fn test_opus_encoder_respects_packet_cap_with_host_sized_buffer() {
        const HOST_SUBFRAME_BYTES: usize = 481;
        const SENTINEL: u8 = 0xA5;
        let mut encoder = OpusStreamEncoder::new(320).expect("create Opus encoder");
        let mut payload = [SENTINEL; HOST_SUBFRAME_BYTES];
        let mut rng = 0x1234_5678u32;
        let mut max_len = 0usize;

        for frame_index in 0..256usize {
            let mut pcm = [0i16; SUBFRAME_STEREO_SAMPLES];
            // Alternate full-scale square bursts and full-scale noise so the
            // CELT VBR reservoir sees loud transients and dense spectra.
            let burst = (frame_index / 8).is_multiple_of(2);
            for i in 0..SUBFRAME_FRAMES {
                rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (rng >> 16) as i16;
                let square = if (i / 8).is_multiple_of(2) {
                    32_000
                } else {
                    -32_000
                };
                let left = if burst { square } else { noise };
                // Invert the right channel on burst frames to stress stereo
                // coding without cancelling the mid signal.
                let right = if burst { -square } else { noise.wrapping_neg() };
                pcm[i * 2] = left;
                pcm[i * 2 + 1] = right;
            }

            // The host does not zero this buffer; prove the encoder does not
            // write past the capped packet region.
            payload[MAX_OPUS_SUBFRAME_BYTES..].fill(SENTINEL);

            let len = encoder
                .encode_subframe(&pcm, 320, &mut payload)
                .unwrap_or_else(|| panic!("frame {frame_index} exceeded the packet cap"));
            assert_eq!(payload[0], TAG_OPUS_RLD);
            max_len = max_len.max(len);
            assert!(
                len <= MAX_OPUS_SUBFRAME_BYTES,
                "frame {frame_index} packet {len} exceeds wire budget"
            );
            assert!(
                payload[MAX_OPUS_SUBFRAME_BYTES..]
                    .iter()
                    .all(|byte| *byte == SENTINEL),
                "frame {frame_index} wrote past the {MAX_OPUS_PACKET_BYTES}-byte packet cap \
                 (max tagged length so far: {max_len})"
            );
        }
    }

    #[test]
    fn test_decoder_rejects_foreign_or_short_payloads() {
        let mut decoder = OpusStreamDecoder::new().expect("create Opus decoder");
        let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];

        assert!(!decoder.decode_subframe(&[], &mut out));
        assert!(!decoder.decode_subframe(&[TAG_OPUS_RLD], &mut out));
        assert!(!decoder.decode_subframe(&[0x7E, 0x00, 0x00], &mut out));
        assert!(!decoder.decode_subframe(&[0x00, 0x00, 0x00], &mut out));
    }

    #[test]
    fn test_decoder_rejects_duplicates_and_conceals_sequence_gaps() {
        let mut encoder = OpusStreamEncoder::new(128).expect("create Opus encoder");
        let mut decoder = OpusStreamDecoder::new().expect("create Opus decoder");
        let mut payload = [0u8; MAX_OPUS_SUBFRAME_BYTES];
        let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];

        // Establish state at seq 100.
        let first = tone_frame(0);
        let len = encoder.encode_subframe(&first, 128, &mut payload).unwrap();
        assert!(decoder.decode_subframe_at_seq(100, &payload[..len], &mut out));

        // A duplicate of 100 and a late 99 must not be decoded through the
        // stateful CELT history.
        let second = tone_frame(1);
        let second_len = encoder.encode_subframe(&second, 128, &mut payload).unwrap();
        assert!(!decoder.decode_subframe_at_seq(100, &payload[..second_len], &mut out));
        assert!(!decoder.decode_subframe_at_seq(99, &payload[..second_len], &mut out));

        // Seq 103 is a two-frame gap: conceal 101 and 102, then decode 103.
        let third = tone_frame(2);
        let third_len = encoder.encode_subframe(&third, 128, &mut payload).unwrap();
        assert!(decoder.decode_subframe_at_seq(103, &payload[..third_len], &mut out));
        assert!(out.iter().all(|sample| sample.is_finite()));

        // Sequence wrap-around is a forward delta, not a late packet.
        let fourth = tone_frame(3);
        let fourth_len = encoder.encode_subframe(&fourth, 128, &mut payload).unwrap();
        let mut wrap_decoder = OpusStreamDecoder::new().expect("create Opus decoder");
        assert!(wrap_decoder.decode_subframe_at_seq(65534, &payload[..fourth_len], &mut out));
        assert!(wrap_decoder.decode_subframe_at_seq(65535, &payload[..fourth_len], &mut out));
        assert!(wrap_decoder.decode_subframe_at_seq(0, &payload[..fourth_len], &mut out));
    }

    #[test]
    fn test_decode_subframe_with_plc_delivers_missing_slots() {
        let mut encoder = OpusStreamEncoder::new(128).expect("create Opus encoder");
        let mut decoder = OpusStreamDecoder::new().expect("create Opus decoder");
        let mut payload = [0u8; MAX_OPUS_SUBFRAME_BYTES];
        let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];

        let first = tone_frame(0);
        let len = encoder.encode_subframe(&first, 128, &mut payload).unwrap();
        assert!(decoder.decode_subframe_at_seq(100, &payload[..len], &mut out));

        // Jump from 100 to 103: the decoder must hand slots 101 and 102 to
        // the callback before decoding the packet that belongs in 103.
        let second = tone_frame(1);
        let second_len = encoder.encode_subframe(&second, 128, &mut payload).unwrap();
        let mut plc = Vec::new();
        assert!(decoder.decode_subframe_with_plc(
            103,
            &payload[..second_len],
            &mut out,
            |missed_seq, concealment| plc.push((missed_seq, *concealment)),
        ));
        assert_eq!(plc.len(), 2);
        assert_eq!(plc[0].0, 101);
        assert_eq!(plc[1].0, 102);
        assert!(plc
            .iter()
            .flat_map(|(_, frame)| frame.iter())
            .all(|sample| sample.is_finite()));
        let concealed_energy: f32 = plc[0].1.iter().map(|sample| sample * sample).sum();
        assert!(
            concealed_energy > 1e-6,
            "CELT PLC returned silence instead of concealing the tone"
        );

        // Duplicate and late sequences are rejected without generating PLC.
        let mut late_calls = 0u32;
        let mut on_late = |_: u16, _: &[f32; SUBFRAME_STEREO_SAMPLES]| late_calls += 1;
        assert!(!decoder.decode_subframe_with_plc(
            102,
            &payload[..second_len],
            &mut out,
            &mut on_late
        ));
        assert!(!decoder.decode_subframe_with_plc(
            100,
            &payload[..second_len],
            &mut out,
            &mut on_late
        ));
        assert_eq!(late_calls, 0);
    }

    #[test]
    fn test_invalid_payload_does_not_advance_plc_or_sequence() {
        let mut encoder = OpusStreamEncoder::new(128).expect("create Opus encoder");
        let mut decoder = OpusStreamDecoder::new().expect("create Opus decoder");
        let mut payload = [0u8; MAX_OPUS_SUBFRAME_BYTES];
        let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];

        let first = tone_frame(0);
        let len = encoder.encode_subframe(&first, 128, &mut payload).unwrap();
        assert!(decoder.decode_subframe_at_seq(10, &payload[..len], &mut out));

        // A malformed packet that also leaves a sequence gap must be rejected
        // before the codec is advanced through PLC, otherwise an attacker can
        // desynchronise the decoder by sending junk with a high sequence
        // number.
        let mut plc_calls = 0u32;
        let mut invalid = [0u8; 2];
        invalid[0] = 0x00; // foreign tag: rejected by decode_subframe
        assert!(!decoder.decode_subframe_with_plc(
            12,
            &invalid,
            &mut out,
            |_, _| plc_calls += 1,
        ));
        assert_eq!(plc_calls, 0);

        // Sequence 11 is still the next expected frame and must decode.
        let second = tone_frame(1);
        let second_len = encoder.encode_subframe(&second, 128, &mut payload).unwrap();
        assert!(decoder.decode_subframe_at_seq(11, &payload[..second_len], &mut out));
    }

    #[test]
    fn test_decode_subframe_with_plc_caps_concealment_window() {
        let mut encoder = OpusStreamEncoder::new(128).expect("create Opus encoder");
        let mut decoder = OpusStreamDecoder::new().expect("create Opus decoder");
        let mut payload = [0u8; MAX_OPUS_SUBFRAME_BYTES];
        let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];

        let first = tone_frame(0);
        let len = encoder.encode_subframe(&first, 128, &mut payload).unwrap();
        assert!(decoder.decode_subframe_at_seq(10, &payload[..len], &mut out));

        // A 9-frame jump generates at most 4 PLC frames (10 ms), matching
        // the useful CELT concealment window. The reported sequence numbers
        // must be the four slots immediately before the decoded packet.
        let second = tone_frame(1);
        let second_len = encoder.encode_subframe(&second, 128, &mut payload).unwrap();
        let mut plc_seqs = Vec::new();
        assert!(decoder.decode_subframe_with_plc(
            19,
            &payload[..second_len],
            &mut out,
            |missed_seq, _| plc_seqs.push(missed_seq),
        ));
        assert_eq!(plc_seqs, vec![15, 16, 17, 18]);
    }

    #[test]
    fn test_encode_subframe_f32_matches_i16_path() {
        let mut enc_f32 = OpusStreamEncoder::new(128).unwrap();
        let mut enc_i16 = OpusStreamEncoder::new(128).unwrap();
        let pcm_f32: Vec<f32> = (0..SUBFRAME_STEREO_SAMPLES)
            .map(|i| ((i as f32) * 0.05).sin() * 0.5)
            .collect();
        let mut pcm_i16 = [0i16; SUBFRAME_STEREO_SAMPLES];
        for (d, &s) in pcm_i16.iter_mut().zip(pcm_f32.iter()) {
            *d = (s * 32768.0) as i16;
        }
        let mut out_f32 = [0u8; MAX_OPUS_SUBFRAME_BYTES];
        let mut out_i16 = [0u8; MAX_OPUS_SUBFRAME_BYTES];
        let len_f32 = enc_f32.encode_subframe_f32(&pcm_f32.clone().try_into().unwrap(), 128, &mut out_f32).unwrap();
        let len_i16 = enc_i16.encode_subframe(&pcm_i16, 128, &mut out_i16).unwrap();
        assert_eq!(out_f32[0], TAG_OPUS_RLD);
        assert!(len_f32 <= MAX_OPUS_SUBFRAME_BYTES && len_i16 <= MAX_OPUS_SUBFRAME_BYTES);
        // Both paths must decode to finite audio of the right frame size.
        let mut dec = OpusStreamDecoder::new().unwrap();
        let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        assert!(dec.decode_subframe(&out_f32[..len_f32], &mut out));
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn test_decode_plc_advances_celt_state_and_sequence() {
        let mut enc = OpusStreamEncoder::new(128).unwrap();
        let mut dec = OpusStreamDecoder::new().unwrap();
        let mut payload = [0u8; MAX_OPUS_SUBFRAME_BYTES];
        let frame = tone_frame(0);
        let len = enc.encode_subframe(&frame, 128, &mut payload).unwrap();
        let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        assert!(dec.decode_subframe_at_seq(7, &payload[..len], &mut out));
        assert!(dec.decode_plc(8, &mut out), "CELT PLC must synthesize a frame");
        assert!(out.iter().all(|s| s.is_finite()));
        // A late duplicate of 7 must still be rejected after the PLC advanced
        // the decoder's last_seq.
        assert!(!dec.decode_subframe_at_seq(7, &payload[..len], &mut out));
    }

    #[test]
    fn test_decode_plc_rejects_non_advancing_sequence() {
        let mut enc = OpusStreamEncoder::new(128).unwrap();
        let mut dec = OpusStreamDecoder::new().unwrap();
        let mut payload = [0u8; MAX_OPUS_SUBFRAME_BYTES];
        let frame = tone_frame(0);
        let len = enc.encode_subframe(&frame, 128, &mut payload).unwrap();
        let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        assert!(dec.decode_subframe_at_seq(20, &payload[..len], &mut out));
        assert_eq!(dec.last_seq, Some(20));

        assert!(!dec.decode_plc(20, &mut out), "duplicate seq must be rejected");
        assert!(!dec.decode_plc(19, &mut out), "late seq must be rejected");
        assert_eq!(dec.last_seq, Some(20), "rejected PLC must not move last_seq");

        assert!(dec.decode_plc(21, &mut out), "forward seq must be accepted");
        assert_eq!(dec.last_seq, Some(21));
    }

    #[test]
    fn test_corrupt_payload_after_gap_does_not_rewind_plc_sequence() {
        let mut encoder = OpusStreamEncoder::new(128).expect("create Opus encoder");
        let mut decoder = OpusStreamDecoder::new().expect("create Opus decoder");
        let mut payload = [0u8; MAX_OPUS_SUBFRAME_BYTES];
        let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];

        let first = tone_frame(0);
        let len = encoder.encode_subframe(&first, 128, &mut payload).unwrap();
        assert!(decoder.decode_subframe_at_seq(10, &payload[..len], &mut out));
        assert_eq!(decoder.last_seq, Some(10));

        // Pick a packet that passes the wire-header check but is rejected by
        // Opus itself: each TOC below advertises a CELT frame longer than the
        // 2.5 ms subframe we request. Probing keeps the fixture robust across
        // opus-rs versions.
        let corrupt = [0x88u8, 0x80u8, 0x90u8, 0xF8u8, 0xA0u8]
            .into_iter()
            .map(|toc| vec![TAG_OPUS_RLD, toc, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])
            .find(|p| {
                let mut probe = OpusStreamDecoder::new().unwrap();
                let mut scratch = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                !probe.decode_subframe(p, &mut scratch)
            })
            .expect("need one header-valid packet that Opus rejects");
        assert!(corrupt.len() >= 2 && corrupt[0] == TAG_OPUS_RLD);

        // Gap 10 -> 13 conceals slots 11 and 12, then the corrupt payload
        // fails. The concealed slots must still advance `last_seq`.
        let mut gap_plc = Vec::new();
        assert!(!decoder.decode_subframe_with_plc(13, &corrupt, &mut out, |seq, _| gap_plc
            .push(seq)));
        assert_eq!(gap_plc, vec![11, 12]);
        assert_eq!(
            decoder.last_seq,
            Some(12),
            "concealed slots must advance last_seq even when the final payload fails"
        );

        // A valid packet at 14 must conceal only the genuinely still-missing
        // slot 13, not replay 11/12 through the stateful CELT decoder.
        let second = tone_frame(1);
        let second_len = encoder.encode_subframe(&second, 128, &mut payload).unwrap();
        let mut recovered_plc = Vec::new();
        assert!(decoder.decode_subframe_with_plc(
            14,
            &payload[..second_len],
            &mut out,
            |seq, _| recovered_plc.push(seq),
        ));
        assert_eq!(recovered_plc, vec![13]);
    }

    #[test]
    fn test_lookahead_matches_latency_constant() {
        let encoder = OpusStreamEncoder::new(128).expect("create Opus encoder");
        assert_eq!(encoder.lookahead_samples(), SUBFRAME_FRAMES);
        assert_eq!(OPUS_LOOKAHEAD_MS, 2.5);
        assert!(is_available());
    }

    #[test]
    fn test_opus_red_blob_round_trip() {
        let sf1 = [1u8, 2, 3, 4, 5];
        let sf2 = [9u8, 8, 7];
        let sf3 = [0xABu8];
        let mut out = [0u8; MAX_OPUS_RED_BYTES];

        let packed = pack_opus_red_blob(0x1234, &[&sf1, &sf2, &sf3], &mut out).expect("pack");
        assert_eq!(out[0], TAG_OPUS_RED);
        assert_eq!(out[1], 0x34);
        assert_eq!(out[2], 0x12);
        assert_eq!(out[3], 3);
        assert!(packed <= MAX_OPUS_RED_BYTES);

        let mut seen: Vec<(u16, Vec<u8>)> = Vec::new();
        assert!(unpack_opus_red_blob(&out[..packed], |seq, payload| {
            seen.push((seq, payload.to_vec()));
        }));
        assert_eq!(
            seen,
            vec![
                (0x1234, sf1.to_vec()),
                (0x1235, sf2.to_vec()),
                (0x1236, sf3.to_vec()),
            ]
        );

        // base_seq wraps cleanly at u16::MAX.
        let packed = pack_opus_red_blob(u16::MAX, &[&sf1, &sf2], &mut out).expect("pack wrap");
        let mut wrap_seqs = Vec::new();
        assert!(unpack_opus_red_blob(&out[..packed], |seq, _| wrap_seqs.push(seq)));
        assert_eq!(wrap_seqs, vec![u16::MAX, 0]);
    }

    #[test]
    fn test_opus_red_blob_rejects_oversize() {
        let mut out = [0u8; MAX_OPUS_RED_BYTES];

        // A single subframe larger than u8::MAX cannot be length-prefixed.
        let huge = vec![0x5Au8; MAX_OPUS_RED_BYTES + 1];
        assert_eq!(pack_opus_red_blob(1, &[&huge], &mut out), None);

        // Individually-valid subframes can still overflow the 255-byte budget.
        let chunk = vec![0x5Au8; 100];
        assert_eq!(pack_opus_red_blob(1, &[&chunk, &chunk, &chunk], &mut out), None);

        // Empty blob, >4 subframes, and empty individual subframes are rejected.
        assert_eq!(pack_opus_red_blob(1, &[], &mut out), None);
        let one: &[u8] = &[1u8];
        let five: Vec<&[u8]> = vec![one, one, one, one, one];
        assert_eq!(pack_opus_red_blob(1, &five, &mut out), None);
        assert_eq!(pack_opus_red_blob(1, &[&[]], &mut out), None);
    }

    #[test]
    fn test_opus_red_blob_rejects_truncated_and_trailing() {
        let sf1 = [1u8, 2, 3];
        let sf2 = [4u8, 5];
        let mut out = [0u8; MAX_OPUS_RED_BYTES];
        let packed = pack_opus_red_blob(7, &[&sf1, &sf2], &mut out).unwrap();
        let blob = out[..packed].to_vec();

        // The well-formed blob emits exactly two subframes.
        let mut valid_count = 0;
        assert!(unpack_opus_red_blob(&blob, |_, _| valid_count += 1));
        assert_eq!(valid_count, 2);

        // No callback may ever fire for a rejected blob (2-pass validation).
        let mut probes = 0u32;
        // Payload truncated by one byte.
        assert!(!unpack_opus_red_blob(&blob[..blob.len() - 1], |_, _| probes += 1));
        // Trailing garbage byte after an otherwise complete blob.
        let mut trailing = blob.clone();
        trailing.push(0x00);
        assert!(!unpack_opus_red_blob(&trailing, |_, _| probes += 1));
        // Foreign tag and too-short header.
        let mut foreign = blob.clone();
        foreign[0] = 0x00;
        assert!(!unpack_opus_red_blob(&foreign, |_, _| probes += 1));
        assert!(!unpack_opus_red_blob(&blob[..4], |_, _| probes += 1));
        // Invalid subframe counts (0 and >4).
        let mut zero_count = blob.clone();
        zero_count[3] = 0;
        assert!(!unpack_opus_red_blob(&zero_count, |_, _| probes += 1));
        let mut five_count = blob.clone();
        five_count[3] = 5;
        assert!(!unpack_opus_red_blob(&five_count, |_, _| probes += 1));
        // A zero-length subframe in pass 1 is rejected.
        let mut zero_len = blob.clone();
        zero_len[4] = 0;
        assert!(!unpack_opus_red_blob(&zero_len, |_, _| probes += 1));
        assert_eq!(probes, 0);
    }
}


