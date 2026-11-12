use audioshare_protocol::lpc_lossless::{
    decode_fec_subframe_to_f32, decode_vbr_subframe_to_f32, encode_fec_subframe,
    encode_vbr_subframe, vbr_encoded_len, ADPCM_FEC_BYTES, MAX_LOSSLESS_SUBFRAME_BYTES,
};
use audioshare_protocol::{SUBFRAME_FRAMES, SUBFRAME_STEREO_SAMPLES};

fn snr(orig: &[i16], dec: &[f32], start: usize) -> f64 {
    let mut signal = 0.0f64;
    let mut error = 0.0f64;
    for i in start..SUBFRAME_STEREO_SAMPLES {
        let s = orig[i] as f64 / 32768.0;
        let e = s - dec[i] as f64;
        signal += s * s;
        error += e * e;
    }
    10.0 * (signal / (error + 1e-18)).log10()
}

fn peak_err_lsb(orig: &[i16], dec: &[f32], end: usize) -> f64 {
    let mut peak = 0.0f64;
    for i in 0..end {
        peak = peak.max((orig[i] as f64 - dec[i] as f64 * 32768.0).abs());
    }
    peak
}

fn make_signal(kind: &str) -> [i16; SUBFRAME_STEREO_SAMPLES] {
    let mut pcm = [0i16; SUBFRAME_STEREO_SAMPLES];
    for i in 0..SUBFRAME_FRAMES {
        for ch in 0..2 {
            let phase = i as f64 * std::f64::consts::TAU * 1000.0 / 48_000.0 + ch as f64 * 0.2;
            let v = match kind {
                "sine_loud" => (phase.sin() * 22_000.0).round(),
                "quiet_100" => (phase.sin() * 100.0).round(),
                "dc_1020" => 1020.0,
                "dc_16384" => 16_384.0,
                "boundary_peak" => {
                    // 1 kHz sine starting at its positive peak: sample 0 and 1
                    // are nearly flat, but the true slope later is ~2600 LSB.
                    (std::f64::consts::FRAC_PI_2
                        + i as f64 * std::f64::consts::TAU * 1000.0 / 48_000.0)
                        .sin()
                        * 20_000.0
                }
                _ => 0.0,
            };
            pcm[i * 2 + ch] = v as i16;
        }
    }
    pcm
}

fn main() {
    println!("case,kbps,size,snr_all,snr_first24,peak_err_first16,first_sample_err");
    for case in [
        "sine_loud",
        "quiet_100",
        "dc_1020",
        "dc_16384",
        "boundary_peak",
    ] {
        let pcm = make_signal(case);
        for kbps in [96u16, 160, 256, 320] {
            let mut encoded = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
            let len = encode_vbr_subframe(&pcm, kbps, &mut encoded);
            let mut decoded = [0.0f32; SUBFRAME_STEREO_SAMPLES];
            assert!(decode_vbr_subframe_to_f32(&encoded[..len], &mut decoded));
            println!(
                "{},{},{},{:.2},{:.2},{:.1},{:.1}",
                case,
                kbps,
                vbr_encoded_len(kbps),
                snr(&pcm, &decoded, 0),
                snr(&pcm, &decoded, SUBFRAME_STEREO_SAMPLES - 216),
                peak_err_lsb(&pcm, &decoded, 16),
                (pcm[0] as f64 - decoded[0] as f64 * 32768.0).abs()
            );
        }
    }

    // IMA-ADPCM FEC: sample 0 sits at a 1 kHz peak, so |s1-s0| is small while
    // the real slope a few samples later is ~2600 LSB.
    let pcm = make_signal("boundary_peak");
    let mut fec = [0u8; ADPCM_FEC_BYTES];
    encode_fec_subframe(&pcm, &mut fec);
    let mut decoded = [0.0f32; SUBFRAME_STEREO_SAMPLES];
    assert!(decode_fec_subframe_to_f32(&fec, &mut decoded));
    println!(
        "fec_boundary_peak,124,124,{:.2},{:.2},{:.1},{:.1}",
        snr(&pcm, &decoded, 0),
        snr(&pcm, &decoded, SUBFRAME_STEREO_SAMPLES - 216),
        peak_err_lsb(&pcm, &decoded, 16),
        (pcm[0] as f64 - decoded[0] as f64 * 32768.0).abs()
    );
}
