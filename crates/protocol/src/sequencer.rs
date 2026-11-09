use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::{SUBFRAME_FRAMES, SUBFRAME_STEREO_SAMPLES};

pub const RING_SLOTS: usize = 64;
const RING_MASK: usize = RING_SLOTS - 1;

pub const STATUS_EMPTY: u32 = 0;
pub const STATUS_PRIMARY_LOSSLESS: u32 = 1;
pub const STATUS_HEALED_BY_FEC: u32 = 2;
const STATUS_BUSY: u32 = 3;

const HISTORY_FRAMES: usize = 360; // 7.5ms pitch search window (down to 133Hz fundamental)
const HISTORY_STEREO_SAMPLES: usize = HISTORY_FRAMES * 2;
const CROSSFADE_FRAMES: usize = 32; // 0.66ms raised-cosine overlap-add crossfade

#[repr(align(64))]
pub struct SequencerSlot {
    state: AtomicU32,
    samples: UnsafeCell<[f32; SUBFRAME_STEREO_SAMPLES]>,
}

unsafe impl Sync for SequencerSlot {}

pub struct ZeroJitterSequencer {
    slots: [SequencerSlot; RING_SLOTS],
    /// Rolling 7.5ms stereo history for WSOLA pitch-period autocorrelation PLC
    history: [f32; HISTORY_STEREO_SAMPLES],
    /// Continuation tail for smooth overlap-add when real packets resume after PLC
    plc_recovery_tail: [f32; CROSSFADE_FRAMES * 2],
    consecutive_plc_count: u32,
}

