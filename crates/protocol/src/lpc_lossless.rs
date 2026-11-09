use crate::{SUBFRAME_FRAMES, SUBFRAME_STEREO_SAMPLES};

/// Maximum encoded size for a 2.5ms lossless subframe (1 byte tag + 480 bytes raw fallback)
pub const MAX_LOSSLESS_SUBFRAME_BYTES: usize = 481;

/// Exact byte size of a 2.5ms (120 stereo frames) IMA-ADPCM FEC backup frame:
/// 4 bytes state header (L_val, R_val) + 120 bytes (4-bit L + 4-bit R nibbles) = 124 bytes!
pub const ADPCM_FEC_BYTES: usize = 124;

const TAG_RAW_PCM16: u8 = 0x00;
const TAG_LPC_PACKED: u8 = 0x01;

/// Tag for a variable-bitrate compressed subframe (quality-profile byte follows).
///
/// Deliberately distinct from the lossless tags so a decoder can tell a VBR
/// payload apart from a legacy 124-byte IMA-ADPCM FEC frame without guessing.
pub const TAG_VBR_BFP: u8 = 0x7E;

/// Maximum encoded size of a VBR subframe (the 320 kbps profile is 94 bytes,
/// leaving headroom for future ladder rungs).
pub const MAX_VBR_SUBFRAME_BYTES: usize = 102;

/// User-selectable quality / stability profiles shared by host, clients and UIs.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityProfile {
    /// Dynamically shift between lossless and compressed based on link loss.
    SmartAuto = 0,
    /// 100% bit-perfect zero-lookahead LPC-delta lossless.
    StudioLossless = 1,
    /// Low-latency compressed VBR at the selected 96/160/256/320 kbps target.
    LowLatencyCompressed = 2,
    /// 10.0 ms (4 x 2.5 ms) framing for long-range 2.4 GHz links through walls.
    MaxStability24Ghz = 3,
}

impl QualityProfile {
    #[inline]
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::StudioLossless,
            2 => Self::LowLatencyCompressed,
            3 => Self::MaxStability24Ghz,
            _ => Self::SmartAuto,
        }
    }
}

/// One rung of the compressed VBR ladder.
///
/// `widths` is the repeating per-sample code width in bits.  Mixed patterns
/// let the codec land close to the advertised 160/256 kbps targets without
/// abandoning uniform quantization.  `block_len == 0` selects the 1-bit CVSD
/// profile, which carries no side information at all.
#[derive(Clone, Copy)]
struct VbrLadderEntry {
    widths: &'static [u8],
    block_len: usize,
}

/// Payload sizes (including the 2-byte tag/profile header) per 2.5 ms subframe:
/// 32 / 49 / 79 / 94 bytes, i.e. ~102 / 157 / 253 / 301 kbps of primary audio.
/// These are *nominal* targets: packet headers and the [N-1]/[N-2] FEC backups
/// add overhead on the wire, and the UI labels keep the familiar 96/160/256/320.
const VBR_LADDER: [VbrLadderEntry; 4] = [
    // 96 kbps: 1-bit CVSD, fully adaptive step, no side information.
    VbrLadderEntry {
        widths: &[1],
        block_len: 0,
    },
    // 160 kbps: alternating 1/2-bit codes (1.5 bits/sample average).
    VbrLadderEntry {
        widths: &[1, 2],
        block_len: SUBFRAME_FRAMES,
    },
    // 256 kbps: alternating 2/3-bit codes (2.5 bits/sample average).
    VbrLadderEntry {
        widths: &[2, 3],
        block_len: SUBFRAME_FRAMES,
    },
    // 320 kbps: 3-bit codes (~301 kbps primary payload).
    VbrLadderEntry {
        widths: &[3],
        block_len: SUBFRAME_FRAMES,
    },
];

#[inline(always)]
fn vbr_profile_index(bitrate_kbps: u16) -> usize {
    match bitrate_kbps {
        0..=127 => 0,
        128..=207 => 1,
        208..=287 => 2,
        _ => 3,
    }
}

/// Exact encoded byte length produced for a given target bitrate setting.
/// Useful for diagnostics, tests and receivers that pre-size buffers.
pub fn vbr_encoded_len(bitrate_kbps: u16) -> usize {
    let idx = vbr_profile_index(bitrate_kbps);
    let spec = VBR_LADDER[idx];
    let blocks_per_channel = SUBFRAME_FRAMES.checked_div(spec.block_len).unwrap_or(0);
    let payload_bits: usize = (0..SUBFRAME_STEREO_SAMPLES)
        .map(|i| spec.widths[i % spec.widths.len()] as usize)
        .sum();
    2 + 2 * blocks_per_channel + payload_bits.div_ceil(8)
}

