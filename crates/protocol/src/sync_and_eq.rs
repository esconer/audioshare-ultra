//! Microsecond Group Sync, Spatial Channel Routing, Audiophile Bauer Crossfeed & Studio EQ.

/// 1. Spatial Channel Role for Multi-Phone Stereo Pairing & 5.1/7.1 Rear Surround
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelRole {
    /// Normal Stereo (L, R)
    Stereo = 0,
    /// Left Speaker Only (Both speakers play Left channel for 2-phone physical stereo pair)
    LeftMono = 1,
    /// Right Speaker Only (Both speakers play Right channel)
    RightMono = 2,
    /// Center Vocal / Dialogue Extraction: Mid = (L + R) * 0.5
    CenterDialogue = 3,
    /// Side/Surround Difference Extraction: Side = (L - R) * 0.5
    SurroundAmbient = 4,
}

impl ChannelRole {
    pub fn from_u8(val: u8) -> Self {
        match val {
            1 => Self::LeftMono,
            2 => Self::RightMono,
            3 => Self::CenterDialogue,
            4 => Self::SurroundAmbient,
            _ => Self::Stereo,
        }
    }

    #[inline]
    pub fn apply_in_place(self, stereo_samples: &mut [f32]) {
        if self == Self::Stereo {
            return;
        }
        for frame in stereo_samples.chunks_exact_mut(2) {
            let l = frame[0];
            let r = frame[1];
            match self {
                Self::Stereo => {}
                Self::LeftMono => {
                    frame[0] = l;
                    frame[1] = l;
                }
                Self::RightMono => {
                    frame[0] = r;
                    frame[1] = r;
                }
                Self::CenterDialogue => {
                    // -6 dB center law. A coherent center (L == R == 1.0)
                    // previously became 1.414 on both speakers because the
                    // 0.7071 constant-power factor doubled the sum. 0.5 keeps
                    // full-scale center content at full scale.
                    let mid = (l + r) * 0.5;
                    frame[0] = mid;
                    frame[1] = mid;
                }
                Self::SurroundAmbient => {
                    // Also -6 dB: anti-phase input (L = -R = 1.0) produced
                    // +/-1.414 and clipped. Side extraction remains exactly
                    // anti-phase between the ears by design; scaling it down
                    // only fixes the overload.
                    let side = (l - r) * 0.5;
                    frame[0] = side;
                    frame[1] = -side;
                }
            }
        }
    }
}

/// 2. Audiophile Bauer Stereophonic-to-Binaural Headphone Crossfeed (0.00ms Lookahead!)
/// Simulates studio monitor speakers at +/- 30 degrees by feeding a 270us (13-sample @ 48kHz)
/// delayed, 700Hz low-pass filtered cross-signal into the opposite ear.
pub struct BauerCrossfeedFilter {
    delay_l: [f32; 16],
    delay_r: [f32; 16],
    idx: usize,
    lp_l: f32,
    lp_r: f32,
}

impl BauerCrossfeedFilter {
    pub const fn new() -> Self {
        Self {
            delay_l: [0.0; 16],
            delay_r: [0.0; 16],
            idx: 0,
            lp_l: 0.0,
            lp_r: 0.0,
        }
    }

    #[inline]
    pub fn process_in_place(&mut self, stereo: &mut [f32]) {
        const ANTI_DENORMAL: f32 = 1.0e-25;
        // 700 Hz 1-pole low-pass coefficient at 48kHz: 1 - exp(-2*pi*700/48000)
        const ALPHA_700HZ: f32 = 0.0875;
        const DIRECT_GAIN: f32 = 0.78;
        const CROSS_GAIN: f32 = 0.22; // DIRECT_GAIN + CROSS_GAIN == 1.00 (0.0 dB unity center)
        const ITD_SAMPLES: usize = 7; // 146 us ITD (+/-30 deg studio monitor angle, no 1.8kHz notch)

        for frame in stereo.chunks_exact_mut(2) {
            let in_l = frame[0] + ANTI_DENORMAL;
            let in_r = frame[1] + ANTI_DENORMAL;

            // Low-pass filter current samples (head shadow simulation)
            self.lp_l += ALPHA_700HZ * (in_l - self.lp_l);
            self.lp_r += ALPHA_700HZ * (in_r - self.lp_r);

            self.delay_l[self.idx] = self.lp_l;
            self.delay_r[self.idx] = self.lp_r;

            let delayed_idx = (self.idx + 16 - ITD_SAMPLES) & 15;
            let cross_from_l = self.delay_l[delayed_idx];
            let cross_from_r = self.delay_r[delayed_idx];

            // Linkwitz/Bauer all-pass shelf: preserve high-frequency direct
            // energy `(in - lp) * CROSS_GAIN` so centered vocals stay flat.
            // The residual 1-pole low-pass at 10 kHz is ~7.5%, leaving
            // -0.21 dB instead of the old -1.41 dB treble loss.
            let hf_l = in_l - self.lp_l;
            let hf_r = in_r - self.lp_r;
            frame[0] = in_l * DIRECT_GAIN + (hf_l + cross_from_r) * CROSS_GAIN;
            frame[1] = in_r * DIRECT_GAIN + (hf_r + cross_from_l) * CROSS_GAIN;

            self.idx = (self.idx + 1) & 15;
        }
    }
}

/// 3. NTP/PTP 4-Timestamp Clock Offset & Multi-Phone Group Sync Estimator
pub struct GroupSyncEstimator {
    pub clock_offset_us: i64,
    pub min_rtt_us: u32,
}

impl GroupSyncEstimator {
    pub const fn new() -> Self {
        Self {
            clock_offset_us: 0,
            min_rtt_us: u32::MAX,
        }
    }

