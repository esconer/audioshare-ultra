use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Queue errors below this are sub-frame phase beat between packet arrival
/// and DAC callbacks, not clock drift. Feeding them into the integral term
/// slowly winds it up and releases the accumulated correction as a ratio step.
const CLOCK_LOOP_INTEGRAL_DEADBAND_SUBFRAMES: f64 = 0.15;

/// 1. Jitter-Immune Windowed-Min DLL + 4-Point Hermite Cubic Fractional Resampler
pub struct ClockDriftResampler {
    phase: f64,
    /// Conversion from the 48 kHz network stream to the output device.
    ///
    /// This is the *input frames per output frame* ratio: an output device
    /// running at 44.1 kHz needs `48_000 / 44_100`, while a 48 kHz device
    /// needs `1.0`. `ratio` below is always `base_ratio * (1 + drift)`.
    pub base_ratio: f64,
    pub ratio: f64,
    hist_l: [f32; 4],
    hist_r: [f32; 4],
    /// Running sum of (observed queue depth - clock setpoint) over the current
    /// 200-tick window. A windowed mean of the *error* cancels periodic HAL
    /// burst sawtooths while tracking both positive and negative drift.
    err_sum_window: f64,
    window_ticks: u32,
    integral_err: f64,
    // 2nd-order Butterworth anti-aliasing LPF. Active only when the input rate
    // exceeds the output rate by >2% (44.1 kHz A2DP, 96 kHz -> 48 kHz capture);
    // completely bypassed (zero CPU cost) on native 48 kHz streams.
    aa_active: bool,
    aa_b0: f32,
    aa_b1: f32,
    aa_b2: f32,
    aa_a1: f32,
    aa_a2: f32,
    aa_z1_l: f32,
    aa_z2_l: f32,
    aa_z1_r: f32,
    aa_z2_r: f32,
}

impl ClockDriftResampler {
    pub fn new() -> Self {
        Self {
            phase: 0.0,
            base_ratio: 1.0,
            ratio: 1.0,
            hist_l: [0.0; 4],
            hist_r: [0.0; 4],
            err_sum_window: 0.0,
            window_ticks: 0,
            integral_err: 0.0,
            aa_active: false,
            aa_b0: 1.0,
            aa_b1: 0.0,
            aa_b2: 0.0,
            aa_a1: 0.0,
            aa_a2: 0.0,
            aa_z1_l: 0.0,
            aa_z2_l: 0.0,
            aa_z1_r: 0.0,
            aa_z2_r: 0.0,
        }
    }

    /// Configure exact input -> output sample rates. Automatically enables the
    /// Butterworth anti-aliasing filter only when downsampling (`ratio > 1.02`).
    #[inline]
    pub fn set_sample_rates(&mut self, input_rate_hz: u32, output_rate_hz: u32) {
        let in_r = input_rate_hz.clamp(8_000, 384_000) as f64;
        let out_r = output_rate_hz.clamp(8_000, 384_000) as f64;
        let new_base = in_r / out_r;
        if (new_base - self.base_ratio).abs() > 1e-9 {
            self.base_ratio = new_base;
            self.ratio = new_base;
            self.err_sum_window = 0.0;
            self.window_ticks = 0;
            self.integral_err = 0.0;
            self.configure_anti_aliasing(new_base);
        }
    }

    fn configure_anti_aliasing(&mut self, base_ratio: f64) {
        self.aa_z1_l = 0.0;
        self.aa_z2_l = 0.0;
        self.aa_z1_r = 0.0;
        self.aa_z2_r = 0.0;
        if base_ratio <= 1.02 {
            self.aa_active = false;
            return;
        }
        // Cutoff at 0.45 * output_rate expressed in input-rate normalized cycles.
        let norm_fc = (0.45 / base_ratio).clamp(0.02, 0.46) as f32;
        let w0 = std::f32::consts::TAU * norm_fc;
        let cos_w0 = w0.cos();
        let alpha = w0.sin() * std::f32::consts::FRAC_1_SQRT_2;
        let inv_a0 = 1.0 / (1.0 + alpha);
        self.aa_b0 = ((1.0 - cos_w0) * 0.5) * inv_a0;
        self.aa_b1 = (1.0 - cos_w0) * inv_a0;
        self.aa_b2 = self.aa_b0;
        self.aa_a1 = (-2.0 * cos_w0) * inv_a0;
        self.aa_a2 = (1.0 - alpha) * inv_a0;
        self.aa_active = true;
    }