#[inline(always)]
fn ceil_log2_u32(v: u32) -> u8 {
    if v <= 1 {
        0
    } else {
        (32 - (v - 1).leading_zeros()) as u8
    }
}

/// Choose a per-block binary exponent (step = 1 << shift) for the quantizer.
#[inline(always)]
fn choose_vbr_shift(peak: i32, max_width: u8) -> u8 {
    let peak = peak.max(1) as u32;
    if max_width <= 1 {
        // One-bit delta modulation tracks a slope of roughly half the peak.
        ceil_log2_u32(peak).saturating_sub(1).min(17)
    } else {
        // Mid-riser codes for `max_width` bits span +/- (2^(w-1) - 0.5) steps.
        let denominator = (1u32 << max_width) - 1;
        let needed_step = (2 * peak).div_ceil(denominator);
        ceil_log2_u32(needed_step.max(1)).min(17)
    }
}

#[inline(always)]
fn write_bits(buf: &mut [u8], bit_pos: &mut usize, value: u32, bits: u8) {
    for i in 0..bits as usize {
        let byte = *bit_pos >> 3;
        if byte >= buf.len() {
            return;
        }
        let mask = 1u8 << (*bit_pos & 7);
        if (value >> i) & 1 != 0 {
            buf[byte] |= mask;
        } else {
            buf[byte] &= !mask;
        }
        *bit_pos += 1;
    }
}

#[inline(always)]
fn read_bits(buf: &[u8], bit_pos: &mut usize, bits: u8) -> Option<u32> {
    let mut value = 0u32;
    for i in 0..bits as usize {
        let byte = *bit_pos >> 3;
        if byte >= buf.len() {
            return None;
        }
        value |= (((buf[byte] >> (*bit_pos & 7)) & 1) as u32) << i;
        *bit_pos += 1;
    }
    Some(value)
}

/// 1-bit continuously variable slope delta (CVSD) state.
///
/// Both encoder and decoder update the step from the transmitted bit history
/// only, so no side information is required and the stream is exactly 1 bit
/// per sample (96 kbps nominal at 48 kHz stereo).
struct CvsdState {
    step: i32,
    history: u8,
}

impl CvsdState {
    #[inline(always)]
    fn new() -> Self {
        Self {
            step: 32,
            history: 0b101,
        }
    }

    #[inline(always)]
    fn update_and_get_step(&mut self, bit: u8) -> i32 {
        self.history = ((self.history << 1) | (bit & 1)) & 0x07;
        if self.history == 0 || self.history == 0x07 {
            // Three identical bits in a row: slope overload, grow the step.
            self.step = (self.step + (self.step >> 2) + 1).min(16_384);
        } else {
            // Leaky decay keeps granular noise under control.
            self.step = (self.step - (self.step >> 4)).max(1);
        }
        self.step
    }
}

