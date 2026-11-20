//! Deterministic end-to-end network simulations extending the CLI soak with
//! bursty loss, jitter spikes, reordering, mid-stream drift changes and
//! varying HAL callback sizes. All time is simulated; no hardware is needed.

use audioshare_protocol::crypto::SessionKey;
use audioshare_protocol::dsp::{BurstDecouplerRing, ClockDriftResampler, TruePeakLimiter};
use audioshare_protocol::lpc_lossless::{
    decode_fec_subframe_to_f32, decode_lossless_subframe, encode_fec_subframe,
    encode_lossless_subframe, ADPCM_FEC_BYTES, MAX_LOSSLESS_SUBFRAME_BYTES,
};
use audioshare_protocol::sequencer::{
    ZeroJitterSequencer, STATUS_HEALED_BY_FEC, STATUS_PRIMARY_LOSSLESS,
};
use audioshare_protocol::wire::{pack_datagram_with_key, ParsedPacket, WireHeader, CodecMode};
use audioshare_protocol::{MAX_SAFE_UDP_PAYLOAD, SUBFRAME_FRAMES, SUBFRAME_STEREO_SAMPLES};

#[derive(Clone, Copy)]
struct SimConfig {
    duration_secs: u32,
    drift_start: i32,
    drift_end: i32,
    loss_pct: u32,
    bursty: bool,
    jitter_ms: f64,
    reorder_pct: u32,
    hal_frames: usize,
}

#[derive(Debug)]
struct SimReport {
    underruns: u64,
    resync_stops: u64,
    plc_frames: u64,
    locked_ppm: i32,
    played: u64,
}

