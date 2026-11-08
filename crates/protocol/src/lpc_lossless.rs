use crate::{SUBFRAME_FRAMES, SUBFRAME_STEREO_SAMPLES};

/// Maximum encoded size for a 2.5ms lossless subframe (1 byte tag + 480 bytes raw fallback)
pub const MAX_LOSSLESS_SUBFRAME_BYTES: usize = 481;

/// Exact byte size of a 2.5ms (120 stereo frames) IMA-ADPCM FEC backup frame:
/// 4 bytes state header (L_val, R_val) + 120 bytes (4-bit L + 4-bit R nibbles) = 124 bytes!
pub const ADPCM_FEC_BYTES: usize = 124;

const TAG_RAW_PCM16: u8 = 0x00;
const TAG_LPC_PACKED: u8 = 0x01;

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
}