    #[inline]
    pub fn record_ptp_exchange(&mut self, t1: i64, t2: i64, t3: i64, t4: i64) {
        let rtt = ((t4 - t1) - (t3 - t2)).max(0) as u32;
        let offset = ((t2 - t1) + (t3 - t4)) / 2;

        if rtt <= self.min_rtt_us.saturating_add(1500) {
            if rtt < self.min_rtt_us {
                self.min_rtt_us = rtt;
            }
            if self.clock_offset_us == 0 {
                self.clock_offset_us = offset;
            } else {
                self.clock_offset_us = (self.clock_offset_us * 7 + offset) / 8;
            }
        } else {
            self.min_rtt_us = self.min_rtt_us.saturating_add(50);
        }
    }

    #[inline]
    pub fn aligned_target_subframes(
        &self,
        base_target: f32,
        max_group_delay_ms: u16,
        my_delay_ms: u16,
    ) -> f32 {
        let extra_ms = max_group_delay_ms.saturating_sub(my_delay_ms) as f32;
        let extra_subframes = (extra_ms / 2.5).clamp(0.0, 40.0);
        base_target + extra_subframes
    }
}

/// 4. Zero-Allocation Direct-Form II Transposed Stereo Biquad Filter
#[derive(Clone, Copy)]
pub struct StereoBiquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1_l: f32,
    z2_l: f32,
    z1_r: f32,
    z2_r: f32,
}

impl StereoBiquad {
    pub const fn passthrough() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            z1_l: 0.0,
            z2_l: 0.0,
            z1_r: 0.0,
            z2_r: 0.0,
        }
    }

    pub fn set_peaking_eq(&mut self, freq_hz: f32, q: f32, gain_db: f32) {
        if gain_db.abs() < 0.05 {
            self.b0 = 1.0;
            self.b1 = 0.0;
            self.b2 = 0.0;
            self.a1 = 0.0;
            self.a2 = 0.0;
            // `StudioEqualizer::process` bypasses all biquads while every band
            // is flat, so these delay states freeze instead of decaying. Clear
            // them here or re-engaging the band discharges the old history as
            // a click on the first frame.
            self.z1_l = 0.0;
            self.z2_l = 0.0;
            self.z1_r = 0.0;
            self.z2_r = 0.0;
            return;
        }
        let a = 10.0f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * freq_hz / 48_000.0;
        let cos_w0 = w0.cos();
        let alpha = w0.sin() / (2.0 * q);

        let b0 = 1.0 + alpha * a;
        let b1 = -2.0 * cos_w0;
        let b2 = 1.0 - alpha * a;
        let a0 = 1.0 + alpha / a;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha / a;

        let inv_a0 = 1.0 / a0;
        self.b0 = b0 * inv_a0;
        self.b1 = b1 * inv_a0;
        self.b2 = b2 * inv_a0;
        self.a1 = a1 * inv_a0;
        self.a2 = a2 * inv_a0;
    }

    #[inline]
    pub fn process_stereo_in_place(&mut self, stereo: &mut [f32]) {
        const ANTI_DENORMAL: f32 = 1.0e-25;
        for frame in stereo.chunks_exact_mut(2) {
            let in_l = frame[0] + ANTI_DENORMAL;
            let out_l = self.b0 * in_l + self.z1_l;
            self.z1_l = self.b1 * in_l - self.a1 * out_l + self.z2_l;
            self.z2_l = self.b2 * in_l - self.a2 * out_l;
            frame[0] = out_l;

            let in_r = frame[1] + ANTI_DENORMAL;
            let out_r = self.b0 * in_r + self.z1_r;
            self.z1_r = self.b1 * in_r - self.a1 * out_r + self.z2_r;
            self.z2_r = self.b2 * in_r - self.a2 * out_r;
            frame[1] = out_r;
        }
    }
}

/// 5. 3-Band Studio Parametric Equalizer (Bass 90Hz, Vocal Presence 2.5kHz, Treble Air 10kHz)
pub struct StudioEqualizer {
    pub bass: StereoBiquad,
    pub presence: StereoBiquad,
    pub treble: StereoBiquad,
    last_bass_tenths: i32,
    last_pres_tenths: i32,
    last_treb_tenths: i32,
}

impl StudioEqualizer {
    pub const fn new() -> Self {
        Self {
            bass: StereoBiquad::passthrough(),
            presence: StereoBiquad::passthrough(),
            treble: StereoBiquad::passthrough(),
            last_bass_tenths: 0,
            last_pres_tenths: 0,
            last_treb_tenths: 0,
        }
    }

    #[inline]
    pub fn update_if_changed(&mut self, bass_tenths: i32, pres_tenths: i32, treb_tenths: i32) {
        if bass_tenths != self.last_bass_tenths
            || pres_tenths != self.last_pres_tenths
            || treb_tenths != self.last_treb_tenths
        {
            self.last_bass_tenths = bass_tenths;
            self.last_pres_tenths = pres_tenths;
            self.last_treb_tenths = treb_tenths;
            self.bass
                .set_peaking_eq(90.0, 0.707, (bass_tenths as f32) / 10.0);
            self.presence
                .set_peaking_eq(2500.0, 1.0, (pres_tenths as f32) / 10.0);
            self.treble
                .set_peaking_eq(10000.0, 0.707, (treb_tenths as f32) / 10.0);
        }
    }

    #[inline]
    pub fn process(&mut self, stereo: &mut [f32]) {
        if self.last_bass_tenths == 0 && self.last_pres_tenths == 0 && self.last_treb_tenths == 0 {
            return;
        }
        self.bass.process_stereo_in_place(stereo);
        self.presence.process_stereo_in_place(stereo);
        self.treble.process_stereo_in_place(stereo);
    }
}