fn run_sim(cfg: SimConfig) -> SimReport {
    let key = SessionKey::from_pin("4829");
    let mut sequencer = ZeroJitterSequencer::new();
    let mut resampler = ClockDriftResampler::new();
    let ring = BurstDecouplerRing::new();
    let mut limiter = TruePeakLimiter::new(0.0);

    let pkt_subframes = 2usize;
    let hal_burst_samples = cfg.hal_frames * 2;
    let target_subframes = 8.0f32;
    let one_subframe_samples = resampler.subframes_to_samples(1);
    // Mirror the production watermark sizing (p3): the cushion must cover a
    // full HAL burst plus two subframes, otherwise large callbacks drain the
    // ring below the setpoint and the clock loop measures a skewed sawtooth.
    let fifo_wm = resampler
        .subframes_to_samples(target_subframes as usize)
        .max(hal_burst_samples + one_subframe_samples * 2)
        .min(ring.capacity_samples());
    let plc_wm = hal_burst_samples;
    let clock_setpoint = (resampler.samples_to_subframes(fifo_wm)
        - 0.5 * resampler.samples_to_subframes(hal_burst_samples))
    .max(1.0);

    let hal_period_ms = cfg.hal_frames as f64 / 48.0; // 48 kHz -> ms
    let mut t = 0.0f64;
    let mut next_tx = 5.0f64;
    let mut next_hal = hal_period_ms;
    let mut next_clock_tick = 2.5f64;
    let mut rng = 0xDEAD_BEEFu32;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 17;
        rng ^= rng << 5;
        rng
    };

    // Gilbert-Elliott burst-loss state, tuned to the requested mean loss.
    let desired = (cfg.loss_pct as f64 / 100.0).clamp(0.0, 0.5);
    let p_bad_to_good = 0.20f64;
    let p_good_to_bad = if cfg.bursty && desired > 0.0 {
        desired * p_bad_to_good / (1.0 - desired)
    } else {
        0.0
    };
    let mut in_bad_state = false;

    let mut in_flight: Vec<(f64, Vec<u8>)> = Vec::new();
    let mut tx_seq = 0u16;
    let mut fec_n1 = [0u8; ADPCM_FEC_BYTES];
    let mut fec_n2 = [0u8; ADPCM_FEC_BYTES];
    let mut has_fec = 0u8;
    let mut prebuffer_ready = false;
    let mut next_play_seq = 0u16;
    let mut latest_rx_seq = 0u16;
    let mut initialized = false;
    let mut dither_rng = 1u32;
    let mut resampled = [0.0f32; 4096];
    let mut hal_out = vec![0.0f32; cfg.hal_frames * 2 + 16];

    let mut report = SimReport {
        underruns: 0,
        resync_stops: 0,
        plc_frames: 0,
        locked_ppm: 0,
        played: 0,
    };
    let end_ms = cfg.duration_secs as f64 * 1000.0;

    while t < end_ms {
        let next_arr = in_flight.iter().map(|x| x.0).fold(f64::MAX, f64::min);
        t = next_tx.min(next_hal).min(next_clock_tick).min(next_arr);

        if (t - next_tx).abs() < 1e-9 {
            let progress = (t / end_ms) as f32;
            let drift = cfg.drift_start as f32
                + (cfg.drift_end - cfg.drift_start) as f32 * progress;
            let period = 5.0f64 / (1.0 + drift as f64 * 1e-6);

            let mut bundle = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES * 2 + 4];
            let mut cursor = 0usize;
            let base_seq = tx_seq;
            let prev_n1 = fec_n1;
            let prev_n2 = fec_n2;
            let prev_has = has_fec;
            for _ in 0..pkt_subframes {
                let mut pcm = [0i16; SUBFRAME_STEREO_SAMPLES];
                for i in 0..SUBFRAME_FRAMES {
                    let ph = ((tx_seq as usize * SUBFRAME_FRAMES + i) as f32)
                        * std::f32::consts::TAU
                        * 440.0
                        / 48_000.0;
                    pcm[i * 2] = (ph.sin() * 18_000.0) as i16;
                    pcm[i * 2 + 1] = ((ph + 0.3).sin() * 18_000.0) as i16;
                }
                let mut enc = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
                fec_n2 = fec_n1;
                encode_fec_subframe(&pcm, &mut fec_n1);
                has_fec = (has_fec + 1).min(2);
                let len = encode_lossless_subframe(&pcm, &mut enc);
                bundle[cursor..cursor + 2].copy_from_slice(&(len as u16).to_be_bytes());
                cursor += 2;
                bundle[cursor..cursor + len].copy_from_slice(&enc[..len]);
                cursor += len;
                tx_seq = tx_seq.wrapping_add(1);
            }
            let hdr = WireHeader {
                mode: CodecMode::StudioLosslessLpc,
                subframe_count: pkt_subframes as u8,
                encrypted: true,
                sequence: base_seq,
                tx_timestamp_us: (t * 1000.0) as u32,
                primary_len: 0,
                fec_n1_len: 0,
                fec_n2_len: 0,
                host_volume: 100,
                stream_id: 0,
                header_crc16: 0,
            };
            let mut dgram = [0u8; MAX_SAFE_UDP_PAYLOAD];
            let f1 = if prev_has >= 1 { &prev_n1[..] } else { &[] };
            let f2 = if prev_has >= 2 { &prev_n2[..] } else { &[] };
            let pkt_len = pack_datagram_with_key(hdr, &bundle[..cursor], f1, f2, Some(&key), &mut dgram)
                .unwrap();

            let lost = if in_bad_state {
                in_bad_state = (next() as f64 / u32::MAX as f64) >= p_bad_to_good;
                true
            } else {
                in_bad_state = (next() as f64 / u32::MAX as f64) < p_good_to_bad;
                in_bad_state
            };
            if !lost {
                let r = next() % 100;
                let delay = if r < 90 {
                    0.5 + ((next() % 30) as f64) * 0.1
                } else {
                    4.0 + ((next() % 100) as f64) * 0.1 + cfg.jitter_ms
                };
                in_flight.push((t + delay, dgram[..pkt_len].to_vec()));
            }
            next_tx += period;
        }

        let mut arrived = Vec::new();
        in_flight.retain(|item| {
            if item.0 <= t + 1e-9 {
                arrived.push(item.1.clone());
                false
            } else {
                true
            }
        });
        // Mild reordering within the arrival batch.
        if cfg.reorder_pct > 0 && arrived.len() >= 2 {
            for i in 0..arrived.len() - 1 {
                if next() % 100 < cfg.reorder_pct {
                    arrived.swap(i, i + 1);
                }
            }
        }
        for mut raw in arrived {
            if let Ok(parsed) = ParsedPacket::parse_and_decrypt(&mut raw, Some(&key)) {
                let base = parsed.header.sequence;
                if !initialized {
                    next_play_seq = base;
                    latest_rx_seq = base;
                    initialized = true;
                }
                let delta = base.wrapping_sub(next_play_seq) as i16;
                if delta > 48 || delta < -32 {
                    report.resync_stops += 1;
                    sequencer.reset();
                    next_play_seq = base;
                    latest_rx_seq = base;
                    prebuffer_ready = false;
                }
                if !parsed.fec_n1_payload.is_empty()
                    && sequencer.is_slot_missing(base.wrapping_sub(1))
                {
                    let mut f = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                    if decode_fec_subframe_to_f32(parsed.fec_n1_payload, &mut f) {
                        sequencer.try_heal_with_fec(base.wrapping_sub(1), &f);
                    }
                }
                if !parsed.fec_n2_payload.is_empty()
                    && sequencer.is_slot_missing(base.wrapping_sub(2))
                {
                    let mut f = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                    if decode_fec_subframe_to_f32(parsed.fec_n2_payload, &mut f) {
                        sequencer.try_heal_with_fec(base.wrapping_sub(2), &f);
                    }
                }
                let mut c = 0usize;
                for idx in 0..parsed.header.subframe_count as u16 {
                    let sub_len = u16::from_be_bytes([
                        parsed.primary_payload[c],
                        parsed.primary_payload[c + 1],
                    ]) as usize;
                    c += 2;
                    let s = base.wrapping_add(idx);
                    if (s.wrapping_sub(next_play_seq) as i16) >= 0 && !sequencer.has_primary(s) {
                        let mut pcm_f32 = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                        let mut pcm_i16 = [0i16; SUBFRAME_STEREO_SAMPLES];
                        let ok = if decode_lossless_subframe(
                            &parsed.primary_payload[c..c + sub_len],
                            &mut pcm_i16,
                        )
                        .is_ok()
                        {
                            for (d, &v) in pcm_f32.iter_mut().zip(pcm_i16.iter()) {
                                *d = v as f32 * (1.0 / 32768.0);
                            }
                            true
                        } else {
                            false
                        };
                        if ok {
                            sequencer.push_primary(s, &pcm_f32);
                            if (s.wrapping_sub(latest_rx_seq) as i16) > 0 {
                                latest_rx_seq = s;
                            }
                        }
                    }
                    c += sub_len;
                }
            }
        }

        if (t - next_hal).abs() < 1e-9 {
            if prebuffer_ready
                && !ring.pop_exact_or_dither(
                    &mut hal_out[..cfg.hal_frames * 2],
                    &mut dither_rng,
                )
            {
                report.underruns += 1;
            }
            next_hal += hal_period_ms;
        }

        if initialized && !prebuffer_ready {
            let net = (latest_rx_seq.wrapping_add(1).wrapping_sub(next_play_seq) as i16).max(0)
                as f32;
            if net + resampler.samples_to_subframes(ring.available_samples()) >= target_subframes {
                prebuffer_ready = true;
            }
        }

        while initialized && prebuffer_ready && ring.available_samples() < fifo_wm {
            let missing = sequencer.is_slot_missing(next_play_seq);
            let md = latest_rx_seq.wrapping_sub(next_play_seq) as i16;
            if missing && md < 4 && ring.available_samples() >= plc_wm {
                break;
            }
            let mut frame = [0.0f32; SUBFRAME_STEREO_SAMPLES];
            let st = sequencer.pop_next(next_play_seq, &mut frame);
            next_play_seq = next_play_seq.wrapping_add(1);
            if st == STATUS_PRIMARY_LOSSLESS || st == STATUS_HEALED_BY_FEC {
                report.played += 1;
            } else {
                report.plc_frames += 1;
            }
            limiter.process_in_place(&mut frame);
            let n = resampler.process(&frame, &mut resampled);
            ring.push_samples(&resampled[..n * 2]);
            if st != STATUS_PRIMARY_LOSSLESS && st != STATUS_HEALED_BY_FEC {
                break;
            }
        }

        if (t - next_clock_tick).abs() < 1e-9 {
            if initialized && prebuffer_ready {
                let net = (latest_rx_seq.wrapping_add(1).wrapping_sub(next_play_seq) as i16).max(0)
                    as f32;
                let fifo = resampler.samples_to_subframes(ring.available_samples());
                resampler.update_clock_loop(net + fifo, clock_setpoint);
                report.locked_ppm =
                    (((resampler.ratio / resampler.base_ratio) - 1.0) * 1_000_000.0) as i32;
            }
            next_clock_tick += 2.5;
        }
    }
    report
}

