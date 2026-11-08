use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

/// 1. Jitter-Immune Windowed-Min DLL + 4-Point Hermite Cubic Fractional Resampler
pub struct ClockDriftResampler {
    phase: f64,
    pub ratio: f64,
    hist_l: [f32; 4],
    hist_r: [f32; 4],
    short_ema_queue: f32,
    min_queue_frames_window: f32,
    window_ticks: u32,
    integral_err: f64,
}

impl ClockDriftResampler {
    pub fn new() -> Self {
        Self {
            phase: 0.0,
            ratio: 1.0,
            hist_l: [0.0; 4],
            hist_r: [0.0; 4],
            short_ema_queue: -1.0,
            min_queue_frames_window: 999.0,
            window_ticks: 0,
            integral_err: 0.0,
        }
    }

    /// Observe current queue depth every 2.5ms subframe.
    /// Uses a 200-subframe (500ms) minimum envelope to reject Wi-Fi arrival jitter completely.
    #[inline]
    pub fn update_clock_loop(&mut self, current_queued_subframes: f32, target_min_subframes: f32) {
        if self.short_ema_queue < 0.0 {
            self.short_ema_queue = current_queued_subframes;
        } else {
            self.short_ema_queue = 0.85 * self.short_ema_queue + 0.15 * current_queued_subframes;
        }
        if self.short_ema_queue < self.min_queue_frames_window {
            self.min_queue_frames_window = self.short_ema_queue;
        }
        self.window_ticks += 1;

        if self.window_ticks >= 200 {
            let err = (self.min_queue_frames_window - target_min_subframes) as f64;
            self.integral_err = (self.integral_err + err * 0.5).clamp(-10.0, 10.0);

            // Dual-slope catch-up: +/- 500 ppm for normal drift, up to +2500 ppm after a Wi-Fi burst spike
            let (kp, max_clamp) = if err > 2.0 {
                (0.00025, 0.0025)
            } else {
                (0.00005, 0.0005)
            };
            let corr = (kp * err + 0.000005 * self.integral_err).clamp(-0.0005, max_clamp);
            self.ratio = 1.0 + corr;
            self.min_queue_frames_window = 999.0;
            self.window_ticks = 0;
        }
    }

    #[inline(always)]
    fn hermite4(frac: f32, y0: f32, y1: f32, y2: f32, y3: f32) -> f32 {
        let c0 = y1;
        let c1 = 0.5 * (y2 - y0);
        let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
        let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
        ((c3 * frac + c2) * frac + c1) * frac + c0
    }

    /// Resamples interleaved stereo input into interleaved stereo output.
    /// Returns number of stereo frames (sample pairs) written to `out_stereo`.
    pub fn process(&mut self, in_stereo: &[f32], out_stereo: &mut [f32]) -> usize {
        let in_frames = in_stereo.len() / 2;
        let max_out_frames = out_stereo.len() / 2;
        let mut in_idx = 0usize;
        let mut out_idx = 0usize;

        while in_idx < in_frames && out_idx < max_out_frames {
            while self.phase < 1.0 && out_idx < max_out_frames {
                let f = self.phase as f32;
                out_stereo[out_idx * 2] = Self::hermite4(
                    f,
                    self.hist_l[0],
                    self.hist_l[1],
                    self.hist_l[2],
                    self.hist_l[3],
                );
                out_stereo[out_idx * 2 + 1] = Self::hermite4(
                    f,
                    self.hist_r[0],
                    self.hist_r[1],
                    self.hist_r[2],
                    self.hist_r[3],
                );
                out_idx += 1;
                self.phase += self.ratio;
            }
            while self.phase >= 1.0 && in_idx < in_frames {
                self.hist_l.copy_within(1..4, 0);
                self.hist_r.copy_within(1..4, 0);
                self.hist_l[3] = in_stereo[in_idx * 2];
                self.hist_r[3] = in_stereo[in_idx * 2 + 1];
                in_idx += 1;
                self.phase -= 1.0;
            }
        }
        out_idx
    }
}

/// 2. Wait-Free SPSC Sample FIFO (Decouples 120-frame network quantum from Android SoC getFramesPerBurst())
const FIFO_CAPACITY: usize = 8192; // Must be power of 2 (4096 stereo frames)
const FIFO_MASK: usize = FIFO_CAPACITY - 1;

pub struct BurstDecouplerRing {
    write_idx: AtomicUsize,
    read_idx: AtomicUsize,
    buf: UnsafeCell<[f32; FIFO_CAPACITY]>,
}