/// Encode a 2.5 ms stereo subframe with the user-selected VBR profile.
///
/// Layout: `[TAG_VBR_BFP][profile][block exponents...][packed q-bit codes...]`.
/// The codec has zero lookahead. 96 kbps uses 1-bit CVSD with no side info;
/// the higher rungs use per-block block-floating-point DPCM with mixed 1/2,
/// 2/3 or 3-bit codes to land near the advertised target rates.  Returns the
/// exact number of bytes written.
pub fn encode_vbr_subframe(
    pcm_stereo: &[i16; SUBFRAME_STEREO_SAMPLES],
    bitrate_kbps: u16,
    out: &mut [u8; MAX_LOSSLESS_SUBFRAME_BYTES],
) -> usize {
    let idx = vbr_profile_index(bitrate_kbps);
    let spec = VBR_LADDER[idx];
    let encoded_len = vbr_encoded_len(bitrate_kbps);

    out[0] = TAG_VBR_BFP;
    out[1] = idx as u8;

    if spec.block_len == 0 {
        // ---- 1-bit CVSD: no exponent side information ----
        let payload_start = 2usize;
        out[payload_start..encoded_len].fill(0);
        let mut bit_pos = payload_start * 8;

        for ch in 0..2 {
            let mut prev1 = 0i32;
            let mut prev2 = 0i32;
            let mut cvsd = CvsdState::new();

            for i in 0..SUBFRAME_FRAMES {
                let s = pcm_stereo[i * 2 + ch] as i32;
                let pred = (prev1 << 1) - prev2;
                let bit = u8::from(s >= pred);
                let step = cvsd.update_and_get_step(bit);
                let dequant = if bit == 1 { step } else { -step };
                let recon = (pred + dequant).clamp(-32_768, 32_767);

                write_bits(out, &mut bit_pos, bit as u32, 1);

                prev2 = prev1;
                prev1 = recon;
            }
        }
        return encoded_len;
    }

    // ---- Block-floating-point DPCM ----
    let blocks_per_channel = SUBFRAME_FRAMES.checked_div(spec.block_len).unwrap_or(0);
    let exponent_start = 2usize;
    let payload_start = exponent_start + 2 * blocks_per_channel;
    let max_width = *spec.widths.iter().max().unwrap_or(&1);

    // Pass 1: open-loop residual peaks -> one exponent byte per block/channel.
    for ch in 0..2 {
        let mut prev1 = 0i32;
        for block in 0..blocks_per_channel {
            let mut peak = 0i32;
            for k in 0..spec.block_len {
                let i = block * spec.block_len + k;
                let s = pcm_stereo[i * 2 + ch] as i32;
                peak = peak.max((s - prev1).abs());
                prev1 = s;
            }
            out[exponent_start + ch * blocks_per_channel + block] =
                choose_vbr_shift(peak, max_width);
        }
    }

    out[payload_start..encoded_len].fill(0);
    let mut bit_pos = payload_start * 8;

    // Pass 2: closed-loop quantization using reconstructed predictor state.
    for ch in 0..2 {
        let mut prev1 = 0i32;

        for block in 0..blocks_per_channel {
            let shift = out[exponent_start + ch * blocks_per_channel + block].min(17);
            let step = 1i32 << shift;

            for k in 0..spec.block_len {
                let i = block * spec.block_len + k;
                let width = spec.widths[i % spec.widths.len()];
                let s = pcm_stereo[i * 2 + ch] as i32;
                let residual = s - prev1;

                let half = 1i32 << (width - 1);
                let code = (residual.div_euclid(step) + half).clamp(0, (1i32 << width) - 1) as u32;
                let dequant = ((2 * code as i32 + 1 - (1i32 << width)) * step) >> 1;

                write_bits(out, &mut bit_pos, code, width);
                let recon = (prev1 + dequant).clamp(-32_768, 32_767);
                prev1 = recon;
            }
        }
    }

    encoded_len
}

/// Decode a VBR compressed subframe (96 / 160 / 256 / 320 kbps targets).
///
/// Returns `false` for a foreign tag, an unknown profile or a payload whose
/// length does not exactly match the profile (which also prevents a legacy
/// ADPCM FEC frame from being mistaken for a VBR frame).
pub fn decode_vbr_subframe_to_f32(
    encoded: &[u8],
    out_f32: &mut [f32; SUBFRAME_STEREO_SAMPLES],
) -> bool {
    if encoded.len() < 2 || encoded[0] != TAG_VBR_BFP {
        return false;
    }
    let idx = encoded[1] as usize;
    if idx >= VBR_LADDER.len() {
        return false;
    }
    let spec = VBR_LADDER[idx];
    let expected_len = {
        let blocks_per_channel = SUBFRAME_FRAMES.checked_div(spec.block_len).unwrap_or(0);
        let payload_bits: usize = (0..SUBFRAME_STEREO_SAMPLES)
            .map(|i| spec.widths[i % spec.widths.len()] as usize)
            .sum();
        2 + 2 * blocks_per_channel + payload_bits.div_ceil(8)
    };
    if encoded.len() != expected_len {
        return false;
    }

    if spec.block_len == 0 {
        // ---- 1-bit CVSD ----
        let mut bit_pos = 2usize * 8;
        for ch in 0..2 {
            let mut prev1 = 0i32;
            let mut prev2 = 0i32;
            let mut cvsd = CvsdState::new();

            for i in 0..SUBFRAME_FRAMES {
                let Some(bit) = read_bits(encoded, &mut bit_pos, 1) else {
                    return false;
                };
                let step = cvsd.update_and_get_step(bit as u8);
                let dequant = if bit == 1 { step } else { -step };
                let pred = (prev1 << 1) - prev2;
                let recon = (pred + dequant).clamp(-32_768, 32_767);

                out_f32[i * 2 + ch] = (recon as f32) * (1.0 / 32_768.0);
                prev2 = prev1;
                prev1 = recon;
            }
        }
        return true;
    }

    let blocks_per_channel = SUBFRAME_FRAMES.checked_div(spec.block_len).unwrap_or(0);
    let exponent_start = 2usize;
    let mut bit_pos = (exponent_start + 2 * blocks_per_channel) * 8;

    for ch in 0..2 {
        let mut prev1 = 0i32;

        for block in 0..blocks_per_channel {
            let shift = encoded[exponent_start + ch * blocks_per_channel + block].min(17);
            let step = 1i32 << shift;

            for k in 0..spec.block_len {
                let i = block * spec.block_len + k;
                let width = spec.widths[i % spec.widths.len()];
                let Some(code) = read_bits(encoded, &mut bit_pos, width) else {
                    return false;
                };
                let dequant = ((2 * code as i32 + 1 - (1i32 << width)) * step) >> 1;

                let recon = (prev1 + dequant).clamp(-32_768, 32_767);
                out_f32[i * 2 + ch] = (recon as f32) * (1.0 / 32_768.0);
                prev1 = recon;
            }
        }
    }

    true
}