/// 6. Zero-Latency "Night Mode" Upward Whisper Compressor & Explosion Tamer (0.00ms Lookahead)
/// - Boosts quiet movie dialogue & game footsteps (-49 dBFS .. -18 dBFS) by up to +10.9 dB
/// - Tames sudden loud explosions/gunshots above -11 dBFS with a 4:1 ratio
/// - Noise-gates signals below -64 dBFS so background hiss is never amplified
///
/// The transfer curve is strictly monotonic: every zone's output level
/// `level * gain(level)` rises with the input level, so louder dialogue never
/// becomes quieter. The old curve fell from 0.2955 to 0.250 between -16 and
/// -12 dBFS, folding speech peaks inward.
const NIGHT_GATE_START: f32 = 0.0006; // -64.4 dBFS: unity below this floor
const NIGHT_NOISE_FLOOR: f32 = 0.0035; // -49.1 dBFS: full whisper boost
const NIGHT_WHISPER_ZONE: f32 = 0.12; // -18.4 dBFS
const NIGHT_LOUD_THRESH: f32 = 0.28; // -11.1 dBFS
const NIGHT_MAX_BOOST: f32 = 3.50; // +10.9 dB
const NIGHT_WHISPER_GAIN: f32 = 2.00;
/// Gain at the downward-compression knee. 1.35 (not 1.15) keeps
/// `d(level * gain)/d(level)` positive all the way to the knee.
const NIGHT_LOUD_GAIN: f32 = 1.35;

/// Target night-mode gain for a peak envelope level.
///
/// Below the gate the signal passes at unity. A smoothstep ramps the gain up
/// to `NIGHT_MAX_BOOST`, then two linear segments hand over to the 4:1
/// downward compressor. The control points are chosen so the output level is
/// monotonic at every boundary; the report's 2.00 -> 1.15 segment still had a
/// negative output slope near 0.25 and was corrected to 1.35.
#[inline(always)]
fn night_mode_target_gain(level: f32) -> f32 {
    if level <= NIGHT_GATE_START {
        1.0
    } else if level < NIGHT_NOISE_FLOOR {
        let t = (level - NIGHT_GATE_START) / (NIGHT_NOISE_FLOOR - NIGHT_GATE_START);
        let smooth = t * t * (3.0 - 2.0 * t);
        1.0 + (NIGHT_MAX_BOOST - 1.0) * smooth
    } else if level < NIGHT_WHISPER_ZONE {
        let t = (level - NIGHT_NOISE_FLOOR) / (NIGHT_WHISPER_ZONE - NIGHT_NOISE_FLOOR);
        NIGHT_MAX_BOOST - (NIGHT_MAX_BOOST - NIGHT_WHISPER_GAIN) * t
    } else if level <= NIGHT_LOUD_THRESH {
        let t = (level - NIGHT_WHISPER_ZONE) / (NIGHT_LOUD_THRESH - NIGHT_WHISPER_ZONE);
        NIGHT_WHISPER_GAIN - (NIGHT_WHISPER_GAIN - NIGHT_LOUD_GAIN) * t
    } else {
        let target = NIGHT_LOUD_THRESH * NIGHT_LOUD_GAIN + (level - NIGHT_LOUD_THRESH) * 0.25;
        target / level
    }
}

pub struct NightModeCompressor {
    env: f32,
    /// Smoothed gain actually applied to the samples. Reductions move quickly
    /// so a transient cannot overshoot; recovery is slow to avoid pumping.
    gain_env: f32,
}

impl NightModeCompressor {
    pub const fn new() -> Self {
        Self {
            env: 0.0,
            gain_env: 1.0,
        }
    }

    #[inline]
    pub fn process_in_place(&mut self, stereo: &mut [f32]) {
        for frame in stereo.chunks_exact_mut(2) {
            let l = frame[0];
            let r = frame[1];
            let peak = l.abs().max(r.abs()) + 1.0e-25;

            // 1.5 ms attack, 100 ms release. The old 93 us attack / 26 ms
            // release tracked 60-200 Hz waveform cycles and modulated their
            // gain, adding harmonics.
            if peak > self.env {
                self.env += 0.014 * (peak - self.env);
            } else {
                self.env += 0.0002 * (peak - self.env);
            }

            // Feed-forward peak: a transient on top of a quiet envelope must
            // be compressed immediately instead of receiving whisper boost
            // on its leading edge.
            let level = self.env.max(peak);
            let target_gain = night_mode_target_gain(level);

            // Reach 75% of a downward target in two samples, but recover
            // slowly enough not to pump. This also smooths the residual
            // envelope ripple that used to distort bass.
            let coeff = if target_gain < self.gain_env {
                0.5
            } else {
                0.001
            };
            self.gain_env += coeff * (target_gain - self.gain_env);

            frame[0] = (l * self.gain_env).clamp(-0.84, 0.84);
            frame[1] = (r * self.gain_env).clamp(-0.84, 0.84);
        }
    }
}

/// 7. 1-Click Acoustic Sonar Chirp Generator & Detector (Measures True Glass-to-Glass Latency)
pub struct LatencySonar;

