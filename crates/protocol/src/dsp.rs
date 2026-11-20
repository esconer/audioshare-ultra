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
                self.integral_err = (self.integral_err + step).clamp(-80.0, 80.0);
            }

            // Symmetric +/- 400 ppm crystal-drift authority with smooth slewing.
            // Removes the +2500 ppm spike jump that caused audible pitch warble
            // after Wi-Fi bursts; large backlogs are trimmed with a declick splice.
            const KP: f64 = 0.00012;
            const MAX_DRIFT_RATIO: f64 = 0.0004;
            let corr = (KP * err + 0.000005 * self.integral_err)
                .clamp(-MAX_DRIFT_RATIO, MAX_DRIFT_RATIO);
            let target_ratio = self.base_ratio * (1.0 + corr);

            self.ratio = 0.3 * self.ratio + 0.7 * target_ratio;
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

    /// Shift exactly one interleaved stereo frame from the batched ring
    /// scratch into the resampler history.
    ///
    /// `scratch_pos`/`scratch_len` track the unconsumed tail of the last ring
    /// read, and `to_pop` the number of input frames this call still intends to
    /// pull. Requesting only what will actually be consumed (in <=64-frame
    /// chunks) is essential: the scratch is stack-local, so any surplus read
    /// would be silently dropped when `process_from_ring` returns.
    #[inline(always)]
    fn shift_one_from_ring(
        &mut self,
        ring: &BurstDecouplerRing,
        dither_rng: &mut u32,
        scratch: &mut [f32; 128],
        scratch_pos: &mut usize,
        scratch_len: &mut usize,
        to_pop: &mut usize,
    ) {
        if *scratch_pos >= *scratch_len {
            let frames = (*to_pop).min(64);
            let read_len = if frames > 0 {
                frames * 2
            } else {
                // Defensive FP-boundary guard: the phase-driven loop asked for
                // one more frame than `needed_input_frames` predicted. Never
                // reuse stale scratch; pull exactly that one frame instead.
                2
            };
            // `pop_exact_or_dither` leaves the whole requested slice populated
            // on every path: a full read copies real data, a partial read fades
            // the tail and zero-fills, and a fully empty ring gets -90 dBFS
            // comfort dither. Its return value is ignored here exactly as it
            // was with the old per-frame pops (fade/dither behavior preserved).
            ring.pop_exact_or_dither(&mut scratch[..read_len], dither_rng);
            *to_pop = (*to_pop).saturating_sub(frames);
            *scratch_pos = 0;
            *scratch_len = read_len;
        }
        let in_l = scratch[*scratch_pos];
        let in_r = scratch[*scratch_pos + 1];
        *scratch_pos += 2;
        self.hist_l.copy_within(1..4, 0);
        self.hist_r.copy_within(1..4, 0);
        let (fl, fr) = self.filter_input_pair(in_l, in_r);
        self.hist_l[3] = fl;
        self.hist_r[3] = fr;
    }

    /// Pull-mode resampler driven directly by a [`BurstDecouplerRing`].
    ///
    /// Pops input from `ring` if and only if `self.phase >= 1.0`,
    /// eliminating any IEEE-754 rounding discrepancy between `floor(phase + N*ratio)`
    /// and repeated `phase += ratio` additions. Guarantees that every sample in
    /// `out_stereo` is filled and zero popped input frames are ever discarded.
    /// Input is pulled in batches of at most 64 stereo frames (128 samples) to
    /// keep the ring access count low; the fade/dither behavior of
    /// `pop_exact_or_dither` is preserved unchanged.
    pub fn process_from_ring(
        &mut self,
        ring: &BurstDecouplerRing,
        dither_rng: &mut u32,
        out_stereo: &mut [f32],
    ) {
        // Stack-only batch scratch: at most 64 stereo frames per ring access,
        // no heap. The phase invariant makes the total input-frame count for
        // this call exactly `floor(phase + out_frames * ratio)`; popping only
        // that many (in batches) leaves no surplus in the stack scratch.
        let mut scratch = [0.0f32; 128];
        let mut scratch_pos = 0usize;
        let mut scratch_len = 0usize;
        let out_frames = out_stereo.len() / 2;
        let mut to_pop = self.needed_input_frames(out_frames);

        for out_idx in 0..out_frames {
            while self.phase >= 1.0 {
                self.shift_one_from_ring(
                    ring,
                    dither_rng,
                    &mut scratch,
                    &mut scratch_pos,
                    &mut scratch_len,
                    &mut to_pop,
                );
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
        // Drain any trailing whole input frames implied by the final phase,
        // keeping the history window identical to the per-frame pop version.
        while self.phase >= 1.0 {
            self.shift_one_from_ring(
                ring,
                dither_rng,
                &mut scratch,
                &mut scratch_pos,
                &mut scratch_len,
                &mut to_pop,
            );
            self.phase -= 1.0;
        }
    }
}

/// 2. Wait-Free SPSC Sample FIFO (Decouples 120-frame network quantum from Android SoC getFramesPerBurst())
const FIFO_CAPACITY: usize = 65536; // Power of 2 (32768 stereo frames = 682 ms @ 48 kHz / 341 ms @ 96 kHz)
const FIFO_MASK: usize = FIFO_CAPACITY - 1;

#[repr(align(64))]
struct CachePadded<T>(T);

pub struct BurstDecouplerRing {
    write_idx: CachePadded<AtomicUsize>,
    read_idx: CachePadded<AtomicUsize>,
    // Heap-boxed to avoid a 256 KB stack frame during `new()` on Android threads.
    buf: Box<[UnsafeCell<f32>]>,
}

unsafe impl Sync for BurstDecouplerRing {}

impl BurstDecouplerRing {
    pub fn new() -> Self {
        let buf = (0..FIFO_CAPACITY)
            .map(|_| UnsafeCell::new(0.0))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            write_idx: CachePadded(AtomicUsize::new(0)),
            read_idx: CachePadded<AtomicUsize::new(0)),
            buf,
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

        // Copy in at most two contiguous runs: one up to the physical end of
        // the buffer, then the wrapped remainder at the start. This replaces a
        // per-element RMW loop with two memcpy-class operations while keeping
        // the Release store below as the single publication point.
        let buf_ptr = self.buf.as_ptr() as *mut f32;
        let first_off = w & FIFO_MASK;
        let first_len = to_write.min(FIFO_CAPACITY - first_off);
        unsafe {
            core::ptr::copy_nonoverlapping(samples.as_ptr(), buf_ptr.add(first_off), first_len);
            if first_len < to_write {
                core::ptr::copy_nonoverlapping(
                    samples.as_ptr().add(first_len),
                    buf_ptr,
                    to_write - first_len,
                );
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
            // Mirror `push_samples`: two contiguous runs instead of a
            // per-element read loop, published by the Release store below
            // after the copy has completed.
            let buf_ptr = self.buf.as_ptr() as *const f32;
            let first_off = r & FIFO_MASK;
            let first_len = req_even.min(FIFO_CAPACITY - first_off);
            unsafe {
                core::ptr::copy_nonoverlapping(buf_ptr.add(first_off), out.as_mut_ptr(), first_len);
                if first_len < req_even {
                    core::ptr::copy_nonoverlapping(
                        buf_ptr,
                        out.as_mut_ptr().add(first_len),
                        req_even - first_len,
                    );
                }
            }
            if req_even < out.len() {
                out[req_even] = 0.0;
            }
            self.read_idx.0
                .store(r.wrapping_add(req_even), Ordering::Release);
            true
        } else if req_even > 0 && (avail & !1) > 0 {
            // Partial underrun: copy what is left, cross-fading the final
            // frames to zero and zero-filling the remainder. Without the fade,
            // a callback only one frame short would leave the output near
            // full-scale, and the next (empty) callback starts at -90 dBFS
            // comfort dither: an audible step into the DAC.
            //
            // The faded/zero-filled audio is still delivered so the DAC sees
            // no discontinuity, but we report `false`: the caller did NOT get
            // the exact burst it asked for, so this counts as an underrun for
            // telemetry and any latency bookkeeping. Only a completely full
            // read reports `true`.
            let safe_avail = (avail & !1).min(req_even); // Keep stereo L/R alignment
            let fade_frames = (safe_avail / 2).min(16);
            let fade_samples = fade_frames * 2;
            let fade_start = safe_avail - fade_samples;
            let buf_ptr = self.buf.as_ptr() as *const f32;
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
            }
            self.read_idx.0
                .store(r.wrapping_add(safe_avail), Ordering::Release);
            out[safe_avail..].fill(0.0);
            false
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
        if !gain_db.is_finite() || gain_db <= -60.0 {
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
        // -0.4 dBFS leaves headroom for 4-point Hermite inter-sample peaks
        // when clock drift != 0 or 44.1 kHz Bluetooth resampling is active.
        const CEILING: f32 = 0.955;
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
            // real signal level.
            //
            // p4-C1: the old condition bypassed the knee for peak <= CEILING
            // (0.955) then jumped abruptly into the THRESHOLD (0.85) formula
            // the instant a sample exceeded 0.955 -- a ~10% downward step on
            // every kick/snare (audible crackle). Use a C1-continuous unity
            // knee starting at UNITY_THRESHOLD = 0.92: <= 0.92 stays bit-exact,
            // 0.92 -> 0.955 is monotonic with slope 1.0 at the knee start
            // (zero step), and the ceiling is still honoured.
            // Blend the knee smoothly over 1.0..=1.05 (+0.42 dB) so sweeping
            // the preamp slider through 0 dB produces no gain step.
            const UNITY_THRESHOLD: f32 = 0.92;
            let boost_blend = ((self.current_gain_linear - 1.0) / 0.05).clamp(0.0, 1.0);
            let knee_start = UNITY_THRESHOLD - (UNITY_THRESHOLD - THRESHOLD) * boost_blend;
            let det = self.envelope.max(peak);
            if det > knee_start {
                let over = det - knee_start;
                let headroom = CEILING - knee_start;
                let compressed = knee_start + headroom * (over / (over + headroom));
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
        if !s.is_finite() {
            *s = 0.0;
            continue;
        }
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

    if channels == 0 {
        return;
    }
    let frames = (in_interleaved.len() / channels).min(out_stereo.len() / 2);
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

/// Expand a mono microphone capture into the interleaved stereo ring that the
/// transmitter consumes, optionally resampling the capture rate to the ring's
/// native 48 kHz rate first.
///
/// This is the pure, platform-independent extraction of the AAudio mono input
/// callback: Android delivers a mono `f32` buffer whose device capture rate may
/// differ from 48 kHz, while the downstream decoupler ring is always
/// interleaved stereo at 48 kHz. Keeping the expand + resample + push logic
/// here (instead of inside the JNI callback) lets it be unit-tested on any host.
///
/// * `mono` - mono capture samples, one `f32` per frame.
/// * `resampler` - carries phase/history across successive chunks; configure it
///   with [`ClockDriftResampler::set_sample_rates`] before the first call. It is
///   only touched when `needs_resample` is true.
/// * `out_scratch` - caller-owned destination for `resampler.process`; never
///   heap-allocated here. It also bounds how much output one resampler call may
///   emit. The function re-feeds any input the resampler could not consume in a
///   call, so a small scratch simply costs more loop iterations, not samples.
///
/// No heap allocation occurs: the stereo expansion uses a bounded stack array,
/// and input chunks larger than that array are processed in a loop.
#[inline]
pub fn push_mono_mic_chunk(
    ring: &BurstDecouplerRing,
    mono: &[f32],
    resampler: &mut ClockDriftResampler,
    needs_resample: bool,
    out_scratch: &mut [f32],
) {
    if mono.is_empty() {
        return;
    }

    // Bounded stack scratch for mono -> interleaved stereo expansion. A single
    // AAudio callback burst is at most a few hundred frames, so 256 leaves
    // margin while keeping the frame well inside real-time stack budgets.
    const EXPAND_FRAMES: usize = 256;
    let mut stereo = [0.0f32; EXPAND_FRAMES * 2];

    if !needs_resample {
        // Bypass: exact bit-for-bit `[s, s]` duplication, looping over chunks
        // larger than the expansion buffer. `out_scratch` is deliberately
        // unused on this path.
        let mut offset = 0usize;
        while offset < mono.len() {
            let frames = (mono.len() - offset).min(EXPAND_FRAMES);
            for (i, &sample) in mono[offset..offset + frames].iter().enumerate() {
                stereo[i * 2] = sample;
                stereo[i * 2 + 1] = sample;
            }
            ring.push_samples(&stereo[..frames * 2]);
            offset += frames;
        }
        return;
    }

    if out_scratch.len() < 2 {
        return; // no room for even one stereo output frame
    }

    // Resample path. Feed bounded mono chunks to `process`, then re-feed any
    // frames it could not consume because the output scratch filled up. The
    // exact consumed count is derived from the resampler's phase invariant
    // `phase_after = phase_before + produced * ratio - consumed`, so no input
    // is dropped regardless of how small `out_scratch` is.
    let mut offset = 0usize;
    while offset < mono.len() {
        let frames = (mono.len() - offset).min(EXPAND_FRAMES);
        for (i, &sample) in mono[offset..offset + frames].iter().enumerate() {
            stereo[i * 2] = sample;
            stereo[i * 2 + 1] = sample;
        }

        let phase_before = resampler.phase;
        let produced = resampler.process(&stereo[..frames * 2], out_scratch);
        if produced > 0 {
            ring.push_samples(&out_scratch[..produced * 2]);
        }

        let consumed_f64 =
            phase_before + produced as f64 * resampler.ratio - resampler.phase;
        let consumed = consumed_f64.round().max(0.0) as usize;
        if consumed == 0 {
            if produced == 0 {
                break; // no output capacity and no phase progress possible
            }
            // Output was produced but the phase never reached 1.0, so no input
            // frame was shifted into history yet. Retry the same chunk; phase
            // advances on every call, so the next iterations make progress.
            continue;
        }
        offset += consumed.min(frames);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pull every sample currently queued in the ring as interleaved stereo.
    fn drain_ring(ring: &BurstDecouplerRing) -> Vec<f32> {
        let samples = ring.available_samples();
        let mut out = vec![0.0f32; samples];
        let mut rng = 0x1234_5678u32;
        ring.pop_exact_or_dither(&mut out, &mut rng);
        out
    }

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
        // callback starts from silence instead of a full-scale step, and now
        // reports `false` because the exact burst was not available.
        assert!(!ring.pop_exact_or_dither(&mut aaudio_burst, &mut rng));
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
        assert_eq!(FIFO_CAPACITY, 65_536);
        assert!(FIFO_CAPACITY.is_power_of_two());
        let ring = BurstDecouplerRing::new();
        assert_eq!(ring.capacity_samples(), 65_536);

        let mut resampler = ClockDriftResampler::new();

        // 48 kHz output: base_ratio 1.0, so 64 subframes are 15,360 samples.
        resampler.set_output_sample_rate(48_000);
        assert_eq!(resampler.base_ratio, 1.0);
        assert_eq!(resampler.subframes_to_samples(64), 15_360);
        assert!((resampler.samples_to_subframes(65_536) - 273.066_67).abs() < 1e-3);
        assert!(resampler.subframes_to_samples(64) + 240 <= ring.capacity_samples());

        // 96 kHz output: base_ratio 0.5, so 64 subframes are 30,720 output
        // samples and the ring holds 136.53 subframe equivalents. 137
        // subframes (65,760 samples) would overflow.
        resampler.set_output_sample_rate(96_000);
        assert_eq!(resampler.base_ratio, 0.5);
        assert_eq!(resampler.subframes_to_samples(64), 30_720);
        assert!((resampler.samples_to_subframes(65_536) - 136.533_33).abs() < 1e-3);
        assert!(resampler.subframes_to_samples(137) > ring.capacity_samples());

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
    fn test_clock_controller_bounds_correction_to_400ppm() {
        let mut resampler = ClockDriftResampler::new();
        // Normal drift: err = 1.0 subframe
        for _ in 0..200 {
            resampler.update_clock_loop(3.0, 2.0);
        }
        let normal_corr = resampler.ratio - 1.0;
        assert!(normal_corr > 0.0 && normal_corr <= 0.0004);

        let mut spike_resampler = ClockDriftResampler::new();
        for _ in 0..200 {
            spike_resampler.update_clock_loop(8.0, 2.0);
        }
        let spike_corr = spike_resampler.ratio - 1.0;
        assert!(
            spike_corr > 0.0 && spike_corr <= 0.0004 + 1e-9,
            "Spike correction must stay within +/-400 ppm to prevent pitch warble: {}",
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
        // Only 382 of 384 samples are available: the faded audio is delivered
        // but a partial read now reports an underrun.
        assert!(!ring.pop_exact_or_dither(&mut out, &mut rng));
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
    fn test_true_peak_limiter_nan_gain_stays_finite() {
        // f32::NAN <= -60.0 is false and NAN.clamp(..) returns NAN, so an
        // unguarded conversion poisons both gain fields and every later sample.
        let mut limiter = TruePeakLimiter::new(f32::NAN);
        assert!(limiter.current_gain_linear.is_finite());
        assert!(limiter.target_gain_linear.is_finite());
        assert_eq!(limiter.current_gain_linear, 0.0);

        let mut signal = [0.5f32; 240];
        limiter.process_in_place(&mut signal);
        assert!(signal.iter().all(|s| s.is_finite()));

        limiter.set_gain_db(f32::INFINITY);
        assert!(limiter.target_gain_linear.is_finite());
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
    fn test_downmix_to_stereo_zero_channels_does_not_panic() {
        // channels == 0 previously made `frames == in.len()` and produced an
        // empty `src` slice, panicking on `src[0]` under panic=abort.
        let mut out = [0.0f32; 4];
        downmix_to_stereo(&[1.0], 0, &mut out);
        assert!(out.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn test_downmix_to_stereo_clamps_to_output_length() {
        // A short destination must bound the work instead of indexing past it.
        let input = [0.5f32; 12]; // 6 stereo frames
        let mut out = [0.0f32; 4]; // room for 2 frames only
        downmix_to_stereo(&input, 2, &mut out);
        assert_eq!(out, [0.5, 0.5, 0.5, 0.5]);
    }

    #[test]
    fn test_soft_limit_post_resample_sanitizes_non_finite() {
        let mut s = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.5];
        soft_limit_post_resample(&mut s);
        assert!(s.iter().all(|v| v.is_finite()));
        assert_eq!(s[3], 0.5);
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

    #[test]
    fn test_process_from_ring_batch_pop_discards_no_samples() {
        // The 128-sample stack scratch must be refilled with only the frames
        // this call will actually consume. A naive fixed-size batch would read
        // 64 frames even when the phase invariant needs fewer, silently
        // stranding the surplus when the scratch goes out of scope.
        for &rate in &[44_100u32, 48_000, 96_000] {
            let mut resampler = ClockDriftResampler::new();
            resampler.set_output_sample_rate(rate);
            let ring = BurstDecouplerRing::new();
            let chunk = [0.25f32; 3000]; // 1500 stereo frames
            assert_eq!(ring.push_samples(&chunk), 3000);
            let mut expected = ring.available_samples();
            let mut rng = 1234u32;

            for &frames in &[100usize, 37, 191] {
                let needed = resampler.needed_input_frames(frames);
                let mut out = vec![0.0f32; frames * 2];
                resampler.process_from_ring(&ring, &mut rng, &mut out);
                expected -= needed * 2;
                assert_eq!(
                    ring.available_samples(),
                    expected,
                    "rate {rate}: batched pop consumed a different count than the phase invariant"
                );
            }
        }
    }

    // ---- p9-3c: pure mono microphone expand + optional resample helper ----

    #[test]
    fn test_push_mono_mic_chunk_resamples_16k_to_48k() {
        let ring = BurstDecouplerRing::new();
        let mut resampler = ClockDriftResampler::new();
        resampler.set_sample_rates(16_000, 48_000);
        assert!((resampler.ratio - 1.0 / 3.0).abs() < 1e-12);

        let input_frames = 4096usize;
        let mono: Vec<f32> = (0..input_frames)
            .map(|i| {
                let t = i as f32 / 16_000.0;
                (std::f32::consts::TAU * 1_000.0 * t).sin() * 0.5
            })
            .collect();

        // Scratch comfortably larger than one 256-frame expansion block.
        let mut scratch = vec![0.0f32; 2048];
        push_mono_mic_chunk(&ring, &mono, &mut resampler, true, &mut scratch);

        let out = drain_ring(&ring);
        let out_frames = out.len() / 2;
        let expected = input_frames * 3; // 48 kHz / 16 kHz = 3x
        assert!(
            (out_frames as i64 - expected as i64).abs() <= 16,
            "16k->48k: ring received {} frames, expected ~{}",
            out_frames,
            expected
        );
        assert!(
            out.iter().all(|s| s.is_finite()),
            "16k->48k resampling produced a non-finite sample"
        );
        // Mono expands to identical L/R; the same filter/interpolation is
        // applied to both channels, so they must stay equal after resampling.
        for pair in out.chunks_exact(2) {
            assert!(
                (pair[0] - pair[1]).abs() < 1e-6,
                "L/R drifted apart after mono resampling: {} vs {}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn test_push_mono_mic_chunk_resamples_44k1_to_48k() {
        let ring = BurstDecouplerRing::new();
        let mut resampler = ClockDriftResampler::new();
        resampler.set_sample_rates(44_100, 48_000);
        let expected_ratio = 48_000.0 / 44_100.0;

        let input_frames = 4096usize;
        let mono: Vec<f32> = (0..input_frames)
            .map(|i| (i as f32 * 0.05).sin() * 0.4)
            .collect();

        let mut scratch = vec![0.0f32; 2048];
        push_mono_mic_chunk(&ring, &mono, &mut resampler, true, &mut scratch);

        let out = drain_ring(&ring);
        let out_frames = out.len() / 2;
        let expected = (input_frames as f64 * expected_ratio).round() as usize;
        assert!(
            (out_frames as i64 - expected as i64).abs() <= 16,
            "44.1k->48k: ring received {} frames, expected ~{}",
            out_frames,
            expected
        );
        assert!(
            out.iter().all(|s| s.is_finite()),
            "44.1k->48k resampling produced a non-finite sample"
        );
    }

    #[test]
    fn test_push_mono_mic_chunk_bypass_is_bit_exact_stereo_duplicate() {
        let ring = BurstDecouplerRing::new();
        let mut resampler = ClockDriftResampler::new(); // untouched on bypass
        // Tiny scratch (< mono length) proves the bypass path never depends on
        // resampler scratch space, and a >256-frame mono input proves looping.
        let mut scratch = [0.0f32; 16];
        let mono: Vec<f32> = (0..1000).map(|i| (i as f32 * 0.017) - 8.0).collect();

        push_mono_mic_chunk(&ring, &mono, &mut resampler, false, &mut scratch);

        let out = drain_ring(&ring);
        assert_eq!(out.len(), mono.len() * 2);
        for (i, &sample) in mono.iter().enumerate() {
            assert_eq!(out[i * 2], sample, "L sample {} modified on bypass", i);
            assert_eq!(out[i * 2 + 1], sample, "R sample {} modified on bypass", i);
        }
        // Bypass must not advance resampler state.
        assert_eq!(resampler.phase, 0.0);
    }

    #[test]
    fn test_push_mono_mic_chunk_resample_loops_when_mono_exceeds_scratch() {
        let ring = BurstDecouplerRing::new();
        let mut resampler = ClockDriftResampler::new();
        resampler.set_sample_rates(16_000, 48_000);

        let input_frames = 4096usize;
        let mono = vec![0.25f32; input_frames];
        // Only 64 output frames of scratch: the 4096-frame mono input must be
        // split across many process() calls without losing any samples.
        let mut scratch = vec![0.0f32; 128];

        push_mono_mic_chunk(&ring, &mono, &mut resampler, true, &mut scratch);

        let out = drain_ring(&ring);
        let out_frames = out.len() / 2;
        let expected = input_frames * 3;
        assert!(
            (out_frames as i64 - expected as i64).abs() <= 16,
            "looped 16k->48k: ring received {} frames, expected ~{}",
            out_frames,
            expected
        );
        assert!(out.iter().all(|s| s.is_finite()));
        // A constant mono level must survive the loop as a constant stereo DC.
        let tail = &out[out.len().saturating_sub(200)..];
        assert!(tail.iter().all(|s| (s - 0.25).abs() < 1e-4));
    }
}
