// Permanent regression tests for swarm W1 (protocol DSP). Each test was a failing
// BEFORE repro and passes AFTER the corresponding fix.
use audioshare_protocol::dsp::{ClockDriftResampler, TruePeakLimiter};

#[test]
fn repro_mute_gain_must_reach_exact_silence() {
    // Android sends -96 dB for Mute / headphone-unplug / call focus loss.
    // The limiter must converge to exactly 0.0 linear gain (digital silence).
    let mut limiter = TruePeakLimiter::new(0.0);
    limiter.set_gain_db(-96.0);
    let mut signal = [0.0f32; 1000];
    for _ in 0..300 {
        signal.fill(1.0); // fresh constant input each pass (avoid in-place feedback)
        limiter.process_in_place(&mut signal);
    }
    assert!(
        signal.iter().all(|s| s.abs() < 1e-6),
        "mute leaked audio: max |s| = {} (expected digital silence)",
        signal.iter().map(|s| s.abs()).fold(0.0f32, f32::max)
    );
}

#[test]
fn repro_transient_step_must_not_hard_clip() {
    // +20 dB preamp, 0.5 -> 5.0 step. First sample must stay strictly below
    // the -0.1 dBFS ceiling BEFORE the final clamp; dividing by the lagging
    // envelope instead of envelope.max(peak) slams it into hard clipping.
    let mut limiter = TruePeakLimiter::new(20.0);
    let mut signal = [0.5f32; 240];
    limiter.process_in_place(&mut signal);
    assert!(
        signal[0] < 0.988,
        "first transient sample hard-clipped to ceiling: {}",
        signal[0]
    );
}

#[test]
fn repro_integrator_must_hold_crystal_offset_inside_deadband() {
    // Real drift locks with a residual error inside the +/-0.15 subframe
    // deadband. The current `*= 0.9` decay erases the learned crystal offset
    // every 500 ms, so the learned correction must NOT collapse to near zero.
    let mut resampler = ClockDriftResampler::new();
    for _ in 0..4000 {
        resampler.update_clock_loop(2.6, 2.0); // 0.6 err -> builds integrator
    }
    let locked = (resampler.ratio / resampler.base_ratio - 1.0) * 1e6;
    for _ in 0..4000 {
        resampler.update_clock_loop(2.05, 2.0); // 0.05 err -> deadband
    }
    let after = (resampler.ratio / resampler.base_ratio - 1.0) * 1e6;
    assert!(
        after > 30.0,
        "integrator decay erased learned crystal offset: locked={:.1}ppm after_deadband={:.1}ppm",
        locked,
        after
    );
}

#[test]
fn repro_integrator_authority_must_exceed_50ppm() {
    // +/-10 integral * 5e-6 = +/-50 ppm max integral authority. A sustained
    // moderate error (0.6 subframes) must be able to drive correction well
    // beyond the ~80 ppm the old P+I path caps at, to track real +-300ppm crystals.
    let mut resampler = ClockDriftResampler::new();
    for _ in 0..20000 {
        resampler.update_clock_loop(2.6, 2.0); // sustained 0.6 err
    }
    let corr = (resampler.ratio / resampler.base_ratio - 1.0).abs() * 1e6;
    assert!(
        corr > 150.0,
        "correction capped near 80ppm (old integral authority): {:.1} ppm",
        corr
    );
}