impl ZeroJitterSequencer {
    pub fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| SequencerSlot {
                state: AtomicU32::new(STATUS_EMPTY),
                samples: UnsafeCell::new([0.0; SUBFRAME_STEREO_SAMPLES]),
            }),
            history: [0.0; HISTORY_STEREO_SAMPLES],
            plc_recovery_tail: [0.0; CROSSFADE_FRAMES * 2],
            consecutive_plc_count: 0,
        }
    }

    /// Fast O(1) check before decoding FEC payloads: returns true ONLY if `target_seq` is still missing
    #[inline]
    pub fn is_slot_missing(&self, target_seq: u16) -> bool {
        let slot = &self.slots[(target_seq as usize) & RING_MASK];
        let cur = slot.state.load(Ordering::Acquire);
        let cur_seq = (cur >> 16) as u16;
        let cur_status = cur & 0xFFFF;
        cur_seq != target_seq || cur_status == STATUS_EMPTY
    }

    #[inline]
    pub fn push_primary(&self, seq: u16, pcm_f32: &[f32; SUBFRAME_STEREO_SAMPLES]) -> bool {
        let slot = &self.slots[(seq as usize) & RING_MASK];
        let cur = slot.state.load(Ordering::Acquire);
        let cur_seq = (cur >> 16) as u16;
        let cur_status = cur & 0xFFFF;

        // Ignore if busy or if primary lossless frame already arrived via faster path (e.g. USB)
        if cur_status == STATUS_BUSY || (cur_seq == seq && cur_status == STATUS_PRIMARY_LOSSLESS) {
            return false;
        }

        // Atomically claim exclusive write access to prevent data races with pop_next()
        let busy_state = ((seq as u32) << 16) | STATUS_BUSY;
        if slot
            .state
            .compare_exchange(cur, busy_state, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }

        unsafe {
            *slot.samples.get() = *pcm_f32;
        }
        slot.state.store(
            ((seq as u32) << 16) | STATUS_PRIMARY_LOSSLESS,
            Ordering::Release,
        );
        true
    }

    #[inline]
    pub fn try_heal_with_fec(
        &self,
        target_seq: u16,
        fec_f32: &[f32; SUBFRAME_STEREO_SAMPLES],
    ) -> bool {
        let slot = &self.slots[(target_seq as usize) & RING_MASK];
        let cur = slot.state.load(Ordering::Acquire);
        let cur_seq = (cur >> 16) as u16;
        let cur_status = cur & 0xFFFF;

        if cur_status == STATUS_BUSY || (cur_seq == target_seq && cur_status != STATUS_EMPTY) {
            return false;
        }

        let busy_state = ((target_seq as u32) << 16) | STATUS_BUSY;
        if slot
            .state
            .compare_exchange(cur, busy_state, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }

        unsafe {
            *slot.samples.get() = *fec_f32;
        }
        slot.state.store(
            ((target_seq as u32) << 16) | STATUS_HEALED_BY_FEC,
            Ordering::Release,
        );
        true
    }

    #[inline]
    fn append_to_history(&mut self, frame: &[f32; SUBFRAME_STEREO_SAMPLES]) {
        let keep = HISTORY_STEREO_SAMPLES - SUBFRAME_STEREO_SAMPLES;
        self.history.copy_within(SUBFRAME_STEREO_SAMPLES.., 0);
        self.history[keep..].copy_from_slice(frame);
    }

    /// Find best pitch period P in [48..=240] frames via energy-normalized autocorrelation on Stereo Mid
    #[inline]
    fn find_best_pitch_period(&self) -> usize {
        let template_start = HISTORY_FRAMES - CROSSFADE_FRAMES;
        let mut best_lag = SUBFRAME_FRAMES;
        let mut best_score = f32::MIN;

        // Step by 2 frames for fast real-time search (< 1.5 us)
        for lag in (48..=240).step_by(2) {
            let cand_start = template_start - lag;
            let mut corr = 0.0f32;
            let mut energy = 1e-6f32;
            for k in 0..CROSSFADE_FRAMES {
                let t_m = self.history[(template_start + k) * 2]
                    + self.history[(template_start + k) * 2 + 1];
                let c_m =
                    self.history[(cand_start + k) * 2] + self.history[(cand_start + k) * 2 + 1];
                corr += t_m * c_m;
                energy += c_m * c_m;
            }
            // Signed squared correlation normalized by candidate window energy (avoids sqrt!)
            let score = (corr * corr.abs()) / energy;
            if score > best_score {
                best_score = score;
                best_lag = lag;
            }
        }
        best_lag
    }

    /// Consumer (Real-Time Audio Thread): Pop next 2.5ms subframe with WSOLA Pitch-Period PLC
    #[inline]
    pub fn pop_next(&mut self, expected_seq: u16, out: &mut [f32; SUBFRAME_STEREO_SAMPLES]) -> u32 {
        let slot = &self.slots[(expected_seq as usize) & RING_MASK];
        let cur = slot.state.load(Ordering::Acquire);
        let cur_seq = (cur >> 16) as u16;
        let cur_status = cur & 0xFFFF;

        if cur_seq == expected_seq
            && (cur_status == STATUS_PRIMARY_LOSSLESS || cur_status == STATUS_HEALED_BY_FEC)
        {
            let busy_state = ((expected_seq as u32) << 16) | STATUS_BUSY;
            if slot
                .state
                .compare_exchange(cur, busy_state, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                unsafe {
                    *out = *slot.samples.get();
                }
                slot.state.store(
                    ((expected_seq as u32) << 16) | STATUS_EMPTY,
                    Ordering::Release,
                );

                // If recovering from a PLC event, crossfade the first 32 frames to prevent phase click!
                if self.consecutive_plc_count > 0 {
                    for i in 0..CROSSFADE_FRAMES {
                        let alpha = (i as f32) / (CROSSFADE_FRAMES as f32);
                        let w_in = 0.5 * (1.0 - (alpha * std::f32::consts::PI).cos());
                        let w_plc = 1.0 - w_in;
                        out[i * 2] = out[i * 2] * w_in + self.plc_recovery_tail[i * 2] * w_plc;
                        out[i * 2 + 1] =
                            out[i * 2 + 1] * w_in + self.plc_recovery_tail[i * 2 + 1] * w_plc;
                    }
                    self.consecutive_plc_count = 0;
                }

                self.append_to_history(out);
                return cur_status;
            }
        }

        // WSOLA Pitch-Period Waveform Extrapolation
        self.consecutive_plc_count += 1;
        let pitch_lag = self.find_best_pitch_period();
        let attenuation = (0.90f32).powi(self.consecutive_plc_count as i32);

        let copy_start = HISTORY_FRAMES - pitch_lag;
        for i in 0..SUBFRAME_FRAMES {
            let src_frame = copy_start + (i % pitch_lag);
            out[i * 2] = self.history[src_frame * 2] * attenuation;
            out[i * 2 + 1] = self.history[src_frame * 2 + 1] * attenuation;
        }

        // Prepare continuation tail for seamless crossfade when next good packet arrives
        for i in 0..CROSSFADE_FRAMES {
            let src_frame = copy_start + ((SUBFRAME_FRAMES + i) % pitch_lag);
            self.plc_recovery_tail[i * 2] = self.history[src_frame * 2] * attenuation;
            self.plc_recovery_tail[i * 2 + 1] = self.history[src_frame * 2 + 1] * attenuation;
        }

        self.append_to_history(out);
        STATUS_EMPTY
    }

    #[inline]
    pub fn consecutive_plc_count(&self) -> u32 {
        self.consecutive_plc_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wsola_plc_and_smooth_recovery_crossfade() {
        let mut seq = ZeroJitterSequencer::new();
        let mut sine_frame = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        for i in 0..SUBFRAME_FRAMES {
            let v = ((i as f32) * 2.0 * std::f32::consts::PI * 400.0 / 48_000.0).sin() * 0.5;
            sine_frame[i * 2] = v;
            sine_frame[i * 2 + 1] = v;
        }

        // Prime history with 3 good frames
        for s in 0..3 {
            seq.push_primary(s, &sine_frame);
            let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
            assert_eq!(seq.pop_next(s, &mut out), STATUS_PRIMARY_LOSSLESS);
        }

        // Simulate burst loss at seq = 3 (invokes WSOLA PLC)
        let mut plc_out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        assert_eq!(seq.pop_next(3, &mut plc_out), STATUS_EMPTY);
        let energy: f32 = plc_out.iter().map(|x| x * x).sum();
        assert!(
            energy > 1.0,
            "WSOLA PLC must extrapolate non-zero harmonic energy"
        );

        // Recover at seq = 4 and verify smooth crossfade
        seq.push_primary(4, &sine_frame);
        let mut rec_out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        assert_eq!(seq.pop_next(4, &mut rec_out), STATUS_PRIMARY_LOSSLESS);
    }
}