    #[inline(always)]
    fn filter_input_pair(&mut self, in_l: f32, in_r: f32) -> (f32, f32) {
        if !self.aa_active {
            return (in_l, in_r);
        }
        const ANTI_DENORMAL: f32 = 1.0e-25;
        let xl = in_l + ANTI_DENORMAL;
        let yl = self.aa_b0 * xl + self.aa_z1_l;
        self.aa_z1_l = self.aa_b1 * xl - self.aa_a1 * yl + self.aa_z2_l;
        self.aa_z2_l = self.aa_b2 * xl - self.aa_a2 * yl;

        let xr = in_r + ANTI_DENORMAL;
        let yr = self.aa_b0 * xr + self.aa_z1_r;
        self.aa_z1_r = self.aa_b1 * xr - self.aa_a1 * yr + self.aa_z2_r;
        self.aa_z2_r = self.aa_b2 * xr - self.aa_a2 * yr;
        (yl, yr)
    }

    /// Point the resampler at the sample rate AAudio actually opened.
    ///
    /// Android may route output through a 44.1 kHz A2DP link even though the
    /// network stream is 48 kHz. Forcing 48 kHz into that route makes
    /// AudioFlinger resample through the normal mixer (where OEM effects such
    /// as Dolby live). Letting AAudio pick the route's native rate keeps the
    /// stream eligible for the low-latency mixer; this resampler then performs
    /// the 48 kHz-to-native conversion and the clock-drift correction in one
    /// pass. Changing the rate resets the drift controller because its queue
    /// observations were expressed in the previous output rate.
    #[inline]
    pub fn set_output_sample_rate(&mut self, output_rate_hz: u32) {
        self.set_sample_rates(crate::SAMPLE_RATE, output_rate_hz);
    }

    /// Convert an output-sample count into 2.5 ms network subframes.
    ///
    /// The decoupler is filled at the output rate, so `240` samples are one
    /// subframe only when that rate is 48 kHz. At 44.1 kHz one subframe is
    /// `240 / 1.088...` ~= 220.5 samples; without this conversion the client
    /// would overestimate its buffer depth and let the DAC run dry.
    #[inline]
    pub fn samples_to_subframes(&self, samples: usize) -> f32 {
        (samples as f64 * self.base_ratio / crate::SUBFRAME_STEREO_SAMPLES as f64) as f32
    }

    /// Convert 2.5 ms subframes into an output-sample count for the current
    /// output rate. Rounded to a whole stereo frame so L/R stay aligned.
    #[inline]
    pub fn subframes_to_samples(&self, subframes: usize) -> usize {
        let samples = (subframes as f64 * crate::SUBFRAME_STEREO_SAMPLES as f64) / self.base_ratio;
        let rounded = (samples.round() as usize) & !1;
        if subframes == 0 {
            0
        } else {
            rounded.max(2)
        }
    }

    /// Observe current queue depth every 2.5ms subframe.
    /// Uses a 200-subframe (500ms) error-relative windowed mean so periodic HAL
    /// bursts cancel out while tracking both positive and negative crystal drift.
    #[inline]
    pub fn update_clock_loop(&mut self, current_queued_subframes: f32, target_min_subframes: f32) {
        self.err_sum_window += (current_queued_subframes - target_min_subframes) as f64;
        self.window_ticks += 1;

        if self.window_ticks >= 200 {
            let err = self.err_sum_window / (self.window_ticks as f64);

            // Hold the learned crystal offset inside the deadband rather than
            // decaying it (the old `*= 0.9` erased the offset every 500 ms).
            // Anti-windup clamps per-window steps; +/-100.0 gives full
            // +/-500 ppm steady-state integral authority for real crystals.
            if err.abs() > CLOCK_LOOP_INTEGRAL_DEADBAND_SUBFRAMES {
                let step = err.clamp(-1.0, 1.0) * 0.6;
                self.integral_err = (self.integral_err + step).clamp(-100.0, 100.0);
            }

            // Dual-slope catch-up: +/- 500 ppm for normal drift, up to +2500 ppm after a Wi-Fi burst spike
            let (kp, max_clamp) = if err > 2.0 {
                (0.00025, 0.0025)
            } else {
                (0.00012, 0.0005)
            };
            let corr = (kp * err + 0.000005 * self.integral_err).clamp(-0.0005, max_clamp);
            let target_ratio = self.base_ratio * (1.0 + corr);

            if err > 2.0 {
                self.ratio = target_ratio;
            } else {
                self.ratio = 0.3 * self.ratio + 0.7 * target_ratio;
            }
            self.err_sum_window = 0.0;
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

    /// Exact number of input stereo frames required by [`Self::process_pull`]
    /// to produce `out_frames` output stereo frames at the current phase and
    /// ratio.
    #[inline]
    pub fn needed_input_frames(&self, out_frames: usize) -> usize {
        let ratio = self.ratio.max(1e-9);
        (self.phase + out_frames as f64 * ratio).floor().max(0.0) as usize
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
                let (fl, fr) = self.filter_input_pair(in_stereo[in_idx * 2], in_stereo[in_idx * 2 + 1]);
                self.hist_l[3] = fl;
                self.hist_r[3] = fr;
                in_idx += 1;
                self.phase -= 1.0;
            }
        }
        out_idx
    }