unsafe impl Sync for BurstDecouplerRing {}

impl BurstDecouplerRing {
    pub fn new() -> Self {
        Self {
            write_idx: AtomicUsize::new(0),
            read_idx: AtomicUsize::new(0),
            buf: UnsafeCell::new([0.0; FIFO_CAPACITY]),
        }
    }

    #[inline]
    pub fn available_samples(&self) -> usize {
        let w = self.write_idx.load(Ordering::Acquire);
        let r = self.read_idx.load(Ordering::Acquire);
        w.wrapping_sub(r)
    }

    #[inline]
    pub fn push_samples(&self, samples: &[f32]) -> usize {
        let w = self.write_idx.load(Ordering::Relaxed);
        let r = self.read_idx.load(Ordering::Acquire);
        let free = FIFO_CAPACITY - w.wrapping_sub(r);
        let to_write = samples.len().min(free);

        let buf_ptr = self.buf.get() as *mut f32;
        for (i, &s) in samples[..to_write].iter().enumerate() {
            unsafe {
                *buf_ptr.add((w.wrapping_add(i)) & FIFO_MASK) = s;
            }
        }
        self.write_idx
            .store(w.wrapping_add(to_write), Ordering::Release);
        to_write
    }

    /// Pull exact `out.len()` samples required by Android AAudio hardware burst (e.g., 96, 128, or 192 stereo frames)
    #[inline]
    pub fn pop_exact_or_dither(&self, out: &mut [f32], dither_rng: &mut u32) -> bool {
        let r = self.read_idx.load(Ordering::Relaxed);
        let w = self.write_idx.load(Ordering::Acquire);
        let avail = w.wrapping_sub(r);

        if avail >= out.len() {
            let buf_ptr = self.buf.get() as *const f32;
            for (i, dst) in out.iter_mut().enumerate() {
                unsafe {
                    *dst = *buf_ptr.add((r.wrapping_add(i)) & FIFO_MASK);
                }
            }
            self.read_idx
                .store(r.wrapping_add(out.len()), Ordering::Release);
            true
        } else if (avail & !1) > 0 {
            // Copy all available samples and smoothly decay the tail (NEVER drop to abrupt silence!)
            let safe_avail = avail & !1; // Keep stereo L/R alignment
            let buf_ptr = self.buf.get() as *const f32;
            let mut last_l = 0.0f32;
            let mut last_r = 0.0f32;
            for i in 0..safe_avail {
                let s = unsafe { *buf_ptr.add((r.wrapping_add(i)) & FIFO_MASK) };
                out[i] = s;
                if i & 1 == 0 {
                    last_l = s;
                } else {
                    last_r = s;
                }
            }
            self.read_idx
                .store(r.wrapping_add(safe_avail), Ordering::Release);
            for i in (safe_avail..out.len()).step_by(2) {
                last_l *= 0.985;
                last_r *= 0.985;
                out[i] = last_l;
                if i + 1 < out.len() {
                    out[i + 1] = last_r;
                }
            }
            true
        } else {
            // Inject -90 dBFS TPDF comfort dither so Bluetooth earbuds never sleep during DTX silence
            inject_bluetooth_keepalive_dither(out, dither_rng);
            false
        }
    }
}

/// 3. Studio Pre-Amp + Soft-Knee True-Peak Limiter (distortion-free loudness boost)
pub struct TruePeakLimiter {
    current_gain_linear: f32,
    target_gain_linear: f32,
    envelope: f32,
}

impl TruePeakLimiter {
    pub fn new(gain_db: f32) -> Self {
        let g = 10.0f32.powf(gain_db.clamp(-24.0, 24.0) / 20.0);
        Self {
            current_gain_linear: g,
            target_gain_linear: g,
            envelope: 0.0,
        }
    }

    pub fn set_gain_db(&mut self, gain_db: f32) {
        self.target_gain_linear = 10.0f32.powf(gain_db.clamp(-24.0, 24.0) / 20.0);
    }