#[inline(always)]
fn zigzag_encode(val: i32) -> u32 {
    ((val << 1) ^ (val >> 31)) as u32
}

#[inline(always)]
fn zigzag_decode(zz: u32) -> i32 {
    ((zz >> 1) as i32) ^ -((zz & 1) as i32)
}

/// Encode 2.5ms (240 interleaved stereo i16 samples) with 0.0ms lookahead and 100% bit-exact lossless reconstruction.
pub fn encode_lossless_subframe(
    pcm_stereo: &[i16; SUBFRAME_STEREO_SAMPLES],
    out: &mut [u8; MAX_LOSSLESS_SUBFRAME_BYTES],
) -> usize {
    let mut mid_zz = [0u32; SUBFRAME_FRAMES];
    let mut side_zz = [0u32; SUBFRAME_FRAMES];

    let mut m_prev1: i32 = 0;
    let mut m_prev2: i32 = 0;
    let mut s_prev1: i32 = 0;
    let mut s_prev2: i32 = 0;

    // 1. Reversible Mid-Side Decorrelation + 2nd-Order Causal LPC Prediction
    for i in 0..SUBFRAME_FRAMES {
        let l = pcm_stereo[i * 2] as i32;
        let r = pcm_stereo[i * 2 + 1] as i32;

        // Exact integer reversible Mid-Side transform
        let side = l - r;
        let mid = r + (side >> 1);

        let m_pred = match i {
            0 => 0,
            1 => m_prev1,
            _ => (m_prev1 << 1) - m_prev2,
        };
        let s_pred = match i {
            0 => 0,
            1 => s_prev1,
            _ => (s_prev1 << 1) - s_prev2,
        };

        mid_zz[i] = zigzag_encode(mid - m_pred);
        side_zz[i] = zigzag_encode(side - s_pred);

        m_prev2 = m_prev1;
        m_prev1 = mid;
        s_prev2 = s_prev1;
        s_prev1 = side;
    }

    // 2. Pack into 8-sample micro-blocks (15 blocks for Mid + 15 blocks for Side = 30 blocks)
    // If packed size >= 480 bytes (e.g., white noise), automatically fall back to raw PCM copy.
    out[0] = TAG_LPC_PACKED;
    let mut cursor = 1usize;

    for channel_zz in [&mid_zz, &side_zz] {
        for block in channel_zz.chunks_exact(8) {
            let max_v = block.iter().copied().max().unwrap_or(0);
            let bit_width = if max_v == 0 {
                0u8
            } else {
                (32 - max_v.leading_zeros()) as u8
            };

            let block_bytes = bit_width as usize; // 8 samples * bit_width bits / 8 = bit_width bytes!
                                                  // The u128 block accumulator holds at most 8 * 16 = 128 bits. Any
                                                  // wider residual block must use the raw PCM16 fallback.
            if bit_width > 16 || cursor + 1 + block_bytes >= 480 {
                // Incompressible noise: fall back to uncompressed raw PCM16 (481 bytes total)
                out[0] = TAG_RAW_PCM16;
                for (idx, &sample) in pcm_stereo.iter().enumerate() {
                    let le = sample.to_le_bytes();
                    out[1 + idx * 2] = le[0];
                    out[1 + idx * 2 + 1] = le[1];
                }
                return 481;
            }

            out[cursor] = bit_width;
            cursor += 1;

            if bit_width > 0 {
                let mut bit_acc: u128 = 0;
                for (k, &v) in block.iter().enumerate() {
                    bit_acc |= (v as u128) << (k * (bit_width as usize));
                }
                let bytes = bit_acc.to_le_bytes();
                out[cursor..cursor + block_bytes].copy_from_slice(&bytes[..block_bytes]);
                cursor += block_bytes;
            }
        }
    }

    cursor
}

