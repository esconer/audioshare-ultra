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
        for frame in stereo.chunks_exact_mut(2) {
            let in_l = frame[0];
            let out_l = self.b0 * in_l + self.z1_l;
            self.z1_l = self.b1 * in_l - self.a1 * out_l + self.z2_l;
            self.z2_l = self.b2 * in_l - self.a2 * out_l;
            frame[0] = out_l;

            let in_r = frame[1];
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
            self.bass.set_peaking_eq(90.0, 0.707, (bass_tenths as f32) / 10.0);
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

/// 6. QR Code Connection URI Builder (`asultra://connect?host=...&pin=...`)
pub fn build_qr_connect_uri(host_ip: &str, port: u16, pin: Option<&str>) -> String {
    match pin {
        Some(p) if !p.is_empty() => format!("asultra://connect?host={}:{}&pin={}", host_ip, port, p),
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
}