    #[inline]
    pub fn process_in_place(&mut self, stereo_samples: &mut [f32]) {
        const THRESHOLD: f32 = 0.85; // -1.4 dBFS soft-knee start
        const CEILING: f32 = 0.988; // -0.1 dBFS hard true-peak ceiling
        const ATTACK: f32 = 0.25;
        const RELEASE: f32 = 0.0005;

        for frame in stereo_samples.chunks_exact_mut(2) {
            self.current_gain_linear += 0.01 * (self.target_gain_linear - self.current_gain_linear);
            let mut l = frame[0] * self.current_gain_linear;
            let mut r = frame[1] * self.current_gain_linear;
            let peak = l.abs().max(r.abs());

            if peak > self.envelope {
                self.envelope += ATTACK * (peak - self.envelope);
            } else {
                self.envelope += RELEASE * (peak - self.envelope);
            }

            if self.envelope > THRESHOLD {
                let over = self.envelope - THRESHOLD;
                let compressed =
                    THRESHOLD + (CEILING - THRESHOLD) * (over / (over + (CEILING - THRESHOLD)));
                let scale = compressed / self.envelope;
                l *= scale;
                r *= scale;
            }

            frame[0] = l.clamp(-CEILING, CEILING);
            frame[1] = r.clamp(-CEILING, CEILING);
        }
    }
}

/// 4. -90 dBFS TPDF Dither Generator (Keeps phone DAC & Bluetooth earbuds awake during DTX silence)
#[inline]
pub fn inject_bluetooth_keepalive_dither(out: &mut [f32], rng_state: &mut u32) {
    const AMPLITUDE_MINUS_90DB: f32 = 0.000_031_62; // 10^(-90/20)
    for sample in out.iter_mut() {
        // Xorshift32 PRNG
        let mut x = *rng_state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        *rng_state = x.max(1);

        let r1 = ((x & 0xFFFF) as f32 / 65535.0) - 0.5;
        let r2 = (((x >> 16) & 0xFFFF) as f32 / 65535.0) - 0.5;
        *sample = (r1 + r2) * AMPLITUDE_MINUS_90DB;
    }
}