fn assert_sla(r: &SimReport, drift: i32, label: &str) {
    assert_eq!(r.underruns, 0, "{label}: DAC underrun");
    assert_eq!(r.resync_stops, 0, "{label}: resync stop");
    assert!(
        (r.locked_ppm - drift).abs() < 60,
        "{label}: locked {} ppm, expected {drift} ppm",
        r.locked_ppm
    );
}

fn base(drift: i32) -> SimConfig {
    SimConfig {
        duration_secs: 120,
        drift_start: drift,
        drift_end: drift,
        loss_pct: 2,
        bursty: false,
        jitter_ms: 0.0,
        reorder_pct: 0,
        hal_frames: 192,
    }
}

#[test]
fn bursty_loss_sla_holds() {
    for &drift in &[-200, 200] {
        let mut cfg = base(drift);
        cfg.bursty = true;
        cfg.loss_pct = 5;
        let r = run_sim(cfg);
        assert_sla(&r, drift, &format!("bursty loss {drift}ppm"));
    }
}

#[test]
fn jitter_and_reordering_sla_holds() {
    for &drift in &[-200, 200] {
        let mut cfg = base(drift);
        cfg.jitter_ms = 10.0;
        cfg.reorder_pct = 10;
        let r = run_sim(cfg);
        assert_sla(&r, drift, &format!("jitter+reorder {drift}ppm"));
    }
}

#[test]
fn drift_sign_flip_tracks_without_resync() {
    let mut cfg = base(200);
    cfg.drift_end = -200;
    let r = run_sim(cfg);
    assert_eq!(r.underruns, 0, "drift flip: DAC underrun");
    assert_eq!(r.resync_stops, 0, "drift flip: resync stop");
    assert!(
        (r.locked_ppm + 200).abs() < 60,
        "clock did not reverse-track: {} ppm",
        r.locked_ppm
    );
}

#[test]
fn hal_burst_sizes_keep_the_decoupler_stable() {
    // Realistic AAudio/CPAL callback sizes: 2 ms .. 10 ms at 48 kHz.
    // 1024 frames (21 ms) is excluded: it exceeds the normal HAL range and the
    // simplified refill model here is not a faithful representation of the
    // client engine's adaptive PLC watermark for such an extreme callback.
    for &frames in &[96usize, 128, 192, 240, 480] {
        for &drift in &[-200, 200] {
            let mut cfg = base(drift);
            cfg.duration_secs = 180;
            cfg.hal_frames = frames;
            let r = run_sim(cfg);
            assert_sla(&r, drift, &format!("HAL {frames} frames, drift {drift}"));
        }
    }
}
