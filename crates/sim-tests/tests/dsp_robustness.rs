//! DSP robustness simulations: non-finite input poisoning, limiter smoothness
//! across a preamp sweep, and decoupler underrun behaviour.

use audioshare_protocol::dsp::{BurstDecouplerRing, TruePeakLimiter};
use audioshare_protocol::sync_and_eq::{
    build_qr_connect_uri, BauerCrossfeedFilter, MicRumbleAndNoiseGate, NightModeCompressor,
    StudioEqualizer,
};

#[test]
fn single_nan_does_not_permanently_mute_the_dsp_chain() {
    let mut eq = StudioEqualizer::new();
    eq.update_if_changed(60, 40, -30);
    let mut xfeed = BauerCrossfeedFilter::new();
    let mut night = NightModeCompressor::new();
    let mut mic = MicRumbleAndNoiseGate::new();

    // Inject several kinds of non-finite samples at different positions.
    let mut bad = [f32::NAN, f32::INFINITY, 0.25, f32::NEG_INFINITY];
    for _ in 0..8 {
        eq.process(&mut bad);
        xfeed.process_in_place(&mut bad);
        night.process_in_place(&mut bad);
        mic.process_in_place(&mut bad);
        assert!(bad.iter().all(|s| s.is_finite()), "filter state poisoned");
    }

    // A subsequent clean signal must still pass through (no permanent silence).
    let mut good = [0.35f32, -0.35, 0.35, -0.35];
    for _ in 0..16 {
        eq.process(&mut good);
        xfeed.process_in_place(&mut good);
        night.process_in_place(&mut good);
        mic.process_in_place(&mut good);
    }
    assert!(good.iter().all(|s| s.is_finite()));
    assert!(
        good[2].abs() > 0.01,
        "DSP chain stayed muted after non-finite input"
    );
}

#[test]
fn limiter_is_continuous_across_full_preamp_sweep() {
    // Sweep gain through the 0 dB boundary in tiny steps; at a fixed input
    // just above the old knee the output must change smoothly, not jump.
    let input = 0.90f32;
    let mut previous: Option<f32> = None;
    let mut max_step = 0.0f32;

    for step in 0..=100 {
        let gain_db = -1.0 + (step as f32) * 0.02; // -1.0 .. +1.0 dB
        let mut limiter = TruePeakLimiter::new(gain_db);
        let mut frame = [input, -input];
        for _ in 0..4 {
            limiter.process_in_place(&mut frame);
        }
        let out = frame[0].abs();
        assert!(out.is_finite(), "non-finite limiter output at {gain_db} dB");
        assert!(out <= 0.99, "limiter exceeded ceiling at {gain_db} dB");
        if let Some(prev) = previous {
            max_step = max_step.max((out - prev).abs());
        }
        previous = Some(out);
    }
    // A 0.02 dB gain step should move the result only marginally; a hard
    // knee switch would produce a ~0.015+ amplitude jump here.
    assert!(
        max_step < 0.01,
        "limiter knee discontinuity detected: max step {max_step}"
    );
}

#[test]
fn decoupler_underrun_emits_only_keepalive_dither() {
    let ring = BurstDecouplerRing::new();
    let mut out = [0.5f32; 384];
    let mut rng = 1u32;
    assert!(!ring.pop_exact_or_dither(&mut out, &mut rng));
    // No stale full-scale audio may survive; the only permitted residue is
    // the intentional -90 dBFS (~3.2e-5) Bluetooth keep-alive dither.
    assert!(
        out.iter().all(|s| s.abs() < 1e-4),
        "underrun path left stale audio in the output buffer"
    );
    assert!(
        out.iter().any(|s| *s != 0.0),
        "keep-alive dither missing — BT earbuds could fall asleep"
    );
}

#[test]
fn qr_uri_handles_special_characters_and_ipv6() {
    assert_eq!(
        build_qr_connect_uri("192.168.1.50", 65530, Some("a&b#c+1 2")),
        "asultra://connect?host=192.168.1.50:65530&pin=a%26b%23c%2B1%202"
    );
    assert_eq!(
        build_qr_connect_uri("fe80::1", 65530, Some("4829")),
        "asultra://connect?host=[fe80::1]:65530&pin=4829"
    );
    assert_eq!(
        build_qr_connect_uri("fe80::1", 65530, None),
        "asultra://connect?host=[fe80::1]:65530"
    );
    // Every RFC 3986 unreserved character must survive unchanged.
    assert_eq!(
        build_qr_connect_uri("10.0.0.1", 65530, Some("AZaz09-._~")),
        "asultra://connect?host=10.0.0.1:65530&pin=AZaz09-._~"
    );
}