impl LatencySonar {
    /// Injects a 2.5ms (120-frame) 3.5kHz -> 7.0kHz swept sonar pulse into a stereo subframe
    #[inline]
    pub fn inject_chirp(stereo_subframe: &mut [f32; 240]) {
        for i in 0..120 {
            let t = (i as f32) / 120.0;
            // Hann window envelope so the chirp has zero click artifacts
            let window = 0.5 * (1.0 - (2.0 * std::f32::consts::PI * t).cos());
            let freq = 3500.0 + 3500.0 * t;
            let sample =
                ((i as f32) * 2.0 * std::f32::consts::PI * freq / 48_000.0).sin() * window * 0.80;
            stereo_subframe[i * 2] = sample;
            stereo_subframe[i * 2 + 1] = sample;
        }
    }

    /// Detects the returning high-frequency sonar chirp in a captured microphone/loopback subframe
    #[inline]
    pub fn detect_chirp_return(stereo_subframe: &[f32; 240]) -> bool {
        let mut zero_crossings = 0u32;
        let mut max_peak = 0.0f32;
        let mut prev = stereo_subframe[0];
        for i in 1..120 {
            let cur = stereo_subframe[i * 2];
            if cur.abs() > max_peak {
                max_peak = cur.abs();
            }
            if (cur >= 0.0 && prev < 0.0) || (cur < 0.0 && prev >= 0.0) {
                zero_crossings += 1;
            }
            prev = cur;
        }
        // A full 2.5ms chirp has ~34 zero crossings, but a real acoustic
        // return almost always straddles two capture subframes. The larger
        // half then has 12-22 crossings, so the old >=20 threshold missed
        // most split returns. Keep a peak floor so low-level noise cannot
        // trigger the measurement.
        max_peak > 0.12 && zero_crossings >= 12
    }
}

/// 8. 1st-Order Psychoacoustic Noise-Shaped TPDF Dither (Boosts 16-bit perceived SNR from 96dB to 108dB)
/// Shifts quantization noise out of the 1-4kHz human vocal band up into >16kHz.
pub struct NoiseShapedQuantizer {
    err_l: f32,
    err_r: f32,
    rng: u32,
}

impl NoiseShapedQuantizer {
    pub const fn new() -> Self {
        Self {
            err_l: 0.0,
            err_r: 0.0,
            rng: 0x2468_ACE1,
        }
    }

    #[inline]
    pub fn quantize_stereo_subframe(
        &mut self,
        src_f32: &[f32; 240],
        dst_i16: &mut [i16; 240],
    ) -> i16 {
        let mut max_abs: i16 = 0;
        for i in 0..120 {
            let in_l = src_f32[i * 2].clamp(-1.0, 1.0);
            let in_r = src_f32[i * 2 + 1].clamp(-1.0, 1.0);

            // Exact digital silence must stay exactly zero: dither here would
            // allocate bits in the lossless encoder and keep DTX from reaching
            // a genuinely silent stream.
            if in_l.abs() < 1.0e-7 && in_r.abs() < 1.0e-7 {
                self.err_l = 0.0;
                self.err_r = 0.0;
                dst_i16[i * 2] = 0;
                dst_i16[i * 2 + 1] = 0;
                continue;
            }

            self.rng ^= self.rng << 13;
            self.rng ^= self.rng >> 17;
            self.rng ^= self.rng << 5;
            let r1 = ((self.rng & 0xFFFF) as f32 / 65535.0) - 0.5;
            let r2 = (((self.rng >> 16) & 0xFFFF) as f32 / 65535.0) - 0.5;
            let dither_l = r1 + r2;
            // Anti-phase dither doubles the noise in the Mid-Side encoder's
            // Side channel (`L - R`), forcing 2-3 bits/sample even for a mono
            // or centered signal. Use correlated dither when the channels are
            // effectively identical so Side stays zero, and independent TPDF
            // otherwise.
            let dither_r = if (in_l - in_r).abs() < 1.0e-5 {
                // Correlated dither alone is not enough: after any genuinely
                // stereo sample the two 0.85 error integrators hold different
                // fractional values, so equal inputs still round differently
                // and the Mid-Side encoder sees phantom Side energy. Converge
                // the states so centered passages quantize to out_l == out_r.
                self.err_r = self.err_l;
                dither_l
            } else {
                r1 - r2
            };

            // Left channel with 0.85 high-pass error feedback
            let shaped_l = in_l * 32766.0 - 0.85 * self.err_l;
            let q_l = (shaped_l + dither_l).round().clamp(-32767.0, 32767.0);
            self.err_l = q_l - shaped_l;
            let out_l = q_l as i16;

            // Right channel with 0.85 high-pass error feedback
            let shaped_r = in_r * 32766.0 - 0.85 * self.err_r;
            let q_r = (shaped_r + dither_r).round().clamp(-32767.0, 32767.0);
            self.err_r = q_r - shaped_r;
            let out_r = q_r as i16;

            dst_i16[i * 2] = out_l;
            dst_i16[i * 2 + 1] = out_r;
            max_abs = max_abs
                .max(out_l.saturating_abs())
                .max(out_r.saturating_abs());
        }
        max_abs
    }
}

/// 9. Phone Microphone 80Hz High-Pass Rumble Filter & Soft-Knee Downward Noise Gate
/// Eliminates desk thumps, AC hum, and PC fan bleed when using Phone-as-PC-Mic.
pub struct MicRumbleAndNoiseGate {
    hp_x1_l: f32,
    hp_y1_l: f32,
    hp_x1_r: f32,
    hp_y1_r: f32,
    gate_env: f32,
}

impl MicRumbleAndNoiseGate {
    pub const fn new() -> Self {
        Self {
            hp_x1_l: 0.0,
            hp_y1_l: 0.0,
            hp_x1_r: 0.0,
            hp_y1_r: 0.0,
            gate_env: 0.0,
        }
    }

