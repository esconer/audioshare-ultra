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
    /// Center Vocal / Dialogue Extraction: Mid = (L + R) * 0.7071
    CenterDialogue = 3,
    /// Side/Surround Difference Extraction: Side = (L - R) * 0.7071
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
                    let mid = (l + r) * std::f32::consts::FRAC_1_SQRT_2;
                    frame[0] = mid;
                    frame[1] = mid;
                }
                Self::SurroundAmbient => {
                    let side = (l - r) * std::f32::consts::FRAC_1_SQRT_2;
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
        // 700 Hz 1-pole low-pass coefficient at 48kHz: 1 - exp(-2*pi*700/48000)
        const ALPHA_700HZ: f32 = 0.0875;
        const DIRECT_GAIN: f32 = 0.84;
        const CROSS_GAIN: f32 = 0.24;
        const ITD_SAMPLES: usize = 13; // 270 microseconds Interaural Time Difference

        for frame in stereo.chunks_exact_mut(2) {
            let in_l = frame[0];
            let in_r = frame[1];

            // Low-pass filter current samples (head shadow simulation)
            self.lp_l += ALPHA_700HZ * (in_l - self.lp_l);
            self.lp_r += ALPHA_700HZ * (in_r - self.lp_r);

            self.delay_l[self.idx] = self.lp_l;
            self.delay_r[self.idx] = self.lp_r;

            let delayed_idx = (self.idx + 16 - ITD_SAMPLES) & 15;
            let cross_from_l = self.delay_l[delayed_idx];
            let cross_from_r = self.delay_r[delayed_idx];

            frame[0] = in_l * DIRECT_GAIN + cross_from_r * CROSS_GAIN;
            frame[1] = in_r * DIRECT_GAIN + cross_from_l * CROSS_GAIN;

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
/// - Boosts quiet movie dialogue & game footsteps (-50 dBFS .. -16 dBFS) by up to +11.5 dB
/// - Tames sudden loud explosions/gunshots above -12 dBFS with a 4:1 ratio
/// - Noise-gates signals below -55 dBFS so background hiss is never amplified
pub struct NightModeCompressor {
    env: f32,
}

impl NightModeCompressor {
    pub const fn new() -> Self {
        Self { env: 0.0 }
    }

    #[inline]
    pub fn process_in_place(&mut self, stereo: &mut [f32]) {
        const NOISE_FLOOR: f32 = 0.0018; // -55 dBFS gate threshold
        const WHISPER_ZONE: f32 = 0.16; // -16 dBFS upward boost target
        const LOUD_THRESH: f32 = 0.25; // -12 dBFS downward 4:1 compression start
        const MAX_BOOST: f32 = 3.75; // +11.5 dB max whisper/footstep boost

        for frame in stereo.chunks_exact_mut(2) {
            let l = frame[0];
            let r = frame[1];
            let peak = l.abs().max(r.abs()) + 1.0e-25;

            // Fast attack (0.2), smooth release (0.0008) to prevent pumping
            if peak > self.env {
                self.env += 0.20 * (peak - self.env);
            } else {
                self.env += 0.0008 * (peak - self.env);
            }

            let gain = if self.env < NOISE_FLOOR {
                1.0
            } else if self.env < WHISPER_ZONE {
                // Smooth upward compression for quiet voices & footsteps
                let norm = (self.env - NOISE_FLOOR) / (WHISPER_ZONE - NOISE_FLOOR);
                1.0 + (MAX_BOOST - 1.0) * (1.0 - norm * 0.7)
            } else if self.env > LOUD_THRESH {
                // 4:1 downward compression for sudden explosions
                let target = LOUD_THRESH + (self.env - LOUD_THRESH) * 0.25;
                (target / self.env).min(1.0)
            } else {
                1.0 + (MAX_BOOST - 1.0)
                    * 0.3
                    * ((LOUD_THRESH - self.env) / (LOUD_THRESH - WHISPER_ZONE))
            };

            frame[0] = (l * gain).clamp(-0.89, 0.89);
            frame[1] = (r * gain).clamp(-0.89, 0.89);
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
            self.rng ^= self.rng << 13;
            self.rng ^= self.rng >> 17;
            self.rng ^= self.rng << 5;
            let r1 = ((self.rng & 0xFFFF) as f32 / 65535.0) - 0.5;
            let r2 = (((self.rng >> 16) & 0xFFFF) as f32 / 65535.0) - 0.5;
            let dither = r1 + r2;

            // Left channel with 0.85 high-pass error feedback
            let shaped_l = src_f32[i * 2].clamp(-1.0, 1.0) * 32766.0 - 0.85 * self.err_l;
            let q_l = (shaped_l + dither).round().clamp(-32767.0, 32767.0);
            self.err_l = q_l - shaped_l;
            let out_l = q_l as i16;

            // Right channel with 0.85 high-pass error feedback
            let shaped_r = src_f32[i * 2 + 1].clamp(-1.0, 1.0) * 32766.0 - 0.85 * self.err_r;
            let q_r = (shaped_r - dither).round().clamp(-32767.0, 32767.0);
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