    /// Pull-mode resampler: consumes `in_stereo` (sized via
    /// [`Self::needed_input_frames`]) and fills `out_stereo`, emitting any
    /// trailing output frames satisfied by the updated history window after the
    /// final input frame is shifted in.
    pub fn process_pull(&mut self, in_stereo: &[f32], out_stereo: &mut [f32]) -> usize {
        let max_out_frames = out_stereo.len() / 2;
        let mut out_idx = self.process(in_stereo, out_stereo);
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
        out_idx
    }

    /// Pull-mode resampler driven directly by a [`BurstDecouplerRing`].
    ///
    /// Pops an input stereo frame from `ring` if and only if `self.phase >= 1.0`,
    /// eliminating any IEEE-754 rounding discrepancy between `floor(phase + N*ratio)`
    /// and repeated `phase += ratio` additions. Guarantees that every sample in
    /// `out_stereo` is filled and zero popped input frames are ever discarded.
    pub fn process_from_ring(
        &mut self,
        ring: &BurstDecouplerRing,
        dither_rng: &mut u32,
        out_stereo: &mut [f32],
    ) {
        let out_frames = out_stereo.len() / 2;
        let mut in_frame = [0.0f32; 2];
        for out_idx in 0..out_frames {
            while self.phase >= 1.0 {
                ring.pop_exact_or_dither(&mut in_frame, dither_rng);
                self.hist_l.copy_within(1..4, 0);
                self.hist_r.copy_within(1..4, 0);
                let (fl, fr) = self.filter_input_pair(in_frame[0], in_frame[1]);
                self.hist_l[3] = fl;
                self.hist_r[3] = fr;
                self.phase -= 1.0;
            }
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
            self.phase += self.ratio;
        }
        while self.phase >= 1.0 {
            ring.pop_exact_or_dither(&mut in_frame, dither_rng);
            self.hist_l.copy_within(1..4, 0);
            self.hist_r.copy_within(1..4, 0);
            let (fl, fr) = self.filter_input_pair(in_frame[0], in_frame[1]);
            self.hist_l[3] = fl;
            self.hist_r[3] = fr;
            self.phase -= 1.0;
        }
    }
}

/// 2. Wait-Free SPSC Sample FIFO (Decouples 120-frame network quantum from Android SoC getFramesPerBurst())
const FIFO_CAPACITY: usize = 32768; // Power of 2 (16384 stereo frames = 341 ms @ 48 kHz for full BT Group Sync)
const FIFO_MASK: usize = FIFO_CAPACITY - 1;

#[repr(align(64))]
struct CachePadded<T>(T);

pub struct BurstDecouplerRing {
    write_idx: CachePadded<AtomicUsize>,
    read_idx: CachePadded<AtomicUsize>,
    buf: UnsafeCell<[f32; FIFO_CAPACITY]>,
}

unsafe impl Sync for BurstDecouplerRing {}

impl BurstDecouplerRing {
    pub fn new() -> Self {
        Self {
            write_idx: CachePadded(AtomicUsize::new(0)),
            read_idx: CachePadded(AtomicUsize::new(0)),
            buf: UnsafeCell::new([0.0; FIFO_CAPACITY]),
        }
    }

    #[inline]
    pub fn available_samples(&self) -> usize {
        let w = self.write_idx.0.load(Ordering::Acquire);
        let r = self.read_idx.0.load(Ordering::Acquire);
        w.wrapping_sub(r)
    }

    /// Total interleaved samples the ring can hold. Callers that translate a
    /// time-based target (subframes) into samples must clamp to this, or a
    /// high-rate output device could produce a watermark the ring can never
    /// reach and keep the prebuffer loop running forever.
    #[inline]
    pub fn capacity_samples(&self) -> usize {
        FIFO_CAPACITY
    }

    #[inline]
    pub fn push_samples(&self, samples: &[f32]) -> usize {
        let w = self.write_idx.0.load(Ordering::Relaxed);
        let r = self.read_idx.0.load(Ordering::Acquire);
        let free = FIFO_CAPACITY - w.wrapping_sub(r);
        // The ring is stereo-interleaved. An odd write count would shift every
        // subsequent Left/Right pair by one sample, so always write whole frames.
        let to_write = samples.len().min(free) & !1;

        let buf_ptr = self.buf.get() as *mut f32;
        for (i, &s) in samples[..to_write].iter().enumerate() {
            unsafe {
                *buf_ptr.add((w.wrapping_add(i)) & FIFO_MASK) = s;
            }
        }
        self.write_idx.0
            .store(w.wrapping_add(to_write), Ordering::Release);
        to_write
    }

