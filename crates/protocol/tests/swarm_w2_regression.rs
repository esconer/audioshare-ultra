// Permanent regression tests for swarm W2 (sequencer PLC + fade). Each test
// was a failing BEFORE repro and passes AFTER the fix.
use audioshare_protocol::sequencer::{ZeroJitterSequencer, STATUS_PRIMARY_LOSSLESS};
use audioshare_protocol::SUBFRAME_STEREO_SAMPLES;

fn energy(frame: &[f32; SUBFRAME_STEREO_SAMPLES]) -> f32 {
    frame.iter().map(|s| s * s).sum()
}

#[test]
fn repro_consecutive_plc_volume_collapse() {
    // Consecutive PLC frames must retain a usable level. The old code applied
    // 0.90^k on top of a history buffer that already held attenuated PLC
    // frames => 0.90^(k(k+1)/2), collapsing to silence within ~20 ms.
    let mut seq = ZeroJitterSequencer::new();
    let mut frame = [0.0f32; SUBFRAME_STEREO_SAMPLES];
    for i in 0..120 {
        let v = ((i as f32) * std::f32::consts::TAU * 300.0 / 48_000.0).sin() * 0.5;
        frame[i * 2] = v;
        frame[i * 2 + 1] = v;
    }
    assert!(seq.push_primary(0, &frame));
    let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
    assert_eq!(seq.pop_next(0, &mut out), STATUS_PRIMARY_LOSSLESS);

    let mut energies = Vec::new();
    for s in 1..=10u16 {
        let st = seq.pop_next(s, &mut out);
        debug_assert_ne!(st, STATUS_PRIMARY_LOSSLESS);
        energies.push(energy(&out));
    }
    // By PLC frame 10 the old code is < 1% of the first PLC frame.
    let ratio = energies[9] / energies[0].max(1e-9);
    println!("PLC energies: {:?}", energies);
    assert!(
        ratio > 0.10,
        "consecutive PLC volume collapsed: frame10/frame1 = {:.4}",
        ratio
    );
}

#[test]
fn repro_stream_start_has_no_click() {
    // A stream starting from -90 dBFS comfort dither to a full-scale first
    // frame must fade in over ~0.66 ms; the old code jumped instantly.
    let mut seq = ZeroJitterSequencer::new();
    let full = [0.8f32; SUBFRAME_STEREO_SAMPLES];
    let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
    assert!(seq.push_primary(0, &full));
    seq.pop_next(0, &mut out);
    assert!(
        out[0].abs() < 0.1,
        "first sample of a fresh stream must fade in from silence, got {}",
        out[0]
    );
    assert!(out[60].abs() > 0.7, "fade-in must complete within 0.66 ms");
}