/// Decode a lossless subframe back into 240 bit-exact stereo i16 samples.
pub fn decode_lossless_subframe(
    encoded: &[u8],
    out_pcm: &mut [i16; SUBFRAME_STEREO_SAMPLES],
) -> Result<(), &'static str> {
    if encoded.is_empty() {
        return Err("Empty lossless subframe");
    }

    if encoded[0] == TAG_RAW_PCM16 {
        if encoded.len() < 481 {
            return Err("Truncated raw PCM16 subframe");
        }
        for i in 0..SUBFRAME_STEREO_SAMPLES {
            out_pcm[i] = i16::from_le_bytes([encoded[1 + i * 2], encoded[2 + i * 2]]);
        }
        return Ok(());
    }

    if encoded[0] != TAG_LPC_PACKED {
        return Err("Unknown lossless subframe tag");
    }

    let mut mid_zz = [0u32; SUBFRAME_FRAMES];
    let mut side_zz = [0u32; SUBFRAME_FRAMES];
    let mut cursor = 1usize;

    for channel_zz in [&mut mid_zz, &mut side_zz] {
        for block in channel_zz.chunks_exact_mut(8) {
            if cursor >= encoded.len() {
                return Err("Truncated LPC block header");
            }
            let bit_width = encoded[cursor];
            cursor += 1;

            if bit_width == 0 {
                block.fill(0);
            } else {
                let block_bytes = bit_width as usize;
                if cursor + block_bytes > encoded.len() || block_bytes > 16 {
                    return Err("Truncated or invalid LPC block payload");
                }
                let mut raw16 = [0u8; 16];
                raw16[..block_bytes].copy_from_slice(&encoded[cursor..cursor + block_bytes]);
                cursor += block_bytes;

                let bit_acc = u128::from_le_bytes(raw16);
                let mask = (1u128 << bit_width) - 1;
                for (k, dst) in block.iter_mut().enumerate() {
                    *dst = ((bit_acc >> (k * (bit_width as usize))) & mask) as u32;
                }
            }
        }
    }

    // Reconstruct 2nd-Order LPC and inverse Mid-Side transform
    let mut m_prev1: i32 = 0;
    let mut m_prev2: i32 = 0;
    let mut s_prev1: i32 = 0;
    let mut s_prev2: i32 = 0;

    for i in 0..SUBFRAME_FRAMES {
        let m_pred = match i {
            0 => 0,
            1 => m_prev1,
            _ => (m_prev1 << 1) - m_prev2,
        };
        let s_pred = match i {
            0 => 0,
            1 => s_prev1,
            _ => (s_prev1 << 1) - s_prev2,
        };

        let mid = m_pred + zigzag_decode(mid_zz[i]);
        let side = s_pred + zigzag_decode(side_zz[i]);

        m_prev2 = m_prev1;
        m_prev1 = mid;
        s_prev2 = s_prev1;
        s_prev1 = side;

        let r = mid - (side >> 1);
        let l = r + side;

        out_pcm[i * 2] = l as i16;
        out_pcm[i * 2 + 1] = r as i16;
    }

    Ok(())
}

// =============================================================================
// Ultra-Fast Stereo IMA-ADPCM Codec for [N-1] and [N-2] Piggybacked FEC Backups
// =============================================================================

const IMA_INDEX_TABLE: [i8; 16] = [-1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8];
const IMA_STEP_TABLE: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449,
    494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272,
    2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493,
    10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];