/// 5. ITU-R BS.775 Multi-Channel Surround Downmixer (7.1 / 5.1 / Quad / Mono -> 2.0 Stereo)
/// Handles 5.1/7.1 headsets, planar Linux audio and mono sources
#[inline]
pub fn downmix_to_stereo(in_interleaved: &[f32], channels: usize, out_stereo: &mut [f32]) {
    const CENTER_MIX: f32 = std::f32::consts::FRAC_1_SQRT_2; // -3.0 dB (0.7071)
    const SURROUND_MIX: f32 = std::f32::consts::FRAC_1_SQRT_2;
    // Equal-power headroom for the stereo bus
    const HEADROOM: f32 = 0.7071;

    let frames = in_interleaved.len() / channels.max(1);
    for i in 0..frames {
        let src = &in_interleaved[i * channels..(i + 1) * channels];
        let (l, r) = match channels {
            1 => (src[0], src[0]),
            2 => (src[0], src[1]),
            4 => {
                // Quad: FL, FR, BL, BR
                (
                    (src[0] + SURROUND_MIX * src[2]) * HEADROOM,
                    (src[1] + SURROUND_MIX * src[3]) * HEADROOM,
                )
            }
            6 => {
                // 5.1: FL, FR, FC, LFE, BL, BR
                let c = src[2] * CENTER_MIX;
                (
                    (src[0] + c + SURROUND_MIX * src[4]) * HEADROOM,
                    (src[1] + c + SURROUND_MIX * src[5]) * HEADROOM,
                )
            }
            8 => {
                // 7.1: FL, FR, FC, LFE, BL, BR, SL, SR
                let c = src[2] * CENTER_MIX;
                (
                    (src[0] + c + SURROUND_MIX * (src[4] + src[6])) * HEADROOM,
                    (src[1] + c + SURROUND_MIX * (src[5] + src[7])) * HEADROOM,
                )
            }
            _ => (src[0], *src.get(1).unwrap_or(&src[0])),
        };
        out_stereo[i * 2] = l;
        out_stereo[i * 2 + 1] = r;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_burst_decoupler_with_android_192_frame_burst() {
        let ring = BurstDecouplerRing::new();
        // Push four 120-frame (240 sample) network subframes = 960 samples
        let net_subframe = [0.42f32; 240];
        for _ in 0..4 {
            assert_eq!(ring.push_samples(&net_subframe), 240);
        }

        // Pull two 192-frame (384 sample) Android AAudio hardware bursts
        let mut aaudio_burst = [0.0f32; 384];
        let mut rng = 12345u32;
        assert!(ring.pop_exact_or_dither(&mut aaudio_burst, &mut rng));
        assert_eq!(aaudio_burst[0], 0.42);
        assert!(ring.pop_exact_or_dither(&mut aaudio_burst, &mut rng));
        assert_eq!(ring.available_samples(), 960 - 768);

        // Third pull: only 192 samples remaining (< 384 request).
        // Partial-read continuity copies all 192 samples and smoothly attenuates the tail!
        assert!(ring.pop_exact_or_dither(&mut aaudio_burst, &mut rng));
        assert_eq!(aaudio_burst[0], 0.42);
        assert_eq!(aaudio_burst[191], 0.42);
        assert!(aaudio_burst[192] < 0.42); // Exponential decay started
        assert!(aaudio_burst[383] < aaudio_burst[192]); // Further decay towards end of tail
        assert_eq!(ring.available_samples(), 0);

        // Fourth pull: 0 samples remaining -> comfort dither injected and returns false
        assert!(!ring.pop_exact_or_dither(&mut aaudio_burst, &mut rng));
    }

    #[test]
    fn test_true_peak_limiter_prevents_clipping_at_plus_20db() {
        let mut limiter = TruePeakLimiter::new(20.0); // +20 dB (10x linear amplification!)
        let mut loud_signal = [0.5f32; 240]; // 0.5 * 10.0 = 5.0 (would clip brutally without limiter)
        limiter.process_in_place(&mut loud_signal);

        for &s in &loud_signal {
            assert!(s.abs() <= 0.989, "Sample clipped: {}", s);
        }
    }

    #[test]
    fn test_hermite_resampler_preserves_dc_and_ratio() {
        let mut resampler = ClockDriftResampler::new();
        resampler.ratio = 44_100.0 / 48_000.0;

        let in_frames = 480usize;
        let mut input = vec![0.0f32; in_frames * 2];
        for frame in input.chunks_exact_mut(2) {
            frame[0] = 0.25;
            frame[1] = -0.5;
        }

        let mut output = vec![0.0f32; 1024];
        let out_frames = resampler.process(&input, &mut output);
        assert!(
            out_frames > 400 && out_frames < 600,
            "unexpected resampled frame count: {}",
            out_frames
        );

        // The 4-point history has filled long before the tail; DC must survive.
        for frame in output[..out_frames * 2]
            .chunks_exact(2)
            .skip(out_frames - 100)
        {
            assert!((frame[0] - 0.25).abs() < 1e-4, "L drift: {}", frame[0]);
            assert!((frame[1] + 0.5).abs() < 1e-4, "R drift: {}", frame[1]);
        }
    }

    #[test]
    fn test_dual_slope_clock_controller_accelerates_on_spikes() {
        let mut resampler = ClockDriftResampler::new();
        // Normal drift: err = 1.0 (<= 2.0 subframes)
        for _ in 0..200 {
            resampler.update_clock_loop(3.0, 2.0);
        }
        let normal_corr = resampler.ratio - 1.0;
        assert!(normal_corr > 0.0 && normal_corr <= 0.0005);

        // Burst spike: err = 6.0 (> 2.0 subframes)
        let mut spike_resampler = ClockDriftResampler::new();
        for _ in 0..200 {
            spike_resampler.update_clock_loop(8.0, 2.0);
        }
        let spike_corr = spike_resampler.ratio - 1.0;
        assert!(
            spike_corr > 0.0005 && spike_corr <= 0.0025,
            "Spike correction must accelerate up to +2500 ppm: {}",
            spike_corr
        );
    }

    #[test]
    fn test_true_peak_limiter_gain_smoothing() {
        let mut limiter = TruePeakLimiter::new(0.0);
        limiter.set_gain_db(12.0); // Step up target
        let mut samples = [0.1f32; 240];
        limiter.process_in_place(&mut samples);
        // First sample should not immediately jump to full 12dB (3.98x)
        assert!(samples[0] < 0.1 * 3.9);
        // Later samples should smoothly ramp upward
        assert!(samples[238] > samples[0]);
    }

    #[test]
    fn test_clock_drift_resampler_ema_smoothing() {
        let mut resampler = ClockDriftResampler::new();
        // Prime loop at 4.0 subframes
        for _ in 0..50 {
            resampler.update_clock_loop(4.0, 4.0);
        }
        // Inject a single-tick sudden packet arrival dip to 0.0
        resampler.update_clock_loop(0.0, 4.0);
        // EMA smoothing must prevent min_queue_frames_window from crashing directly to 0.0
        assert!(
            resampler.min_queue_frames_window > 2.0,
            "EMA must cushion against single-tick packet arrival dips: {}",
            resampler.min_queue_frames_window
        );
    }
}
