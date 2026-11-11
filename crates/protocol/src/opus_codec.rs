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

/// Verified `OPUS_GET_LOOKAHEAD` for `RestrictedLowDelay` at 48 kHz.
///
/// This is the only codec delay added by the Opus path. The lossless LPC and
/// pure-Rust VBR paths remain zero-lookahead.
pub const OPUS_LOOKAHEAD_MS: f32 = 2.5;

/// Opus encoder complexity. 5 is the measured sweet spot for 2.5 ms frames:
/// roughly 20 µs/frame encode+decode on x86-64, or under 1% of one core at
/// 400 frames/s.
const OPUS_COMPLEXITY: i32 = 5;

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
    /// `out` must have room for at least [`MAX_OPUS_SUBFRAME_BYTES`] bytes.
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
        let written = self
            .inner
            .encode_i16(pcm_stereo, SUBFRAME_FRAMES, &mut out[1..])
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
}

impl OpusStreamDecoder {
    /// Create a 48 kHz stereo decoder.
    pub fn new() -> Option<Self> {
        OpusDecoder::new(SAMPLE_RATE as i32, CHANNELS)
            .ok()
            .map(|inner| Self { inner })
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
    fn test_lookahead_matches_latency_constant() {
        let encoder = OpusStreamEncoder::new(128).expect("create Opus encoder");
        assert_eq!(encoder.lookahead_samples(), SUBFRAME_FRAMES);
        assert_eq!(OPUS_LOOKAHEAD_MS, 2.5);
        assert!(is_available());
    }
}