    /// Pull exact `out.len()` samples required by Android AAudio hardware burst (e.g., 96, 128, or 192 stereo frames)
    #[inline]
    pub fn pop_exact_or_dither(&self, out: &mut [f32], dither_rng: &mut u32) -> bool {
        let r = self.read_idx.0.load(Ordering::Relaxed);
        let w = self.write_idx.0.load(Ordering::Acquire);
        let avail = w.wrapping_sub(r);
        // The ring is stereo-interleaved. Advancing by an odd sample count
        // would make every later pull start on a Right sample, permanently
        // swapping channels. Consume whole frames only and pad a trailing odd
        // slot with silence.
        let req_even = out.len() & !1;

        if req_even > 0 && avail >= req_even {
            let buf_ptr = self.buf.get() as *const f32;
            for (i, dst) in out[..req_even].iter_mut().enumerate() {
                unsafe {
                    *dst = *buf_ptr.add((r.wrapping_add(i)) & FIFO_MASK);
                }
            }
            if req_even < out.len() {
                out[req_even] = 0.0;
            }
            self.read_idx.0
                .store(r.wrapping_add(req_even), Ordering::Release);
            true
        } else if req_even > 0 && (avail & !1) > 0 {
            // Partial underrun: copy what is left, but cross-fade the final
            // frames to zero before the exponential tail. A callback that is
            // only one frame short would otherwise leave the output at ~98.5%
            // amplitude, and the next (empty) callback starts at -90 dBFS
            // comfort dither: a full-scale step into the DAC.
            let safe_avail = (avail & !1).min(req_even); // Keep stereo L/R alignment
            let fade_frames = (safe_avail / 2).min(16);
            let fade_samples = fade_frames * 2;
            let fade_start = safe_avail - fade_samples;
            let buf_ptr = self.buf.get() as *const f32;
            let mut last_l = 0.0f32;
            let mut last_r = 0.0f32;
            for i in 0..safe_avail {
                let mut s = unsafe { *buf_ptr.add((r.wrapping_add(i)) & FIFO_MASK) };
                if i >= fade_start {
                    let frame = (i - fade_start) / 2;
                    // Raised-cosine fade: gain 1.0 on the first frame and
                    // exactly 0.0 on the last, with no slope discontinuity.
                    let gain = if fade_frames > 1 {
                        let phase = core::f32::consts::PI * frame as f32 / (fade_frames - 1) as f32;
                        0.5 * (1.0 + phase.cos())
                    } else {
                        0.0
                    };
                    s *= gain;
                }
                out[i] = s;
                if i & 1 == 0 {
                    last_l = s;
                } else {
                    last_r = s;
                }
            }
            self.read_idx.0
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
    #[inline]
    fn gain_db_to_linear(gain_db: f32) -> f32 {
        if gain_db <= -60.0 {
            0.0
        } else {
            10.0f32.powf(gain_db.clamp(-60.0, 24.0) / 20.0)
        }
    }

    pub fn new(gain_db: f32) -> Self {
        let g = Self::gain_db_to_linear(gain_db);
        Self {
            current_gain_linear: g,
            target_gain_linear: g,
            envelope: 0.0,
        }
    }

    pub fn set_gain_db(&mut self, gain_db: f32) {
        self.target_gain_linear = Self::gain_db_to_linear(gain_db);
    }

    #[inline]
    pub fn process_in_place(&mut self, stereo_samples: &mut [f32]) {
        const THRESHOLD: f32 = 0.85; // -1.4 dBFS soft-knee start
        const CEILING: f32 = 0.988; // -0.1 dBFS hard true-peak ceiling
        const ATTACK: f32 = 0.25;
        const RELEASE: f32 = 0.0005;

        for frame in stereo_samples.chunks_exact_mut(2) {
            self.current_gain_linear += 0.01 * (self.target_gain_linear - self.current_gain_linear);
            // A non-finite capture sample (driver glitch, decoder damage) must
            // not propagate NaN/Inf into the ring, DAC, or envelope follower.
            let in_l = if frame[0].is_finite() { frame[0] } else { 0.0 };
            let in_r = if frame[1].is_finite() { frame[1] } else { 0.0 };
            let mut l = in_l * self.current_gain_linear;
            let mut r = in_r * self.current_gain_linear;
            let peak = l.abs().max(r.abs());

            if peak > self.envelope {
                self.envelope += ATTACK * (peak - self.envelope);
            } else {
                self.envelope += RELEASE * (peak - self.envelope);
            }

            // At unity gain the limiter must be bit-transparent below the
            // ceiling. The soft knee exists to absorb pre-amp boost, not to
            // re-compress material that already fits; applying it at 0 dB
            // audibly squashes peaks between -1.4 dBFS and -0.1 dBFS. Keep
            // the envelope updated above so later gain changes start from the
            // real signal level, and still engage the knee for any sample
            // that would actually exceed the ceiling.
            let unity_gain = (self.current_gain_linear - 1.0).abs() <= 1e-4
                && (self.target_gain_linear - 1.0).abs() <= 1e-4;
            let det = self.envelope.max(peak);
            if (!unity_gain || peak > CEILING) && det > THRESHOLD {
                let over = det - THRESHOLD;
                let compressed =
                    THRESHOLD + (CEILING - THRESHOLD) * (over / (over + (CEILING - THRESHOLD)));
                let scale = compressed / det;
                l *= scale;
                r *= scale;
            }

            frame[0] = l.clamp(-CEILING, CEILING);
            frame[1] = r.clamp(-CEILING, CEILING);
        }
    }
}

/// Transparent safety soft-clipper applied *after* fractional resampling so
/// 4-point Hermite inter-sample overshoots (> 0.96) smoothly round off below
/// 0.999 instead of hard-clipping in AAudio/CPAL. Bit-transparent <= 0.96.
#[inline]
pub fn soft_limit_post_resample(samples: &mut [f32]) {
    const KNEE: f32 = 0.96;
    const HEADROOM: f32 = 0.039; // asymptotic ceiling = 0.999
    for s in samples.iter_mut() {
        let a = s.abs();
        if a > KNEE {
            let over = a - KNEE;
            let shaped = KNEE + HEADROOM * (over / (over + HEADROOM));
            *s = shaped.copysign(*s);
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
        // Keep non-finite capture samples out of the resampler history and the
        // downstream limiter; one NaN would otherwise survive every later
        // interpolation stage.
        out_stereo[i * 2] = if l.is_finite() { l } else { 0.0 };
        out_stereo[i * 2 + 1] = if r.is_finite() { r } else { 0.0 };
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
        // The ring cross-fades the last 16 stereo frames to zero so the next
        // callback starts from silence instead of a full-scale step.
        assert!(ring.pop_exact_or_dither(&mut aaudio_burst, &mut rng));
        assert_eq!(aaudio_burst[0], 0.42);
        assert_eq!(aaudio_burst[159], 0.42); // last sample before the fade
        assert!((aaudio_burst[160] - 0.42).abs() < 1.0e-6); // fade starts at unity
        assert!(aaudio_burst[191].abs() < 1.0e-6); // last frame faded to zero
        assert!(aaudio_burst[192..].iter().all(|&s| s == 0.0));
        assert_eq!(ring.available_samples(), 0);

        // Fourth pull: 0 samples remaining -> comfort dither injected and returns false
        assert!(!ring.pop_exact_or_dither(&mut aaudio_burst, &mut rng));
    }

    #[test]
    fn test_fifo_capacity_holds_64_subframes_at_96khz() {
        assert_eq!(FIFO_CAPACITY, 32_768);
        assert!(FIFO_CAPACITY.is_power_of_two());
        let ring = BurstDecouplerRing::new();
        assert_eq!(ring.capacity_samples(), 32_768);

        let mut resampler = ClockDriftResampler::new();

        // 48 kHz output: base_ratio 1.0, so 64 subframes are 15,360 samples.
        resampler.set_output_sample_rate(48_000);
        assert_eq!(resampler.base_ratio, 1.0);
        assert_eq!(resampler.subframes_to_samples(64), 15_360);
        assert!((resampler.samples_to_subframes(32_768) - 136.533_33).abs() < 1e-3);
        assert!(resampler.subframes_to_samples(64) + 240 <= ring.capacity_samples());

        // 96 kHz output: base_ratio 0.5, so 64 subframes are 30,720 output
        // samples and the ring holds 68.27 subframe equivalents. 69
        // subframes (33,120 samples) would overflow.
        resampler.set_output_sample_rate(96_000);
        assert_eq!(resampler.base_ratio, 0.5);
        assert_eq!(resampler.subframes_to_samples(64), 30_720);
        assert!((resampler.samples_to_subframes(32_768) - 68.266_67).abs() < 1e-3);
        assert!(resampler.subframes_to_samples(69) > ring.capacity_samples());

        // Simulate the client refill loop: push one output subframe at a
        // time until the 64-subframe watermark is reached. Every push must
        // be accepted whole, and one more subframe must still fit.
        let watermark = resampler
            .subframes_to_samples(64)
            .min(ring.capacity_samples() - 2);
        let subframe_samples = resampler.subframes_to_samples(1);
        assert_eq!(subframe_samples, 480);
        let chunk = vec![0.25f32; subframe_samples];
        let mut pushes = 0;
        while ring.available_samples() < watermark {
            assert_eq!(ring.push_samples(&chunk), chunk.len());
            pushes += 1;
        }
        assert_eq!(pushes, 64);
        assert_eq!(ring.available_samples(), watermark);
        assert!(watermark + subframe_samples <= ring.capacity_samples());
    }

    #[test]
    fn test_anti_aliasing_filter_only_when_downsampling() {
        let mut r = ClockDriftResampler::new();
        r.set_sample_rates(48_000, 48_000);
        assert!(!r.aa_active, "native 48 kHz must bypass the LPF");
        r.set_sample_rates(48_000, 44_100);
        assert!(r.aa_active, "48->44.1 kHz downsampling must enable the LPF");
        r.set_sample_rates(96_000, 48_000);
        assert!(r.aa_active, "96->48 kHz capture must enable the LPF");
        // A 2 kHz tone through the filter path must stay finite and bounded.
        let mut out = [0.0f32; 64];
        let mut input = [0.0f32; 128];
        for i in 0..64 {
            let v = (i as f32 * 0.5).sin() * 0.5;
            input[i * 2] = v;
            input[i * 2 + 1] = v;
        }
        r.process(&input, &mut out);
        assert!(out.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
    }

    #[test]
    fn test_soft_limit_post_resample_bounds_overshoot() {
        let mut s = [1.23f32, -1.23, 0.5, -0.5, 0.96, -0.96];
        soft_limit_post_resample(&mut s);
        assert!(s.iter().all(|v| v.abs() < 0.999));
        assert_eq!(s[4], 0.96); // bit-transparent at/below the knee
        assert_eq!(s[5], -0.96);
        assert!(s[0] > 0.96 && s[0] < 0.999);
    }

    #[test]
    fn test_burst_ring_indices_are_cache_line_padded() {
        assert!(std::mem::align_of::<CachePadded<AtomicUsize>>() >= 64);
        assert!(std::mem::size_of::<CachePadded<AtomicUsize>>() >= 64);
        let ring = BurstDecouplerRing::new();
        let off_w = std::mem::offset_of!(BurstDecouplerRing, write_idx);
        let off_r = std::mem::offset_of!(BurstDecouplerRing, read_idx);
        assert_ne!(off_w / 64, off_r / 64, "write/read indices share a cache line");
        let _ = ring;
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
    fn test_true_peak_limiter_unity_gain_is_transparent_below_ceiling() {
        let mut limiter = TruePeakLimiter::new(0.0);
        let mut signal = [0.0f32; 480];
        for (i, s) in signal.iter_mut().enumerate() {
            *s = if i % 2 == 0 { 0.9 } else { -0.9 };
        }
        limiter.process_in_place(&mut signal);

        // 0.9 is below the -0.1 dBFS ceiling, so a 0 dB pre-amp must not
        // touch it. The old soft knee compressed anything above 0.85.
        for &s in &signal {
            assert_eq!(s.abs(), 0.9, "unity gain must be bit-transparent: {}", s);
        }
    }

    #[test]
    fn test_true_peak_limiter_unity_gain_still_catches_samples_above_ceiling() {
        let mut limiter = TruePeakLimiter::new(0.0);
        let mut signal = [1.0f32; 480];
        limiter.process_in_place(&mut signal);

        for &s in &signal {
            assert!(
                s.abs() <= 0.989,
                "unity gain must still honour ceiling: {}",
                s
            );
        }
        // After the envelope tracks the full-scale input the soft knee must
        // pull it below the hard clamp, so this is not just clipping.
        assert!(
            signal[signal.len() - 1].abs() < 0.96,
            "soft knee should engage for overs: {}",
            signal[signal.len() - 1]
        );
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
    fn test_resampler_follows_native_output_sample_rate() {
        let mut resampler = ClockDriftResampler::new();
        resampler.set_output_sample_rate(44_100);

        // 48 kHz input to 44.1 kHz output: 240 input frames produce ~220.5
        // output frames, not 240.
        let expected_base = 48_000.0 / 44_100.0;
        assert!((resampler.base_ratio - expected_base).abs() < 1e-12);
        assert!((resampler.ratio - expected_base).abs() < 1e-12);

        let input = [0.1f32; 480];
        let mut output = [0.0f32; 4096];
        let out_frames = resampler.process(&input, &mut output);
        assert!(
            (219..=221).contains(&out_frames),
            "44.1 kHz output frame count: {}",
            out_frames
        );

        // One 2.5 ms subframe at 44.1 kHz must stay one subframe in the
        // client's buffer accounting, and the sample counts must be even.
        assert_eq!(resampler.subframes_to_samples(1), 220);
        assert_eq!(resampler.subframes_to_samples(2), 440);
        assert_eq!(resampler.subframes_to_samples(0), 0);
        let subframes = resampler.samples_to_subframes(220);
        assert!((subframes - 1.0).abs() < 0.01, "got {}", subframes);

        // A high-rate USB DAC needs more output samples than the input
        // subframe provides; the old +16 sample buffer would have truncated it.
        resampler.set_output_sample_rate(96_000);
        assert_eq!(resampler.subframes_to_samples(1), 480);
        let out_frames = resampler.process(&input, &mut output);
        assert!(
            (479..=481).contains(&out_frames),
            "96 kHz output frame count: {}",
            out_frames
        );

        // Re-applying the same rate must not discard the current drift
        // correction or reset the phase history.
        let corrected = resampler.base_ratio * 1.0001;
        resampler.ratio = corrected;
        resampler.set_output_sample_rate(96_000);
        assert!((resampler.ratio - corrected).abs() < 1e-12);
    }

    #[test]
    fn test_clock_loop_correction_is_relative_to_base_ratio() {
        let mut resampler = ClockDriftResampler::new();
        resampler.set_output_sample_rate(44_100);
        let base = resampler.base_ratio;
        for _ in 0..200 {
            resampler.update_clock_loop(3.0, 2.0);
        }
        let correction = resampler.ratio / base - 1.0;
        assert!(
            correction > 0.0 && correction <= 0.0005,
            "correction must be reported around the output base ratio: {}",
            correction
        );
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
    fn test_clock_drift_windowed_mean_rejects_single_tick_dip() {
        let mut resampler = ClockDriftResampler::new();
        // A locked stream with a single-tick packet-arrival dip must not be
        // mistaken for drift: the error-relative windowed mean divides the
        // -4 subframe dip across 200 ticks => -0.02, inside the deadband.
        for _ in 0..200 {
            resampler.update_clock_loop(4.0, 4.0);
        }
        resampler.update_clock_loop(0.0, 4.0);
        for _ in 0..199 {
            resampler.update_clock_loop(4.0, 4.0);
        }
        let corr_ppm = (resampler.ratio / resampler.base_ratio - 1.0) * 1e6;
        assert!(
            corr_ppm.abs() < 20.0,
            "single-tick dip must not move the clock lock: {} ppm",
            corr_ppm
        );
    }

    #[test]
    fn test_clock_loop_smooths_normal_ratio_changes() {
        let mut resampler = ClockDriftResampler::new();
        for _ in 0..200 {
            resampler.update_clock_loop(3.0, 2.0);
        }
        let first_ratio = resampler.ratio;
        assert!(first_ratio > 1.0);
        // Target correction for err=1.0 is ~52.5 ppm. The first window must
        // apply only part of it instead of jumping to the full target.
        assert!(
            first_ratio < 1.000_10,
            "normal ratio correction must slew, got {}",
            first_ratio
        );

        for _ in 0..200 {
            resampler.update_clock_loop(3.0, 2.0);
        }
        assert!(
            resampler.ratio > first_ratio,
            "ratio must converge toward the target over successive windows"
        );
    }

    #[test]
    fn test_clock_loop_integral_deadband_ignores_phase_beat() {
        let mut resampler = ClockDriftResampler::new();
        // A 0.05-subframe error is normal packet/DAC phase beat, not drift.
        for _ in 0..400 {
            resampler.update_clock_loop(2.05, 2.0);
        }
        assert_eq!(
            resampler.integral_err, 0.0,
            "sub-deadband error must not wind up the integral term"
        );
    }

    #[test]
    fn test_burst_decoupler_near_full_underrun_fades_to_zero() {
        let ring = BurstDecouplerRing::new();
        // One stereo frame short of a full 384-sample AAudio burst.
        let input = [0.5f32; 382];
        assert_eq!(ring.push_samples(&input), 382);

        let mut out = [0.0f32; 384];
        let mut rng = 7u32;
        assert!(ring.pop_exact_or_dither(&mut out, &mut rng));
        assert_eq!(out[0], 0.5);
        // The old 0.985 tail left the final sample at ~98.5% amplitude and
        // the next empty callback clicked at -90 dBFS comfort dither.
        assert_eq!(out[383], 0.0);
        assert_eq!(ring.available_samples(), 0);

        assert!(!ring.pop_exact_or_dither(&mut out, &mut rng));
        assert!(out.iter().all(|sample| sample.abs() <= 0.0001));
    }

    #[test]
    fn test_burst_decoupler_odd_push_preserves_stereo_alignment() {
        let ring = BurstDecouplerRing::new();
        // An odd caller slice must not write a half frame.
        assert_eq!(ring.push_samples(&[1.0, 2.0, 3.0]), 2);
        assert_eq!(ring.available_samples(), 2);

        assert_eq!(ring.push_samples(&[4.0, 5.0]), 2);
        assert_eq!(ring.available_samples(), 4);

        let mut out = [0.0f32; 4];
        let mut rng = 1u32;
        assert!(ring.pop_exact_or_dither(&mut out, &mut rng));
        assert_eq!(out, [1.0, 2.0, 4.0, 5.0]);
    }

    #[test]
    fn test_burst_decoupler_odd_pop_preserves_stereo_alignment() {
        let ring = BurstDecouplerRing::new();
        assert_eq!(ring.push_samples(&[1.0, 2.0, 3.0, 4.0]), 4);

        // A caller asking for an odd sample count must not advance the read
        // cursor by a half frame; the unpaired trailing sample is dropped and
        // the next pull still starts on a Left sample.
        let mut out = [0.0f32; 3];
        let mut rng = 1u32;
        assert!(ring.pop_exact_or_dither(&mut out, &mut rng));
        assert_eq!(&out[..2], &[1.0, 2.0]);
        assert_eq!(out[2], 0.0);
        assert_eq!(ring.available_samples(), 2);

        assert_eq!(ring.push_samples(&[5.0, 6.0]), 2);
        let mut out4 = [0.0f32; 4];
        assert!(ring.pop_exact_or_dither(&mut out4, &mut rng));
        assert_eq!(out4, [3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn test_true_peak_limiter_recovers_from_non_finite_input() {
        let mut limiter = TruePeakLimiter::new(0.0);
        let mut poisoned = [f32::NAN, f32::INFINITY];
        limiter.process_in_place(&mut poisoned);
        assert!(
            poisoned.iter().all(|sample| sample.is_finite()),
            "non-finite input must not reach the output"
        );

        let mut normal = [0.25f32, -0.25, 0.5, -0.5];
        limiter.process_in_place(&mut normal);
        assert!(
            normal.iter().all(|sample| sample.is_finite()),
            "a single NaN frame must not poison the limiter envelope"
        );
        assert!((normal[0] - 0.25).abs() < 1e-3);
        assert!((normal[1] + 0.25).abs() < 1e-3);
    }

    #[test]
    fn test_downmix_to_stereo_sanitizes_non_finite_channels() {
        let input = [f32::NAN, 0.5, f32::INFINITY, -0.25, 0.1, f32::NEG_INFINITY];
        let mut out = [0.0f32; 6];
        downmix_to_stereo(&input, 2, &mut out);
        assert!(
            out.iter().all(|sample| sample.is_finite()),
            "non-finite capture samples must be replaced with silence"
        );
        assert_eq!(out[1], 0.5);
        assert_eq!(out[3], -0.25);
        assert_eq!(out[4], 0.1);
    }

    #[test]
    fn test_process_pull_emits_exact_requested_frames_across_rates() {
        for &rate in &[44_100, 48_000, 88_200, 96_000, 192_000] {
            let mut resampler = ClockDriftResampler::new();
            resampler.set_output_sample_rate(rate);

            let req_frames = 240;
            let mut output = vec![0.0f32; req_frames * 2];

            for _ in 0..20 {
                let needed = resampler.needed_input_frames(req_frames);
                let input = vec![0.5f32; needed * 2];
                let produced = resampler.process_pull(&input, &mut output);
                assert_eq!(
                    produced, req_frames,
                    "Rate {} failed: requested {} frames, produced {}",
                    rate, req_frames, produced
                );
            }
        }
    }

    #[test]
    fn test_process_from_ring_exact_frames() {
        for &rate in &[44_100, 48_000, 88_200, 96_000, 192_000] {
            let mut resampler = ClockDriftResampler::new();
            resampler.set_output_sample_rate(rate);
            let ring = BurstDecouplerRing::new();

            // Push initial buffer into ring
            let subframe = [0.25f32; 240];
            for _ in 0..10 {
                ring.push_samples(&subframe);
            }

            let mut rng = 12345u32;
            let req_frames = 441; // Non-trivial frame size (10ms @ 44.1kHz)
            let mut output = vec![0.0f32; req_frames * 2];

            for _ in 0..5 {
                resampler.process_from_ring(&ring, &mut rng, &mut output);
                assert!(output.iter().all(|s| s.is_finite()));
            }
        }
    }
}