#[inline(always)]
fn encode_adpcm_sample(sample: i16, pred: &mut i32, idx: &mut i8) -> u8 {
    let step = IMA_STEP_TABLE[*idx as usize];
    let mut diff = (sample as i32) - *pred;
    let mut nibble = 0u8;
    if diff < 0 {
        nibble = 8;
        diff = -diff;
    }
    let mut vpdiff = step >> 3;
    if diff >= step {
        nibble |= 4;
        diff -= step;
        vpdiff += step;
    }
    if diff >= (step >> 1) {
        nibble |= 2;
        diff -= step >> 1;
        vpdiff += step >> 1;
    }
    if diff >= (step >> 2) {
        nibble |= 1;
        vpdiff += step >> 2;
    }
    if (nibble & 8) != 0 {
        *pred = (*pred - vpdiff).clamp(-32768, 32767);
    } else {
        *pred = (*pred + vpdiff).clamp(-32768, 32767);
    }
    *idx = (*idx + IMA_INDEX_TABLE[nibble as usize]).clamp(0, 88);
    nibble
}

#[inline(always)]
fn decode_adpcm_sample(nibble: u8, pred: &mut i32, idx: &mut i8) -> i16 {
    let step = IMA_STEP_TABLE[*idx as usize];
    let mut vpdiff = step >> 3;
    if (nibble & 4) != 0 {
        vpdiff += step;
    }
    if (nibble & 2) != 0 {
        vpdiff += step >> 1;
    }
    if (nibble & 1) != 0 {
        vpdiff += step >> 2;
    }
    if (nibble & 8) != 0 {
        *pred = (*pred - vpdiff).clamp(-32768, 32767);
    } else {
        *pred = (*pred + vpdiff).clamp(-32768, 32767);
    }
    *idx = (*idx + IMA_INDEX_TABLE[(nibble & 0x0F) as usize]).clamp(0, 88);
    *pred as i16
}

/// Self-contained 124-byte FEC encoder for a 2.5ms stereo subframe
pub fn encode_fec_subframe(
    pcm_stereo: &[i16; SUBFRAME_STEREO_SAMPLES],
    out: &mut [u8; ADPCM_FEC_BYTES],
) {
    let mut pred_l = pcm_stereo[0] as i32;
    let mut pred_r = pcm_stereo[1] as i32;

    // Estimate initial signal slope to prevent ADPCM slope-overload distortion on frame start
    let diff_l = ((pcm_stereo[2] as i32) - pred_l).abs();
    let diff_r = ((pcm_stereo[3] as i32) - pred_r).abs();
    let init_idx = IMA_STEP_TABLE
        .iter()
        .position(|&s| s >= diff_l.max(diff_r).max(60))
        .unwrap_or(42) as i8;
    let mut idx_l = init_idx;
    let mut idx_r = init_idx;

    let l_tagged = (pcm_stereo[0] & !0x007F) | ((init_idx as i16) & 0x007F);
    out[0..2].copy_from_slice(&l_tagged.to_le_bytes());
    out[2..4].copy_from_slice(&pcm_stereo[1].to_le_bytes());
    pred_l = l_tagged as i32;

    for i in 0..SUBFRAME_FRAMES {
        let nib_l = encode_adpcm_sample(pcm_stereo[i * 2], &mut pred_l, &mut idx_l);
        let nib_r = encode_adpcm_sample(pcm_stereo[i * 2 + 1], &mut pred_r, &mut idx_r);
        out[4 + i] = (nib_l & 0x0F) | ((nib_r & 0x0F) << 4);
    }
}

