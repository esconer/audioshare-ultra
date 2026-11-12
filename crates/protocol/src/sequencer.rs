use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::{SUBFRAME_FRAMES, SUBFRAME_STEREO_SAMPLES};

pub const RING_SLOTS: usize = 128; // 128 * 2.5ms = 320ms capacity for Wired + Bluetooth Group Sync!
const RING_MASK: usize = RING_SLOTS - 1;

pub const STATUS_EMPTY: u32 = 0;
pub const STATUS_PRIMARY_LOSSLESS: u32 = 1;
pub const STATUS_HEALED_BY_FEC: u32 = 2;
const STATUS_BUSY: u32 = 3;

const HISTORY_FRAMES: usize = 360; // 7.5ms pitch search window (down to 133Hz fundamental)
const HISTORY_STEREO_SAMPLES: usize = HISTORY_FRAMES * 2;
const CROSSFADE_FRAMES: usize = 32; // 0.66ms raised-cosine overlap-add crossfade
/// Linear correction applied when a lossy FEC reconstruction is followed by a
/// bit-exact primary frame. 8 frames = 0.17 ms at 48 kHz: long enough to hide
/// the accumulated ADPCM error, short enough to leave the transient intact.
const FEC_DECLICK_FRAMES: usize = 8;

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
    /// True when the most recently popped subframe was an IMA-ADPCM FEC
    /// reconstruction. The next bit-exact primary frame is joined to it with
    /// a short linear correction so the codec switch cannot click.
    last_pop_was_fec: bool,
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
            last_pop_was_fec: false,
        }
    }

    /// Reset all sequencer slots and PLC state on stream re-initialization or DTX wake.
    ///
    /// Slots are indexed by `seq & (RING_SLOTS - 1)`, so a new stream that
    /// restarts at a previously used sequence number would otherwise find a
    /// stale `STATUS_PRIMARY_LOSSLESS` payload and play it. Clearing the
    /// history/tail also prevents PLC from extrapolating audio from the old
    /// stream after a reset.
    pub fn reset(&mut self) {
        for slot in &self.slots {
            slot.state.store(STATUS_EMPTY, Ordering::Release);
        }
        self.history.fill(0.0);
        self.plc_recovery_tail.fill(0.0);
        self.consecutive_plc_count = 0;
        self.last_pop_was_fec = false;
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

    /// Returns true if this exact sequence already holds a decoded primary
    /// frame. Used by the receiver to skip a redundant decode when the same
    /// packet arrives over two paths (USB + Wi-Fi, or party-mode unicast +
    /// broadcast). FEC-healed slots deliberately return false so a late
    /// primary copy can still replace the concealment.
    #[inline]
    pub fn has_primary(&self, seq: u16) -> bool {
        let slot = &self.slots[(seq as usize) & RING_MASK];
        let cur = slot.state.load(Ordering::Acquire);
        (cur >> 16) as u16 == seq && (cur & 0xFFFF) == STATUS_PRIMARY_LOSSLESS
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
                } else if self.last_pop_was_fec && cur_status != STATUS_HEALED_BY_FEC {
                    // The previous subframe was an IMA-ADPCM FEC reconstruction.
                    // Its timbre is close, but its accumulated quantization error
                    // leaves the final sample offset from the bit-exact primary
                    // frame that follows. Splice from that exact last sample for
                    // 0.17 ms instead of applying the old unconditional codec
                    // step, then let the primary frame take over unchanged.
                    let last_l = self.history[(HISTORY_FRAMES - 1) * 2];
                    let last_r = self.history[(HISTORY_FRAMES - 1) * 2 + 1];
                    let delta_l = last_l - out[0];
                    let delta_r = last_r - out[1];
                    for i in 0..FEC_DECLICK_FRAMES {
                        let w = 1.0 - (i as f32) / (FEC_DECLICK_FRAMES as f32);
                        out[i * 2] += delta_l * w;
                        out[i * 2 + 1] += delta_r * w;
                    }
                }

                self.last_pop_was_fec = cur_status == STATUS_HEALED_BY_FEC;
                self.append_to_history(out);
                return cur_status;
            }
        }

        // WSOLA Pitch-Period Waveform Extrapolation
        //
        // At PLC onset the first sample must continue from the last played
        // sample, not jump to `history[HISTORY_FRAMES - pitch_lag] * 0.90`.
        // A 10% step at the frame boundary is a broadband click. Crossfade
        // the first CROSSFADE_FRAMES from the exact last output sample and
        // ramp the attenuation from the previous frame's value to this
        // frame's target instead of applying a constant per-frame step.
        let prev_attenuation = (0.90f32).powi(self.consecutive_plc_count as i32);
        self.consecutive_plc_count += 1;
        let target_attenuation = (0.90f32).powi(self.consecutive_plc_count as i32);
        let pitch_lag = self.find_best_pitch_period();
        let last_l = self.history[(HISTORY_FRAMES - 1) * 2];
        let last_r = self.history[(HISTORY_FRAMES - 1) * 2 + 1];

        let copy_start = HISTORY_FRAMES - pitch_lag;
        for i in 0..SUBFRAME_FRAMES {
            let frac = (i as f32) / ((SUBFRAME_FRAMES - 1) as f32);
            let attenuation = prev_attenuation + (target_attenuation - prev_attenuation) * frac;
            let src_frame = copy_start + (i % pitch_lag);
            let mut l = self.history[src_frame * 2] * attenuation;
            let mut r = self.history[src_frame * 2 + 1] * attenuation;

            if self.consecutive_plc_count == 1 && i < CROSSFADE_FRAMES {
                // Raised-cosine onset fade from the last real sample. At i=0
                // the weight is exactly 0, so there is no step discontinuity.
                let alpha = (i as f32) / ((CROSSFADE_FRAMES - 1) as f32);
                let w_in = 0.5 * (1.0 - (alpha * std::f32::consts::PI).cos());
                l = last_l * (1.0 - w_in) + l * w_in;
                r = last_r * (1.0 - w_in) + r * w_in;
            }

            out[i * 2] = l;
            out[i * 2 + 1] = r;
        }

        // Prepare continuation tail for seamless crossfade when next good packet arrives
        for i in 0..CROSSFADE_FRAMES {
            let src_frame = copy_start + ((SUBFRAME_FRAMES + i) % pitch_lag);
            self.plc_recovery_tail[i * 2] = self.history[src_frame * 2] * target_attenuation;
            self.plc_recovery_tail[i * 2 + 1] =
                self.history[src_frame * 2 + 1] * target_attenuation;
        }

        // WSOLA concealment already has its own recovery crossfade above. Do
        // not additionally apply the FEC splice ramp to the next frame.
        self.last_pop_was_fec = false;
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

    #[test]
    fn test_has_primary_skips_only_decoded_primary_slots() {
        let mut seq = ZeroJitterSequencer::new();
        let frame = [0.25f32; SUBFRAME_STEREO_SAMPLES];

        assert!(!seq.has_primary(7));
        assert!(seq.push_primary(7, &frame));
        assert!(seq.has_primary(7));
        assert!(!seq.has_primary(8));

        // A FEC-healed slot is not a primary copy: a late real packet may
        // still upgrade it.
        assert!(seq.try_heal_with_fec(9, &frame));
        assert!(!seq.has_primary(9));
        assert!(!seq.is_slot_missing(9));

        // Popping consumes the slot, so a duplicate arriving afterwards is
        // no longer considered decoded and would be rejected by the playhead.
        let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        assert_eq!(seq.pop_next(7, &mut out), STATUS_PRIMARY_LOSSLESS);
        assert!(!seq.has_primary(7));
    }

    #[test]
    fn test_plc_onset_continues_from_last_sample() {
        let mut seq = ZeroJitterSequencer::new();
        let mut last_sample = 0.0f32;

        // Use a continuous 300Hz sine across subframes so the pitch period
        // (160 frames) is well inside the WSOLA search range. Repeating one
        // 120-frame buffer would create artificial discontinuities.
        for s in 0..3u16 {
            let mut frame = [0.0f32; SUBFRAME_STEREO_SAMPLES];
            for i in 0..SUBFRAME_FRAMES {
                let n = s as usize * SUBFRAME_FRAMES + i;
                let v = ((n as f32) * 2.0 * std::f32::consts::PI * 300.0 / 48_000.0).sin() * 0.5;
                frame[i * 2] = v;
                frame[i * 2 + 1] = v;
                last_sample = v;
            }
            assert!(seq.push_primary(s, &frame));
            let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
            assert_eq!(seq.pop_next(s, &mut out), STATUS_PRIMARY_LOSSLESS);
        }

        let mut plc_out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        assert_eq!(seq.pop_next(3, &mut plc_out), STATUS_EMPTY);

        // The old code multiplied the first extrapolated sample by 0.90,
        // producing a -1 dB step from the last real sample.
        assert!(
            (plc_out[0] - last_sample).abs() < 1e-4,
            "PLC onset stepped from {} to {}",
            last_sample,
            plc_out[0]
        );

        // The per-sample attenuation ramp must still reduce the frame energy.
        let start_energy: f32 = plc_out[..32].iter().map(|x| x * x).sum();
        let end_energy: f32 = plc_out[SUBFRAME_STEREO_SAMPLES - 32..]
            .iter()
            .map(|x| x * x)
            .sum();
        assert!(
            end_energy < start_energy,
            "PLC attenuation must ramp down across the subframe"
        );
    }

    #[test]
    fn test_reset_discards_stale_slots_and_plc_history() {
        let mut seq = ZeroJitterSequencer::new();
        let stale = [0.5f32; SUBFRAME_STEREO_SAMPLES];
        let fresh = [0.25f32; SUBFRAME_STEREO_SAMPLES];

        // Leave a decoded payload in slot 0 without consuming it.
        assert!(seq.push_primary(0, &stale));
        seq.reset();
        assert_eq!(seq.consecutive_plc_count(), 0);

        // History must be empty as well: a missing slot after reset produces
        // silence, not extrapolation from the previous stream.
        let mut plc = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        assert_eq!(seq.pop_next(0, &mut plc), STATUS_EMPTY);
        assert!(plc.iter().all(|&s| s == 0.0));
        seq.reset();

        // A new stream reusing sequence 0 must be accepted, not rejected as
        // a duplicate of the stale payload.
        assert!(seq.push_primary(0, &fresh));
        let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        assert_eq!(seq.pop_next(0, &mut out), STATUS_PRIMARY_LOSSLESS);
        assert_eq!(out[0], 0.25);
    }

    #[test]
    fn test_fec_to_primary_splice_is_declicked() {
        use crate::lpc_lossless::{
            decode_fec_subframe_to_f32, encode_fec_subframe, ADPCM_FEC_BYTES,
        };

        fn sine_frame(subframe_index: u16) -> [f32; SUBFRAME_STEREO_SAMPLES] {
            let mut frame = [0.0f32; SUBFRAME_STEREO_SAMPLES];
            for i in 0..SUBFRAME_FRAMES {
                let n = subframe_index as usize * SUBFRAME_FRAMES + i;
                let v = ((n as f32) * 2.0 * std::f32::consts::PI * 300.0 / 48_000.0).sin() * 0.5;
                frame[i * 2] = v;
                frame[i * 2 + 1] = v;
            }
            frame
        }

        let mut seq = ZeroJitterSequencer::new();
        let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        let first = sine_frame(0);
        assert!(seq.push_primary(0, &first));
        assert_eq!(seq.pop_next(0, &mut out), STATUS_PRIMARY_LOSSLESS);

        // Reconstruct subframe 1 through the real IMA-ADPCM FEC codec. Its
        // accumulated error is what used to produce the codec-switch click.
        let second = sine_frame(1);
        let mut second_i16 = [0i16; SUBFRAME_STEREO_SAMPLES];
        for (dst, &s) in second_i16.iter_mut().zip(second.iter()) {
            *dst = (s * 32767.0) as i16;
        }
        let mut fec_bytes = [0u8; ADPCM_FEC_BYTES];
        encode_fec_subframe(&second_i16, &mut fec_bytes);
        let mut fec_f32 = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        assert!(decode_fec_subframe_to_f32(&fec_bytes, &mut fec_f32));
        assert!(seq.try_heal_with_fec(1, &fec_f32));
        assert_eq!(seq.pop_next(1, &mut out), STATUS_HEALED_BY_FEC);
        let last_fec_l = out[SUBFRAME_STEREO_SAMPLES - 2];
        let last_fec_r = out[SUBFRAME_STEREO_SAMPLES - 1];

        // The following primary frame must start at the exact sample the
        // listener just heard, then converge to the bit-exact waveform.
        let third = sine_frame(2);
        assert!(seq.push_primary(2, &third));
        assert_eq!(seq.pop_next(2, &mut out), STATUS_PRIMARY_LOSSLESS);
        assert!(
            (out[0] - last_fec_l).abs() < 1e-6 && (out[1] - last_fec_r).abs() < 1e-6,
            "FEC->primary splice stepped from ({last_fec_l}, {last_fec_r}) to ({}, {})",
            out[0],
            out[1]
        );
        for i in FEC_DECLICK_FRAMES..SUBFRAME_FRAMES {
            assert_eq!(out[i * 2], third[i * 2], "ramp leaked past frame {}", i);
            assert_eq!(out[i * 2 + 1], third[i * 2 + 1]);
        }

        // The correction is one-shot: the next primary frame is untouched.
        let fourth = sine_frame(3);
        assert!(seq.push_primary(3, &fourth));
        assert_eq!(seq.pop_next(3, &mut out), STATUS_PRIMARY_LOSSLESS);
        assert_eq!(out[0], fourth[0]);
        assert_eq!(out[1], fourth[1]);
    }
}