    #[inline]
    pub fn process_in_place(&mut self, stereo: &mut [f32]) {
        // 80 Hz 1-pole high-pass at 48kHz: R = exp(-2*pi*80/48000) = 0.98958
        const R: f32 = 0.98958;
        const GATE_THRESH: f32 = 0.0056; // -45 dBFS noise gate threshold

        for frame in stereo.chunks_exact_mut(2) {
            let in_l = frame[0] + 1.0e-25;
            let in_r = frame[1] + 1.0e-25;
            let hp_l = R * (self.hp_y1_l + in_l - self.hp_x1_l);
            let hp_r = R * (self.hp_y1_r + in_r - self.hp_x1_r);
            self.hp_x1_l = in_l;
            self.hp_y1_l = hp_l;
            self.hp_x1_r = in_r;
            self.hp_y1_r = hp_r;

            let peak = hp_l.abs().max(hp_r.abs());
            let coeff = if peak > self.gate_env { 0.25 } else { 0.002 };
            self.gate_env += coeff * (peak - self.gate_env);

            let gate_gain = (self.gate_env / GATE_THRESH).clamp(0.05, 1.0);
            frame[0] = hp_l * gate_gain;
            frame[1] = hp_r * gate_gain;
        }
    }
}

/// 10. QR Code Connection URI Builder (`asultra://connect?host=...&pin=...`)
pub fn build_qr_connect_uri(host_ip: &str, port: u16, pin: Option<&str>) -> String {
    match pin {
        Some(p) if !p.is_empty() => {
            format!("asultra://connect?host={}:{}&pin={}", host_ip, port, p)
        }
        _ => format!("asultra://connect?host={}:{}", host_ip, port),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_multi_phone_ptp_sync_and_channel_roles() {
        let mut sync = GroupSyncEstimator::new();
        // Simulate Host clock 5,000 us ahead of Client clock with 2,000 us RTT
        sync.record_ptp_exchange(10_000, 16_000, 16_100, 12_100);
        assert_eq!(sync.clock_offset_us, 5_000);
        assert_eq!(sync.min_rtt_us, 2_000);

        // Phone 1 has 8ms wired delay; Phone 2 has 28ms speaker delay -> Phone 1 adds +8 subframes (20ms)
        let aligned = sync.aligned_target_subframes(2.0, 28, 8);
        assert!((aligned - 10.0).abs() < 0.01);

        let mut stereo = [1.0f32, -1.0f32, 0.5f32, 0.5f32];
        ChannelRole::LeftMono.apply_in_place(&mut stereo);
        assert_eq!(stereo, [1.0, 1.0, 0.5, 0.5]);

        let mut stereo2 = [1.0f32, -1.0f32];
        ChannelRole::RightMono.apply_in_place(&mut stereo2);
        assert_eq!(stereo2, [-1.0, -1.0]);

        assert_eq!(ChannelRole::from_u8(1), ChannelRole::LeftMono);
        assert_eq!(ChannelRole::from_u8(2), ChannelRole::RightMono);
        assert_eq!(ChannelRole::from_u8(3), ChannelRole::CenterDialogue);
        assert_eq!(ChannelRole::from_u8(4), ChannelRole::SurroundAmbient);
        assert_eq!(ChannelRole::from_u8(0), ChannelRole::Stereo);
    }

    #[test]
    fn test_bauer_crossfeed_and_eq() {
        let mut crossfeed = BauerCrossfeedFilter::new();
        let mut samples = [0.5f32, -0.5f32, 0.2f32, -0.2f32];
        crossfeed.process_in_place(&mut samples);
        // Crossfeed shouldn't blow up or produce NaNs
        assert!(samples[0].is_finite());
        assert!(samples[1].is_finite());

        let mut eq = StudioEqualizer::new();
        eq.update_if_changed(30, 20, -10); // +3dB bass, +2dB vocal, -1dB treble
        eq.process(&mut samples);
        assert!(samples[0].is_finite());
        assert!(samples[1].is_finite());
    }

    /// Measure the steady-state magnitude response of the crossfeed for a
    /// centered signal (`L == R`). The measurement window is 4800 samples, an
    /// integer number of cycles at 700 Hz, 1.8 kHz and 10 kHz, so the DFT has
    /// no rectangular-window leakage at those probes.
    fn crossfeed_centered_gain(freq_hz: f32) -> f32 {
        const SETTLE: usize = 2400;
        const MEASURE: usize = 4800;
        let mut filter = BauerCrossfeedFilter::new();
        let w = 2.0 * std::f32::consts::PI * freq_hz / 48_000.0;
        let mut out = vec![0.0f32; SETTLE + MEASURE];
        for (i, slot) in out.iter_mut().enumerate() {
            let v = if freq_hz == 0.0 {
                0.25
            } else {
                (w * i as f32).sin() * 0.25
            };
            let mut frame = [v, v];
            filter.process_in_place(&mut frame);
            *slot = frame[0];
        }
        let settled = &out[SETTLE..];
        if freq_hz == 0.0 {
            (settled.iter().sum::<f32>() / MEASURE as f32) / 0.25
        } else {
            (tone_amplitude(settled, freq_hz) / 0.25) as f32
        }
    }

    #[test]
    fn test_bauer_crossfeed_centered_response_is_flat() {
        // The old direct gain (0.84) plus a low-passed crossfeed (0.24) gave
        // centered audio +0.67 dB of bass, -1.51 dB of treble, and a -2.44 dB
        // comb notch near 1.85 kHz. 3D Mode must not change the timbre of
        // centered vocals; it should only widen hard-panned content.
        let dc = crossfeed_centered_gain(0.0);
        let low_mid = crossfeed_centered_gain(700.0);
        let notch = crossfeed_centered_gain(1800.0);
        let high = crossfeed_centered_gain(10_000.0);
        println!(
            "crossfeed centered gain: DC {:.4}, 700 Hz {:.4}, 1800 Hz {:.4}, 10 kHz {:.4}",
            dc, low_mid, notch, high
        );

        assert!(
            (dc - 1.0).abs() < 0.02,
            "centered DC gain is {:.4} instead of 1.0",
            dc
        );
        // The 1-pole 700 Hz low-pass still passes ~7.5% at 10 kHz, so the
        // HF complement leaves -0.21 dB rather than absolute unity. That is a
        // 1.2 dB improvement over the old 0.8502 (-1.41 dB) and well inside
        // the 0.03 bound.
        assert!(
            (high - 1.0).abs() < 0.03,
            "centered 10 kHz gain is {:.4} instead of ~1.0",
            high
        );
        assert!(
            notch > 0.85,
            "centered midrange gain is {:.4}; the old comb notch was ~0.755",
            notch
        );
    }

    #[test]
    fn test_bauer_crossfeed_still_mixes_opposite_channel() {
        // Fixing the center response must not turn the crossfeed into a plain
        // passthrough: a hard-left signal still needs to reach the right ear.
        const SETTLE: usize = 2400;
        const MEASURE: usize = 4800;
        let mut filter = BauerCrossfeedFilter::new();
        let w = 2.0 * std::f32::consts::PI * 700.0 / 48_000.0;
        let mut right = vec![0.0f32; SETTLE + MEASURE];
        for (i, slot) in right.iter_mut().enumerate() {
            let v = (w * i as f32).sin() * 0.5;
            let mut frame = [v, 0.0];
            filter.process_in_place(&mut frame);
            *slot = frame[1];
        }
        let leak = (tone_amplitude(&right[SETTLE..], 700.0) / 0.5) as f32;
        println!("crossfeed opposite-ear leak at 700 Hz: {:.4}", leak);
        assert!(
            leak > 0.05,
            "crossfeed no longer mixes into the opposite ear: {:.4}",
            leak
        );
        assert!(
            leak < 0.30,
            "crossfeed leaks too much into the opposite ear: {:.4}",
            leak
        );
    }

    #[test]
    fn test_eq_reset_to_flat_clears_filter_history() {
        // While all three bands are at 0 dB `StudioEqualizer::process`
        // bypasses the biquads, so their delay state freezes. Re-engaging a
        // band then discharged that stale state as a click on the first frame.
        let mut eq = StudioEqualizer::new();
        eq.update_if_changed(60, 0, 0); // +6 dB bass
        let mut warmup = [0.25f32; 480];
        eq.process(&mut warmup);

        eq.update_if_changed(0, 0, 0); // user clicks Flat
        eq.update_if_changed(60, 0, 0); // re-engage the same bass boost

        let mut first = [0.0f32; 2];
        eq.process(&mut first);
        println!("first sample after EQ re-engage: {:?}", first);
        assert!(
            first[0].abs() < 1.0e-6 && first[1].abs() < 1.0e-6,
            "stale biquad state produced a click: {:?}",
            first
        );
    }

    #[test]
    fn test_qr_uri_builder() {
        assert_eq!(
            build_qr_connect_uri("192.168.1.50", 65530, Some("1234")),
            "asultra://connect?host=192.168.1.50:65530&pin=1234"
        );
        assert_eq!(
            build_qr_connect_uri("192.168.1.50", 65530, None),
            "asultra://connect?host=192.168.1.50:65530"
        );
    }

    #[test]
    fn test_night_mode_compressor() {
        let mut comp = NightModeCompressor::new();
        // Quiet whisper at ~0.05 (-26 dBFS) should receive upward boost
        let mut quiet_block = [0.05f32; 240];
        comp.process_in_place(&mut quiet_block);
        assert!(quiet_block[238].abs() > 0.05);

        // Loud explosion at 0.95 should be clamped
        let mut loud_block = [0.95f32; 240];
        comp.process_in_place(&mut loud_block);
        assert!(loud_block[238].abs() < 0.90);
    }

    #[test]
    fn test_night_mode_gate_has_no_step_discontinuity() {
        // Evaluate the gain one LSB either side of the old -55 dBFS gate
        // boundary. The old curve jumped from 1.0x to 3.75x (+11.5 dB).
        let low = 0.001799f32;
        let high = 0.001801f32;

        let mut below = NightModeCompressor::new();
        below.env = low;
        let mut a = [low, low];
        below.process_in_place(&mut a);
        let gain_below = a[0] / low;

        let mut above = NightModeCompressor::new();
        above.env = high;
        let mut b = [high, high];
        above.process_in_place(&mut b);
        let gain_above = b[0] / high;

        println!(
            "night gate gain across boundary: {:.4} -> {:.4}",
            gain_below, gain_above
        );
        assert!(
            (gain_above - gain_below).abs() < 0.25,
            "gain steps from {:.4} to {:.4} across the gate boundary",
            gain_below,
            gain_above
        );
    }

    fn settled_night_output(level: f32) -> f32 {
        let mut comp = NightModeCompressor::new();
        let mut out = 0.0f32;
        // 125 ms is long enough for both the 1.5 ms level envelope and the
        // slow gain-recovery smoother to settle on a constant input.
        for _ in 0..6000 {
            let mut frame = [level, level];
            comp.process_in_place(&mut frame);
            out = frame[0];
        }
        out
    }

    #[test]
    fn test_night_mode_transfer_is_monotonic() {
        // Normal speech sits between about -16 dBFS and -12 dBFS. The old
        // curve reduced gain faster than the level grew, so 0.19 input
        // produced less output than 0.18 (dynamic inversion / wavefolding).
        let levels = [
            0.16f32, 0.17, 0.18, 0.19, 0.20, 0.22, 0.24, 0.25, 0.27, 0.28,
        ];
        let outputs: Vec<f32> = levels.iter().map(|&l| settled_night_output(l)).collect();
        println!("night-mode transfer outputs: {:?}", outputs);

        for (pair, levels) in outputs.windows(2).zip(levels.windows(2)) {
            assert!(
                pair[1] > pair[0],
                "louder input {} -> {:.4} did not exceed {} -> {:.4}",
                levels[1],
                pair[1],
                levels[0],
                pair[0]
            );
        }
    }

    #[test]
    fn test_night_mode_transient_does_not_overshoot() {
        // A 0.85 transient on top of a 0.01 envelope must not receive whisper
        // boost on its leading edge. The old gain was computed from the
        // partially updated envelope (0.178) and amplified it to 1.41 before
        // hard-clipping at 0.89.
        let mut comp = NightModeCompressor::new();
        comp.env = 0.01;
        let mut frame = [0.85f32, 0.85];
        comp.process_in_place(&mut frame);
        let peak = frame[0].abs();
        println!("night-mode transient output peak: {:.4}", peak);
        assert!(
            peak <= 0.84 + 1.0e-6,
            "transient overshot to {} instead of staying under the clamp",
            peak
        );
        assert!(peak > 0.2, "transient was over-attenuated to {}", peak);
    }

    fn tone_amplitude(samples: &[f32], freq_hz: f32) -> f64 {
        let w = 2.0 * std::f64::consts::PI * freq_hz as f64 / 48_000.0;
        let mut re = 0.0f64;
        let mut im = 0.0f64;
        for (n, &sample) in samples.iter().enumerate() {
            let phase = w * n as f64;
            re += sample as f64 * phase.cos();
            im += sample as f64 * phase.sin();
        }
        2.0 * (re * re + im * im).sqrt() / samples.len() as f64
    }

    #[test]
    fn test_night_mode_low_frequency_distortion() {
        // A steady 100 Hz bass tone at 0.2 sits in the old transition zone.
        // Its envelope ripples between peaks, so the gain modulation shows up
        // as harmonics. 0.9 s after a 0.1 s settle is an integer number of
        // cycles at 100 Hz and each measured harmonic, avoiding leakage.
        let mut comp = NightModeCompressor::new();
        let mut processed = Vec::with_capacity(48_000);
        for n in 0..48_000 {
            let v = ((n as f32) * 2.0 * std::f32::consts::PI * 100.0 / 48_000.0).sin() * 0.2;
            let mut frame = [v, v];
            comp.process_in_place(&mut frame);
            processed.push(frame[0]);
        }
        let settled = &processed[4_800..];
        let h1 = tone_amplitude(settled, 100.0);
        let h2 = tone_amplitude(settled, 200.0);
        let h3 = tone_amplitude(settled, 300.0);
        let h4 = tone_amplitude(settled, 400.0);
        let thd = (h2 * h2 + h3 * h3 + h4 * h4).sqrt() / h1;
        println!(
            "night-mode 100 Hz THD: {:.3}% (H1 {:.5}, H2 {:.5}, H3 {:.5}, H4 {:.5})",
            thd * 100.0,
            h1,
            h2,
            h3,
            h4
        );
        assert!(
            thd < 0.02,
            "100 Hz bass THD is {:.2}%, above the 2% ceiling",
            thd * 100.0
        );
    }

    #[test]
    fn test_latency_sonar_chirp_and_detection() {
        let mut chirp_subframe = [0.0f32; 240];
        LatencySonar::inject_chirp(&mut chirp_subframe);
        assert!(LatencySonar::detect_chirp_return(&chirp_subframe));

        // Random noise or silence shouldn't trigger false positive
        let silence = [0.0f32; 240];
        assert!(!LatencySonar::detect_chirp_return(&silence));
    }

    #[test]
    fn test_latency_sonar_detects_chirp_split_across_subframes() {
        let mut chirp = [0.0f32; 240];
        LatencySonar::inject_chirp(&mut chirp);

        // An acoustic return arrives at an arbitrary phase, so the 2.5ms
        // chirp is split between two capture subframes. At least one half
        // must still exceed the detection threshold.
        for split in [40usize, 50, 60, 70, 80, 90] {
            let mut first = [0.0f32; 240];
            let mut second = [0.0f32; 240];
            first[..split * 2].copy_from_slice(&chirp[..split * 2]);
            second[..(120 - split) * 2].copy_from_slice(&chirp[split * 2..]);
            assert!(
                LatencySonar::detect_chirp_return(&first)
                    || LatencySonar::detect_chirp_return(&second),
                "split at {} frames was missed",
                split
            );
        }

        // A 1kHz tone has too few zero crossings to be mistaken for a chirp.
        let mut tone = [0.0f32; 240];
        for i in 0..120 {
            let v = ((i as f32) * 2.0 * std::f32::consts::PI * 1000.0 / 48_000.0).sin() * 0.8;
            tone[i * 2] = v;
            tone[i * 2 + 1] = v;
        }
        assert!(!LatencySonar::detect_chirp_return(&tone));
    }

    #[test]
    fn test_noise_shaped_quantizer() {
        let mut quantizer = NoiseShapedQuantizer::new();
        let src = [0.25f32; 240];
        let mut dst = [0i16; 240];
        let max_abs = quantizer.quantize_stereo_subframe(&src, &mut dst);
        assert!(max_abs > 0);
        assert!((dst[0] - 8192).abs() < 100);
    }

    #[test]
    fn test_noise_shaped_quantizer_keeps_silence_and_mono_side_clean() {
        // Digital silence must remain bit-exact zero so DTX can engage and
        // the lossless encoder does not spend bits on dither.
        let mut quantizer = NoiseShapedQuantizer::new();
        let silence = [0.0f32; 240];
        let mut silent_out = [123i16; 240];
        assert_eq!(
            quantizer.quantize_stereo_subframe(&silence, &mut silent_out),
            0
        );
        assert!(silent_out.iter().all(|&sample| sample == 0));

        // A centered/mono signal must quantize to identical L/R samples. The
        // old anti-phase dither made L-R a random +/-2 LSB signal, which the
        // Mid-Side lossless encoder had to transmit at 2-3 bits/sample.
        let mut mono = [0.0f32; 240];
        for i in 0..120 {
            let sample = ((i as f32) * 2.0 * std::f32::consts::PI * 440.0 / 48_000.0).sin() * 0.4;
            mono[i * 2] = sample;
            mono[i * 2 + 1] = sample;
        }
        let mut mono_out = [0i16; 240];
        quantizer.quantize_stereo_subframe(&mono, &mut mono_out);
        for frame in mono_out.chunks_exact(2) {
            assert_eq!(
                frame[0], frame[1],
                "mono input must produce an exactly zero Side channel"
            );
        }

        // Uncorrelated stereo still receives independent dither and must stay
        // finite and within the 16-bit range.
        let mut stereo = [0.0f32; 240];
        for i in 0..240 {
            stereo[i] = if i & 1 == 0 { 0.3 } else { -0.2 };
        }
        let mut stereo_out = [0i16; 240];
        quantizer.quantize_stereo_subframe(&stereo, &mut stereo_out);
        assert!(stereo_out.iter().all(|&sample| sample < i16::MAX));
    }

    #[test]
    fn test_channel_roles_do_not_boost_centered_or_anti_phase_peaks() {
        // Coherent center at full scale: (L + R) must not exceed the original
        // per-speaker amplitude. 1/sqrt(2) produced 1.414 (+3.01 dB) and
        // clipped any centered full-scale passage.
        let mut center = [1.0f32; 240];
        ChannelRole::CenterDialogue.apply_in_place(&mut center);
        let center_peak = center.iter().fold(0.0f32, |peak, &s| peak.max(s.abs()));

        // Anti-phase stereo at full scale: the extracted side signal reached
        // 1.414 and clipped before the output limiter even saw it.
        let mut anti_phase = [0.0f32; 240];
        for frame in anti_phase.chunks_exact_mut(2) {
            frame[0] = 1.0;
            frame[1] = -1.0;
        }
        ChannelRole::SurroundAmbient.apply_in_place(&mut anti_phase);
        let surround_peak = anti_phase.iter().fold(0.0f32, |peak, &s| peak.max(s.abs()));

        println!(
            "center-dialogue peak: {:.6}, surround-ambient peak: {:.6}",
            center_peak, surround_peak
        );
        assert!(
            center_peak <= 1.0 + 1.0e-6,
            "center dialogue boosted full scale to {}",
            center_peak
        );
        assert!(
            surround_peak <= 1.0 + 1.0e-6,
            "surround extraction boosted anti-phase full scale to {}",
            surround_peak
        );
    }

    #[test]
    fn test_noise_shaped_quantizer_centered_audio_converges_after_stereo() {
        let mut quantizer = NoiseShapedQuantizer::new();

        // Any preceding uncorrelated stereo sample can leave the two error
        // integrators at different fractional values. Reproduce a concrete
        // divergence directly so the test does not depend on whatever the
        // final sample of one particular stereo waveform happened to be.
        quantizer.err_l = 0.4;
        quantizer.err_r = -0.4;

        // A quiet centered passage is where the residual error-state mismatch
        // dominates: 0.01 is only ~328 LSB, so a fraction of an LSB of
        // feedback difference flips the rounding decision.
        let mut mono = [0.0f32; 240];
        for i in 0..120 {
            let sample =
                ((i as f32) * 2.0 * std::f32::consts::PI * 997.0 / 48_000.0 + 0.3).sin() * 0.01;
            mono[i * 2] = sample;
            mono[i * 2 + 1] = sample;
        }

        let mut mismatches = 0usize;
        let mut mono_out = [0i16; 240];
        for _ in 0..4 {
            quantizer.quantize_stereo_subframe(&mono, &mut mono_out);
            mismatches += mono_out
                .chunks_exact(2)
                .filter(|frame| frame[0] != frame[1])
                .count();
        }
        println!("centered L/R mismatches after stereo: {}", mismatches);
        assert_eq!(
            mismatches, 0,
            "centered audio produced {} non-zero Side samples after stereo",
            mismatches
        );
    }

    #[test]
    fn test_mic_rumble_and_noise_gate() {
        let mut gate = MicRumbleAndNoiseGate::new();
        // Quiet hiss below gate threshold (0.0056)
        let mut quiet = [0.001f32; 240];
        gate.process_in_place(&mut quiet);
        assert!(quiet[238].abs() < 0.001); // Attenuated by gate
    }

    #[test]
    fn test_anti_denormal_preserves_subnormal_stability() {
        let mut biquad = StereoBiquad::passthrough();
        biquad.set_peaking_eq(1000.0, 1.0, 3.0);
        // Feed prolonged silence to test subnormal decay
        let mut silence = [0.0f32; 480];
        biquad.process_stereo_in_place(&mut silence);
        for &s in &silence {
            assert!(s.is_finite());
            // Filter states should remain finite and free of NaN/Inf
            assert!(s.abs() < 1e-4);
        }
    }
}