/// Decode a 124-byte FEC backup frame into normalized f32 stereo samples
pub fn decode_fec_subframe_to_f32(
    fec_bytes: &[u8],
    out_f32: &mut [f32; SUBFRAME_STEREO_SAMPLES],
) -> bool {
    if fec_bytes.len() < ADPCM_FEC_BYTES {
        return false;
    }
    let l_tagged = i16::from_le_bytes([fec_bytes[0], fec_bytes[1]]);
    let init_idx = ((l_tagged & 0x007F) as i8).clamp(0, 88);
    let mut pred_l = l_tagged as i32;
    let mut pred_r = i16::from_le_bytes([fec_bytes[2], fec_bytes[3]]) as i32;
    let mut idx_l = init_idx;
    let mut idx_r = init_idx;

    for i in 0..SUBFRAME_FRAMES {
        let packed = fec_bytes[4 + i];
        let s_l = decode_adpcm_sample(packed & 0x0F, &mut pred_l, &mut idx_l);
        let s_r = decode_adpcm_sample(packed >> 4, &mut pred_r, &mut idx_r);
        out_f32[i * 2] = (s_l as f32) * (1.0 / 32768.0);
        out_f32[i * 2 + 1] = (s_r as f32) * (1.0 / 32768.0);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bit_exact_lossless_sine_and_silence() {
        let mut original = [0i16; SUBFRAME_STEREO_SAMPLES];
        for i in 0..SUBFRAME_FRAMES {
            let phase = (i as f32) * 2.0 * std::f32::consts::PI * 440.0 / 48_000.0;
            original[i * 2] = (phase.sin() * 28_000.0) as i16;
            original[i * 2 + 1] = ((phase + 0.2).sin() * 28_000.0) as i16;
        }

        let mut encoded = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
        let enc_len = encode_lossless_subframe(&original, &mut encoded);

        // Verify substantial lossless compression (< 240 bytes vs 480 bytes raw PCM!)
        assert!(enc_len < 240, "Expected < 240B, got {}B", enc_len);

        let mut decoded = [0i16; SUBFRAME_STEREO_SAMPLES];
        decode_lossless_subframe(&encoded[..enc_len], &mut decoded).unwrap();

        // 100% Bit-for-Bit Lossless verification
        assert_eq!(original, decoded);
    }

    #[test]
    fn test_lossless_extreme_full_scale_edges() {
        let mut extreme = [0i16; SUBFRAME_STEREO_SAMPLES];
        for i in 0..SUBFRAME_FRAMES {
            extreme[i * 2] = if i % 2 == 0 { i16::MIN } else { i16::MAX };
            extreme[i * 2 + 1] = if i % 3 == 0 { i16::MAX } else { i16::MIN };
        }

        let mut encoded = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
        let enc_len = encode_lossless_subframe(&extreme, &mut encoded);

        let mut decoded = [0i16; SUBFRAME_STEREO_SAMPLES];
        decode_lossless_subframe(&encoded[..enc_len], &mut decoded).unwrap();
        assert_eq!(extreme, decoded);
    }

    #[test]
    fn test_fec_adpcm_slope_tracking() {
        let mut pcm = [0i16; SUBFRAME_STEREO_SAMPLES];
        for i in 0..SUBFRAME_FRAMES {
            let phase = (i as f32) * 2.0 * std::f32::consts::PI * 1000.0 / 48_000.0;
            pcm[i * 2] = (phase.sin() * 20_000.0) as i16;
            pcm[i * 2 + 1] = (phase.cos() * 20_000.0) as i16;
        }

        let mut fec = [0u8; ADPCM_FEC_BYTES];
        encode_fec_subframe(&pcm, &mut fec);

        let mut decoded_f32 = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        assert!(decode_fec_subframe_to_f32(&fec, &mut decoded_f32));

        // Initial sample recovery must be highly accurate with slope tracking
        let orig_l = (pcm[0] as f32) / 32768.0;
        let orig_r = (pcm[1] as f32) / 32768.0;
        assert!((decoded_f32[0] - orig_l).abs() < 0.05);
        assert!((decoded_f32[1] - orig_r).abs() < 0.05);
    }

    fn make_sine(samples: &mut [i16; SUBFRAME_STEREO_SAMPLES]) {
        for i in 0..SUBFRAME_FRAMES {
            let phase = (i as f32) * 2.0 * std::f32::consts::PI * 440.0 / 48_000.0;
            samples[i * 2] = (phase.sin() * 24_000.0) as i16;
            samples[i * 2 + 1] = ((phase + 0.3).sin() * 24_000.0) as i16;
        }
    }

    fn signal_to_noise_db(
        original: &[i16; SUBFRAME_STEREO_SAMPLES],
        decoded: &[f32; SUBFRAME_STEREO_SAMPLES],
    ) -> f32 {
        let mut signal = 0.0f64;
        let mut error = 0.0f64;
        for i in 0..SUBFRAME_STEREO_SAMPLES {
            let s = original[i] as f64 / 32768.0;
            let e = s - decoded[i] as f64;
            signal += s * s;
            error += e * e;
        }
        10.0 * (signal / (error + 1e-12)).log10() as f32
    }

    #[test]
    fn test_vbr_profiles_have_exact_payload_sizes() {
        let mut pcm = [0i16; SUBFRAME_STEREO_SAMPLES];
        make_sine(&mut pcm);

        for (kbps, expected_len) in [(96u16, 32usize), (160, 49), (256, 79), (320, 94)] {
            let mut encoded = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
            let len = encode_vbr_subframe(&pcm, kbps, &mut encoded);
            assert_eq!(len, expected_len, "unexpected size at {} kbps", kbps);
            assert_eq!(vbr_encoded_len(kbps), expected_len);
            assert_eq!(encoded[0], TAG_VBR_BFP);
        }
    }

    #[test]
    fn test_vbr_roundtrip_all_profiles() {
        let mut pcm = [0i16; SUBFRAME_STEREO_SAMPLES];
        make_sine(&mut pcm);

        for kbps in [96u16, 160, 256, 300, 320] {
            let mut encoded = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
            let len = encode_vbr_subframe(&pcm, kbps, &mut encoded);
            assert_eq!(len, vbr_encoded_len(kbps));

            let mut decoded = [0.0f32; SUBFRAME_STEREO_SAMPLES];
            assert!(decode_vbr_subframe_to_f32(&encoded[..len], &mut decoded));

            let snr = signal_to_noise_db(&pcm, &decoded);
            assert!(snr.is_finite(), "non-finite SNR at {} kbps", kbps);
            assert!(
                snr > 3.0,
                "unexpectedly poor {} kbps SNR {:.1} dB",
                kbps,
                snr
            );
        }

        // The 320 kbps rung must be clearly better than the 96 kbps rung.
        let mut enc_low = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
        let low_len = encode_vbr_subframe(&pcm, 96, &mut enc_low);
        let mut dec_low = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        assert!(decode_vbr_subframe_to_f32(
            &enc_low[..low_len],
            &mut dec_low
        ));

        let mut enc_high = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
        let high_len = encode_vbr_subframe(&pcm, 320, &mut enc_high);
        let mut dec_high = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        assert!(decode_vbr_subframe_to_f32(
            &enc_high[..high_len],
            &mut dec_high
        ));

        let snr_low = signal_to_noise_db(&pcm, &dec_low);
        let snr_high = signal_to_noise_db(&pcm, &dec_high);
        assert!(
            snr_high > snr_low + 6.0,
            "320 kbps ({:.1} dB) should beat 96 kbps ({:.1} dB) by >= 6 dB",
            snr_high,
            snr_low
        );
    }

    #[test]
    fn test_vbr_rejects_corrupt_or_truncated_payloads() {
        let mut pcm = [0i16; SUBFRAME_STEREO_SAMPLES];
        make_sine(&mut pcm);
        let mut encoded = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
        let len = encode_vbr_subframe(&pcm, 320, &mut encoded);

        let mut decoded = [0.0f32; SUBFRAME_STEREO_SAMPLES];

        // Truncation, wrong tag, unknown profile and legacy ADPCM-length payloads
        // must all be rejected rather than producing noise.
        assert!(!decode_vbr_subframe_to_f32(
            &encoded[..len - 1],
            &mut decoded
        ));
        assert!(!decode_vbr_subframe_to_f32(&encoded[..0], &mut decoded));

        let mut wrong_tag = encoded;
        wrong_tag[0] = TAG_LPC_PACKED;
        assert!(!decode_vbr_subframe_to_f32(&wrong_tag[..len], &mut decoded));

        let mut bad_profile = encoded;
        bad_profile[1] = 9;
        assert!(!decode_vbr_subframe_to_f32(
            &bad_profile[..len],
            &mut decoded
        ));

        // A 124-byte IMA-ADPCM FEC frame must never be consumed as VBR.
        let mut fec = [0u8; ADPCM_FEC_BYTES];
        encode_fec_subframe(&pcm, &mut fec);
        assert!(!decode_vbr_subframe_to_f32(&fec, &mut decoded));
    }

    #[test]
    fn test_quality_profile_roundtrip() {
        for v in 0..=3u8 {
            assert_eq!(QualityProfile::from_u8(v) as u8, v);
        }
        assert_eq!(QualityProfile::from_u8(200), QualityProfile::SmartAuto);

        // Nearest-target bucketing of the four advertised bitrates.
        for (kbps, expected) in [
            (96u16, 32usize),
            (160, 49),
            (256, 79),
            (320, 94),
            (128, 49),
            (300, 94),
        ] {
            assert_eq!(vbr_encoded_len(kbps), expected);
        }
    }
}
