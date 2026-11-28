use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::{SUBFRAME_FRAMES, SUBFRAME_STEREO_SAMPLES};

pub const RING_SLOTS: usize = 256; // 256 * 2.5ms = 640ms capacity (absorbs stock-OS Wi-Fi scan bursts)
const RING_MASK: usize = RING_SLOTS - 1;

pub const STATUS_EMPTY: u32 = 0;
pub const STATUS_PRIMARY_LOSSLESS: u32 = 1;
pub const STATUS_HEALED_BY_FEC: u32 = 2;
const STATUS_BUSY: u32 = 3;

const HISTORY_FRAMES: usize = 720; // 15.0ms pitch search window (down to 80Hz fundamental)
const HISTORY_STEREO_SAMPLES: usize = HISTORY_FRAMES * 2;
const CROSSFADE_FRAMES: usize = 32; // 0.66ms raised-cosine overlap-add crossfade
/// Tick-1 WSOLA onset splice length (0.17 ms): continues seamlessly from the
/// last played sample without smearing a 32-frame static-DC flatline.
const PLC_ONSET_SPLICE_FRAMES: usize = 8;
/// Linear correction applied when a lossy FEC reconstruction is followed by a
/// bit-exact primary frame. 8 frames = 0.17 ms at 48 kHz: long enough to hide
/// the accumulated ADPCM error, short enough to leave the transient intact.
const FEC_DECLICK_FRAMES: usize = 8;
/// Bounded spin-wait iterations when `pop_next` collides with an in-flight
/// `push_primary` (`STATUS_BUSY`) write (~200 ns).
const POP_BUSY_SPIN_LIMIT: usize = 64;

#[repr(align(64))]
pub struct SequencerSlot {
    state: AtomicU32,
    samples: UnsafeCell<[f32; SUBFRAME_STEREO_SAMPLES]>,
}

unsafe impl Sync for SequencerSlot {}

pub struct ZeroJitterSequencer {
    // Heap-allocated once: an inline [SequencerSlot; 128] is ~128 KB (each
    // align(64) slot pads to 1024 bytes) and was returned by value on the
    // stack, risking overflow on Android worker threads.
    slots: Box<[SequencerSlot]>,
    /// Rolling 15.0ms unattenuated stereo history for WSOLA pitch-period autocorrelation PLC
    history: [f32; HISTORY_STEREO_SAMPLES],
    /// Exact last stereo sample rendered to `out`, used for boundary declicking
    /// even while `history` holds unattenuated WSOLA waveforms.
    last_played: [f32; 2],
    /// Continuation tail for smooth overlap-add when real packets resume after PLC
    plc_recovery_tail: [f32; CROSSFADE_FRAMES * 2],
    consecutive_plc_count: u32,
    consecutive_external_plc_count: u32,
    /// True when the most recently popped subframe was an IMA-ADPCM FEC
    /// reconstruction. The next bit-exact primary frame is joined to it with
    /// a short linear correction so the codec switch cannot click.
    last_pop_was_fec: bool,
    /// Arm a short raised-cosine fade-in on the next rendered subframe
    /// (stream start / DTX wake), preventing a full-scale step from dither.
    pending_fade_in: bool,
    /// Arm a short splice from the last played sample on the next subframe
    /// (post-stall fast-forward / codec switch), preventing a discontinuity.
    pending_declick: bool,
}

impl ZeroJitterSequencer {
    pub fn new() -> Self {
        let slots: Vec<SequencerSlot> = (0..RING_SLOTS)
            .map(|_| SequencerSlot {
                state: AtomicU32::new(STATUS_EMPTY),
                samples: UnsafeCell::new([0.0; SUBFRAME_STEREO_SAMPLES]),
            })
            .collect();
        Self {
            slots: slots.into_boxed_slice(),
            history: [0.0; HISTORY_STEREO_SAMPLES],
            last_played: [0.0; 2],
            plc_recovery_tail: [0.0; CROSSFADE_FRAMES * 2],
            consecutive_plc_count: 0,
            consecutive_external_plc_count: 0,
            last_pop_was_fec: false,
            pending_fade_in: true,
            pending_declick: false,
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
        for slot in self.slots.iter() {
            slot.state.store(STATUS_EMPTY, Ordering::Release);
        }
        self.history.fill(0.0);
        self.last_played = [0.0; 2];
        self.plc_recovery_tail.fill(0.0);
        self.consecutive_plc_count = 0;
        self.consecutive_external_plc_count = 0;
        self.last_pop_was_fec = false;
        self.pending_fade_in = true;
        self.pending_declick = false;
    }

    /// Arm a 0.66 ms raised-cosine fade-in on the next rendered subframe.
    #[inline]
    pub fn arm_fade_in(&mut self) {
        self.pending_fade_in = true;
    }

    /// Arm a 1.0 ms raised-cosine splice from the last played sample on the
    /// next subframe (post-stall fast-forward / codec switch).
    #[inline]
    pub fn arm_declick(&mut self) {
        self.pending_declick = true;
    }

    #[inline]
    fn apply_boundary_ramps(&mut self, out: &mut [f32; SUBFRAME_STEREO_SAMPLES]) {
        if self.pending_fade_in {
            for i in 0..CROSSFADE_FRAMES {
                let alpha = (i as f32) / (CROSSFADE_FRAMES as f32);
                let w = 0.5 * (1.0 - (alpha * std::f32::consts::PI).cos());
                out[i * 2] *= w;
                out[i * 2 + 1] *= w;
            }
            self.pending_fade_in = false;
            self.pending_declick = false;
        } else if self.pending_declick {
            let [last_l, last_r] = self.last_played;
            // 48 frames = 1.0 ms C1-continuous convex raised-cosine crossfade:
            // hides both DC steps and lookahead/phase jumps across codec
            // transitions (e.g. Lossless 0 ms <-> Opus 2.5 ms) or post-stall
            // skips without ever overshooting ±1.0 and slamming the limiter.
            const SPLICE_FRAMES: usize = 48;
            for i in 0..SPLICE_FRAMES {
                let alpha = (i as f32) / (SPLICE_FRAMES as f32);
                let w = 0.5 * (1.0 + (alpha * std::f32::consts::PI).cos());
                out[i * 2] = last_l * w + out[i * 2] * (1.0 - w);
                out[i * 2 + 1] = last_r * w + out[i * 2 + 1] * (1.0 - w);
            }
            self.pending_declick = false;
        }
    }

    /// Smoothly integrate a frame decoded outside `ZeroJitterSequencer`
    /// (decode-at-pop Opus / CELT PLC), keeping history and crossfades coherent.
    #[inline]
    pub fn finalize_external_frame(&mut self, out: &mut [f32; SUBFRAME_STEREO_SAMPLES], is_plc: bool) {
        if self.consecutive_plc_count > 0 {
            // Smoothly splice both WSOLA -> real Opus and WSOLA -> CELT PLC
            // using the active WSOLA continuation tail before clearing PLC state.
            for i in 0..CROSSFADE_FRAMES {
                let alpha = (i as f32) / (CROSSFADE_FRAMES as f32);
                let w_in = 0.5 * (1.0 - (alpha * std::f32::consts::PI).cos());
                out[i * 2] = out[i * 2] * w_in + self.plc_recovery_tail[i * 2] * (1.0 - w_in);
                out[i * 2 + 1] =
                    out[i * 2 + 1] * w_in + self.plc_recovery_tail[i * 2 + 1] * (1.0 - w_in);
            }
            self.consecutive_plc_count = 0;
            self.pending_declick = false;
        }
        if is_plc {
            // Mirror WSOLA's 40..60 ms fade-to-silence window on sustained
            // external CELT PLC runs so extended outages decay cleanly instead
            // of looping residual buzz, and declick the first recovery frame.
            self.consecutive_external_plc_count =
                self.consecutive_external_plc_count.saturating_add(1);
            let cnt = self.consecutive_external_plc_count;
            if cnt > 24 {
                out.fill(0.0);
            } else if cnt > 16 {
                let prev_gain = 1.0 - ((cnt - 17) as f32) / 8.0;
                let next_gain = 1.0 - ((cnt - 16) as f32) / 8.0;
                let denom = (SUBFRAME_FRAMES - 1).max(1) as f32;
                for i in 0..SUBFRAME_FRAMES {
                    let g = prev_gain + (next_gain - prev_gain) * ((i as f32) / denom);
                    out[i * 2] *= g;
                    out[i * 2 + 1] *= g;
                }
            }
        } else if self.consecutive_external_plc_count > 0 {
            if self.consecutive_external_plc_count > 16 {
                self.pending_declick = true;
            }
            self.consecutive_external_plc_count = 0;
        }
        self.apply_boundary_ramps(out);
        self.last_pop_was_fec = false;
        self.last_played = [
            out[SUBFRAME_STEREO_SAMPLES - 2],
            out[SUBFRAME_STEREO_SAMPLES - 1],
        ];
        self.append_to_history(out);
    }

    /// Invalidate a single ring slot if it currently belongs to `seq` so
    /// skipped or Opus-superseded subframes can never resurface 65,536
    /// subframes (~2.73 min) later after a 16-bit sequence wraparound.
    /// Uses an atomic CAS so it never races a concurrent `STATUS_BUSY` writer
    /// or wipes an unrelated epoch sharing the same ring index.
    #[inline]
    pub fn clear_slot(&self, seq: u16) {
        let slot = &self.slots[(seq as usize) & RING_MASK];
        loop {
            let cur = slot.state.load(Ordering::Acquire);
            let cur_seq = (cur >> 16) as u16;
            let cur_status = cur & 0xFFFF;
            if cur_status == STATUS_EMPTY || cur_seq != seq {
                return;
            }
            if cur_status == STATUS_BUSY {
                std::hint::spin_loop();
                continue;
            }
            if slot
                .state
                .compare_exchange_weak(cur, STATUS_EMPTY, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
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

    /// Find best pitch period P in [48..=600] frames (80 Hz .. 1000 Hz) via
    /// energy-normalized joint-stereo autocorrelation with ±1 lag refinement.
    #[inline]
    fn find_best_pitch_period(&self) -> usize {
        let template_start = HISTORY_FRAMES - CROSSFADE_FRAMES;
        let eval_lag = |lag: usize| -> f32 {
            let cand_start = template_start - lag;
            let mut corr = 0.0f32;
            let mut energy = 1e-6f32;
            for k in 0..CROSSFADE_FRAMES {
                let t_l = self.history[(template_start + k) * 2];
                let t_r = self.history[(template_start + k) * 2 + 1];
                let c_l = self.history[(cand_start + k) * 2];
                let c_r = self.history[(cand_start + k) * 2 + 1];
                corr += t_l * c_l + t_r * c_r;
                energy += c_l * c_l + c_r * c_r;
            }
            // Signed squared correlation normalized by candidate window energy (avoids sqrt!)
            (corr * corr.abs()) / energy
        };

        let mut coarse_lag = SUBFRAME_FRAMES;
        let mut best_score = f32::MIN;
        for lag in (48..=600).step_by(2) {
            let score = eval_lag(lag);
            // Require a 0.2% margin over the running peak so integer multiples
            // (2x, 3x period) do not steal the lock from the true fundamental.
            let threshold = if best_score == f32::MIN {
                f32::MIN
            } else {
                best_score + best_score.abs() * 0.002
            };
            if score > threshold {
                best_score = score;
                coarse_lag = lag;
            }
        }

        // Refine ±1 frame around coarse peak so odd pitch periods (e.g. 440 Hz = 109)
        // lock cleanly without frame-to-frame phase drift.
        let mut best_lag = coarse_lag;
        let lo = coarse_lag.saturating_sub(1).max(48);
        let hi = (coarse_lag + 1).min(600);
        for lag in lo..=hi {
            if lag == coarse_lag {
                continue;
            }
            let score = eval_lag(lag);
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
        for _ in 0..POP_BUSY_SPIN_LIMIT {
            let cur = slot.state.load(Ordering::Acquire);
            let cur_seq = (cur >> 16) as u16;
            let cur_status = cur & 0xFFFF;
            if cur_seq != expected_seq || cur_status == STATUS_EMPTY {
                break;
            }
            if cur_status == STATUS_BUSY {
                std::hint::spin_loop();
                continue;
            }
            if cur_status == STATUS_PRIMARY_LOSSLESS || cur_status == STATUS_HEALED_BY_FEC {
                let busy_state = ((expected_seq as u32) << 16) | STATUS_BUSY;
                if slot
                    .state
                    .compare_exchange_weak(cur, busy_state, Ordering::Acquire, Ordering::Relaxed)
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
                        // The PLC recovery crossfade already spliced frame 0.
                        // Disarm pending_declick so apply_boundary_ramps() below
                        // does not add the same step a second time.
                        self.pending_declick = false;
                    } else if cur_status == STATUS_HEALED_BY_FEC || self.last_pop_was_fec {
                        // Splice primary <-> FEC and FEC <-> FEC boundaries from the
                        // exact previous output sample for 0.17 ms.
                        let [last_l, last_r] = self.last_played;
                        for i in 0..FEC_DECLICK_FRAMES {
                            let alpha = (i as f32) / ((FEC_DECLICK_FRAMES - 1) as f32);
                            let w = 0.5 * (1.0 + (alpha * std::f32::consts::PI).cos());
                            out[i * 2] = last_l * w + out[i * 2] * (1.0 - w);
                            out[i * 2 + 1] = last_r * w + out[i * 2 + 1] * (1.0 - w);
                        }
                        self.pending_declick = false;
                    }

                    self.last_pop_was_fec = cur_status == STATUS_HEALED_BY_FEC;
                    if self.consecutive_external_plc_count > 16 {
                        self.pending_declick = true;
                    }
                    self.consecutive_external_plc_count = 0;
                    self.apply_boundary_ramps(out);
                    self.last_played = [
                        out[SUBFRAME_STEREO_SAMPLES - 2],
                        out[SUBFRAME_STEREO_SAMPLES - 1],
                    ];
                    self.append_to_history(out);
                    return cur_status;
                }
            } else {
                break;
            }
        }

        // WSOLA Pitch-Period Waveform Extrapolation
        self.consecutive_plc_count += 1;
        // Quiet fade-outs / between-song silence (<-48 dBFS): ramp cleanly to digital silence.
        let recent_peak = self.history[(HISTORY_FRAMES - SUBFRAME_FRAMES) * 2..]
            .iter()
            .fold(0.0f32, |m, s| m.max(s.abs()));
        if recent_peak < 0.004 {
            let [last_l, last_r] = self.last_played;
            for i in 0..SUBFRAME_FRAMES {
                let w = if i < CROSSFADE_FRAMES {
                    let alpha = (i as f32) / (CROSSFADE_FRAMES as f32);
                    0.5 * (1.0 + (alpha * std::f32::consts::PI).cos())
                } else {
                    0.0
                };
                out[i * 2] = last_l * w;
                out[i * 2 + 1] = last_r * w;
            }
            self.plc_recovery_tail.fill(0.0);
            self.last_pop_was_fec = false;
            self.last_played = [
                out[SUBFRAME_STEREO_SAMPLES - 2],
                out[SUBFRAME_STEREO_SAMPLES - 1],
            ];
            self.append_to_history(out);
            return STATUS_EMPTY;
        }

        let (start_attenuation, target_attenuation) = if self.consecutive_plc_count == 1 {
            (1.0f32, 0.90f32)
        } else if self.consecutive_plc_count <= 16 {
            (0.85f32, 0.85f32)
        } else if self.consecutive_plc_count <= 24 {
            // Fade smoothly to digital silence between 40 ms and 60 ms so long
            // stock-OS Wi-Fi scan freezes never loop as metallic buzz.
            let prev_frac = ((self.consecutive_plc_count - 17) as f32) / 8.0;
            let next_frac = ((self.consecutive_plc_count - 16) as f32) / 8.0;
            ((1.0 - prev_frac) * 0.85, (1.0 - next_frac) * 0.85)
        } else {
            (0.0f32, 0.0f32)
        };
        let pitch_lag = self.find_best_pitch_period();
        let [last_l, last_r] = self.last_played;

        // Keep an unattenuated copy of the extrapolated waveform so `self.history`
        // does not compound `0.85^k` volume collapse across consecutive PLC ticks.
        let mut raw_plc = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        let copy_start = HISTORY_FRAMES - pitch_lag;
        for i in 0..SUBFRAME_FRAMES {
            let frac = (i as f32) / ((SUBFRAME_FRAMES - 1) as f32);
            let attenuation = start_attenuation + (target_attenuation - start_attenuation) * frac;
            let src_frame = copy_start + (i % pitch_lag);
            let raw_l = self.history[src_frame * 2];
            let raw_r = self.history[src_frame * 2 + 1];
            raw_plc[i * 2] = raw_l;
            raw_plc[i * 2 + 1] = raw_r;

            let mut l = raw_l * attenuation;
            let mut r = raw_r * attenuation;

            if self.consecutive_plc_count == 1 {
                if i < PLC_ONSET_SPLICE_FRAMES {
                    let alpha = (i as f32) / ((PLC_ONSET_SPLICE_FRAMES - 1) as f32);
                    let w_in = 0.5 * (1.0 - (alpha * std::f32::consts::PI).cos());
                    l = last_l * (1.0 - w_in) + l * w_in;
                    r = last_r * (1.0 - w_in) + r * w_in;
                }
            } else if i < CROSSFADE_FRAMES {
                let alpha = (i as f32) / ((CROSSFADE_FRAMES - 1) as f32);
                let w_in = 0.5 * (1.0 - (alpha * std::f32::consts::PI).cos());
                l = self.plc_recovery_tail[i * 2] * (1.0 - w_in) + l * w_in;
                r = self.plc_recovery_tail[i * 2 + 1] * (1.0 - w_in) + r * w_in;
            }

            out[i * 2] = l;
            out[i * 2 + 1] = r;
        }

        // Prepare continuation tail at the current output attenuation for the next
        // PLC frame or real packet recovery.
        for i in 0..CROSSFADE_FRAMES {
            let src_frame = copy_start + ((SUBFRAME_FRAMES + i) % pitch_lag);
            self.plc_recovery_tail[i * 2] = self.history[src_frame * 2] * target_attenuation;
            self.plc_recovery_tail[i * 2 + 1] =
                self.history[src_frame * 2 + 1] * target_attenuation;
        }

        self.last_pop_was_fec = false;
        self.last_played = [
            out[SUBFRAME_STEREO_SAMPLES - 2],
            out[SUBFRAME_STEREO_SAMPLES - 1],
        ];
        self.append_to_history(&raw_plc);
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

    fn fec_sine_frame(subframe_index: u16) -> [f32; SUBFRAME_STEREO_SAMPLES] {
        let mut frame = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        for i in 0..SUBFRAME_FRAMES {
            let n = subframe_index as usize * SUBFRAME_FRAMES + i;
            let v = ((n as f32) * 2.0 * std::f32::consts::PI * 300.0 / 48_000.0).sin() * 0.5;
            frame[i * 2] = v;
            frame[i * 2 + 1] = v;
        }
        frame
    }

    fn fec_roundtrip(frame: &[f32; SUBFRAME_STEREO_SAMPLES]) -> [f32; SUBFRAME_STEREO_SAMPLES] {
        use crate::lpc_lossless::{
            decode_fec_subframe_to_f32, encode_fec_subframe, ADPCM_FEC_BYTES,
        };

        let mut pcm_i16 = [0i16; SUBFRAME_STEREO_SAMPLES];
        for (dst, &s) in pcm_i16.iter_mut().zip(frame.iter()) {
            *dst = (s * 32767.0) as i16;
        }
        let mut fec_bytes = [0u8; ADPCM_FEC_BYTES];
        encode_fec_subframe(&pcm_i16, &mut fec_bytes);
        let mut decoded = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        assert!(decode_fec_subframe_to_f32(&fec_bytes, &mut decoded));
        decoded
    }

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
    fn test_plc_pitch_search_survives_anti_phase_stereo() {
        let mut seq = ZeroJitterSequencer::new();
        // 240 Hz => a 200-frame period, comfortably inside the 48..=240 search
        // range. L and R are exact opposites, so summing them before the
        // correlation cancels the signal to zero at every candidate lag.
        for n in 0..HISTORY_FRAMES {
            let v = ((n as f32) * 2.0 * std::f32::consts::PI * 240.0 / 48_000.0).sin() * 0.5;
            seq.history[n * 2] = v;
            seq.history[n * 2 + 1] = -v;
        }

        let lag = seq.find_best_pitch_period();
        println!("anti-phase pitch lag: {}", lag);
        assert!(
            (lag as i32 - 200).abs() <= 2,
            "anti-phase stereo chose lag {} ({} Hz), expected 200 frames",
            lag,
            48_000 / lag
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
        // The first 32 frames fade in from silence on a fresh stream; the
        // steady-state sample must still be the exact primary value.
        assert_eq!(out[CROSSFADE_FRAMES * 2], 0.25);
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

    #[test]
    fn test_primary_to_fec_entry_is_declicked() {
        let mut seq = ZeroJitterSequencer::new();
        let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];

        for s in 0..2u16 {
            let frame = fec_sine_frame(s);
            assert!(seq.push_primary(s, &frame));
            assert_eq!(seq.pop_next(s, &mut out), STATUS_PRIMARY_LOSSLESS);
        }
        let last_l = out[SUBFRAME_STEREO_SAMPLES - 2];
        let last_r = out[SUBFRAME_STEREO_SAMPLES - 1];

        let third = fec_sine_frame(2);
        let fec = fec_roundtrip(&third);
        assert!(seq.try_heal_with_fec(2, &fec));
        assert_eq!(seq.pop_next(2, &mut out), STATUS_HEALED_BY_FEC);

        assert!(
            (out[0] - last_l).abs() < 1e-6 && (out[1] - last_r).abs() < 1e-6,
            "primary->FEC entry stepped from ({last_l}, {last_r}) to ({}, {})",
            out[0],
            out[1]
        );
        // The splice must end before the bit-exact FEC waveform resumes.
        for i in FEC_DECLICK_FRAMES..SUBFRAME_FRAMES {
            assert_eq!(out[i * 2], fec[i * 2], "entry splice leaked past frame {i}");
            assert_eq!(out[i * 2 + 1], fec[i * 2 + 1]);
        }
    }

    #[test]
    fn test_fec_to_fec_boundary_is_declicked() {
        let mut seq = ZeroJitterSequencer::new();
        let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];

        for s in 0..2u16 {
            let frame = fec_sine_frame(s);
            assert!(seq.push_primary(s, &frame));
            assert_eq!(seq.pop_next(s, &mut out), STATUS_PRIMARY_LOSSLESS);
        }

        let second_fec = fec_roundtrip(&fec_sine_frame(2));
        assert!(seq.try_heal_with_fec(2, &second_fec));
        assert_eq!(seq.pop_next(2, &mut out), STATUS_HEALED_BY_FEC);
        let last_l = out[SUBFRAME_STEREO_SAMPLES - 2];
        let last_r = out[SUBFRAME_STEREO_SAMPLES - 1];

        let third_fec = fec_roundtrip(&fec_sine_frame(3));
        assert!(seq.try_heal_with_fec(3, &third_fec));
        assert_eq!(seq.pop_next(3, &mut out), STATUS_HEALED_BY_FEC);
        assert!(
            (out[0] - last_l).abs() < 1e-6 && (out[1] - last_r).abs() < 1e-6,
            "FEC->FEC boundary stepped from ({last_l}, {last_r}) to ({}, {})",
            out[0],
            out[1]
        );
    }

    #[test]
    fn test_fec_entry_splice_never_overshoots_full_scale_flip() {
        let mut seq = ZeroJitterSequencer::new();
        let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        let hi = [0.95f32; SUBFRAME_STEREO_SAMPLES];

        for s in 0..2u16 {
            assert!(seq.push_primary(s, &hi));
            assert_eq!(seq.pop_next(s, &mut out), STATUS_PRIMARY_LOSSLESS);
        }

        // A 2 kHz full-scale flip immediately after +0.95: additive splicing
        // would push the first samples toward +2.4 and slam the limiter.
        let mut flip = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        for i in 0..SUBFRAME_FRAMES {
            let v = -((i as f32) * std::f32::consts::TAU * 2000.0 / 48_000.0).cos() * 0.95;
            flip[i * 2] = v;
            flip[i * 2 + 1] = v;
        }
        let fec = fec_roundtrip(&flip);
        assert!(seq.try_heal_with_fec(2, &fec));
        assert_eq!(seq.pop_next(2, &mut out), STATUS_HEALED_BY_FEC);

        let peak = out.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        assert!(
            peak <= 1.0 + 1e-5,
            "convex FEC entry splice overshot input peak: {peak}"
        );
    }

    #[test]
    fn test_finalize_external_plc_clears_stale_wsola_tail() {
        let mut seq = ZeroJitterSequencer::new();
        let loud = [0.8f32; SUBFRAME_STEREO_SAMPLES];
        assert!(seq.push_primary(0, &loud));
        let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        seq.pop_next(0, &mut out);
        // Trigger WSOLA PLC -> consecutive_plc_count == 1, tail holds ~0.72
        seq.pop_next(1, &mut out);
        assert_eq!(seq.consecutive_plc_count(), 1);
        // CELT PLC takes over
        let mut celt_plc = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        seq.finalize_external_frame(&mut celt_plc, true);
        assert_eq!(seq.consecutive_plc_count(), 0);
        // Subsequent silent real Opus frame must not inherit the old 0.72 WSOLA tail
        let mut real_silent = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        seq.finalize_external_frame(&mut real_silent, false);
        assert!(real_silent[0].abs() < 1e-6);
    }

    #[test]
    fn test_wsola_plc_fades_to_silence_after_40ms() {
        let mut seq = ZeroJitterSequencer::new();
        for s in 0..3u16 {
            let mut frame = [0.0f32; SUBFRAME_STEREO_SAMPLES];
            for i in 0..SUBFRAME_FRAMES {
                let n = s as usize * SUBFRAME_FRAMES + i;
                let v = ((n as f32) * std::f32::consts::TAU * 300.0 / 48_000.0).sin() * 0.5;
                frame[i * 2] = v;
                frame[i * 2 + 1] = v;
            }
            assert!(seq.push_primary(s, &frame));
            let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
            assert_eq!(seq.pop_next(s, &mut out), STATUS_PRIMARY_LOSSLESS);
        }
        let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        for miss in 3..30u16 {
            assert_eq!(seq.pop_next(miss, &mut out), STATUS_EMPTY);
        }
        let tail_peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(tail_peak < 1e-6, "PLC must fade to silence after 60 ms, got {tail_peak}");
    }

    #[test]
    fn test_arm_declick_applies_raised_cosine_splice() {
        let mut seq = ZeroJitterSequencer::new();
        let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];

        // Establish steady +0.5 output history (pop two frames so the initial
        // fade-in on frame 0 is complete).
        let hi = [0.5f32; SUBFRAME_STEREO_SAMPLES];
        assert!(seq.push_primary(0, &hi));
        assert_eq!(seq.pop_next(0, &mut out), STATUS_PRIMARY_LOSSLESS);
        assert!(seq.push_primary(1, &hi));
        assert_eq!(seq.pop_next(1, &mut out), STATUS_PRIMARY_LOSSLESS);

        // Arm declick before a full-scale polarity flip (-0.5) such as a
        // Lossless <-> Opus lookahead jump.
        seq.arm_declick();
        let lo = [-0.5f32; SUBFRAME_STEREO_SAMPLES];
        assert!(seq.push_primary(2, &lo));
        assert_eq!(seq.pop_next(2, &mut out), STATUS_PRIMARY_LOSSLESS);

        // Frame 0 starts exactly at the previous sample (+0.5) with zero jump.
        assert!((out[0] - 0.5).abs() < 1e-6, "frame 0 stepped to {}", out[0]);
        // Midpoint (frame 24) sits at 0.0 (halfway through raised cosine).
        assert!(out[24 * 2].abs() < 1e-5, "midpoint was {}", out[24 * 2]);
        // Monotonically decreasing across the 48-frame splice window.
        for i in 1..48 {
            assert!(
                out[i * 2] <= out[(i - 1) * 2] + 1e-6,
                "non-monotonic splice at frame {i}"
            );
        }
        // Unmodified from frame 48 onward.
        for i in 48..SUBFRAME_FRAMES {
            assert_eq!(out[i * 2], -0.5, "splice leaked past frame {i}");
        }
    }

    #[test]
    fn test_arm_declick_never_overshoots_on_ac_phase_flip() {
        let mut seq = ZeroJitterSequencer::new();
        let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];

        // Previous frame ends at +0.95.
        let prev = [0.95f32; SUBFRAME_STEREO_SAMPLES];
        assert!(seq.push_primary(0, &prev));
        assert_eq!(seq.pop_next(0, &mut out), STATUS_PRIMARY_LOSSLESS);
        assert!(seq.push_primary(1, &prev));
        assert_eq!(seq.pop_next(1, &mut out), STATUS_PRIMARY_LOSSLESS);

        // Next frame starts at -0.95 then swings to +0.95 within 12 frames (a
        // 2 kHz full-scale tone). The old additive `out + dl * w` pushed peaks
        // to +2.45 and slammed the limiter; convex blending stays strictly <= 0.95.
        let mut ac = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        for i in 0..SUBFRAME_FRAMES {
            let v = -((i as f32) * std::f32::consts::TAU * 2000.0 / 48_000.0).cos() * 0.95;
            ac[i * 2] = v;
            ac[i * 2 + 1] = v;
        }
        seq.arm_declick();
        assert!(seq.push_primary(2, &ac));
        assert_eq!(seq.pop_next(2, &mut out), STATUS_PRIMARY_LOSSLESS);

        let peak = out.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        assert!(
            peak <= 0.95 + 1e-5,
            "convex declick overshot input peak: {peak}"
        );
    }

    #[test]
    fn test_wsola_plc_ramps_quiet_tails_to_silence() {
        let mut seq = ZeroJitterSequencer::new();
        let mut quiet_tail = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        // -60 dBFS (0.001) quiet fade-out / reverb tail.
        for i in 0..SUBFRAME_FRAMES {
            let v = ((i as f32) * std::f32::consts::TAU * 1000.0 / 48_000.0).sin() * 0.001;
            quiet_tail[i * 2] = v;
            quiet_tail[i * 2 + 1] = v;
        }
        for s in 0..3u16 {
            assert!(seq.push_primary(s, &quiet_tail));
            let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
            assert_eq!(seq.pop_next(s, &mut out), STATUS_PRIMARY_LOSSLESS);
        }

        let mut plc_out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        assert_eq!(seq.pop_next(3, &mut plc_out), STATUS_EMPTY);
        // After the 32-frame raised-cosine ramp, trailing samples must be exact digital zero
        // with no 1 kHz pitch-loop oscillation.
        assert!(
            plc_out[CROSSFADE_FRAMES * 2..]
                .iter()
                .all(|&s| s == 0.0),
            "quiet-tail PLC leaked non-zero audio after ramp"
        );
    }

    #[test]
    fn test_wsola_plc_holds_steady_gain_at_480hz_and_tracks_100hz_bass() {
        // 480 Hz (lag = 100 <= 120) previously compounded 0.85^k down to ~0.16.
        let mut seq480 = ZeroJitterSequencer::new();
        for s in 0..4u16 {
            let mut f = [0.0f32; SUBFRAME_STEREO_SAMPLES];
            for i in 0..SUBFRAME_FRAMES {
                let n = s as usize * SUBFRAME_FRAMES + i;
                let v = ((n as f32) * std::f32::consts::TAU * 480.0 / 48_000.0).sin() * 0.5;
                f[i * 2] = v;
                f[i * 2 + 1] = v;
            }
            assert!(seq480.push_primary(s, &f));
            let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
            assert_eq!(seq480.pop_next(s, &mut out), STATUS_PRIMARY_LOSSLESS);
        }
        let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        for miss in 4..16u16 {
            assert_eq!(seq480.pop_next(miss, &mut out), STATUS_EMPTY);
        }
        let peak_12 = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak_12 > 0.40, "480 Hz PLC collapsed to {peak_12}");

        // 100 Hz (lag = 480) across 6 primed frames (720-frame history) must not click.
        let mut seq100 = ZeroJitterSequencer::new();
        let mut prev = 0.0f32;
        for s in 0..6u16 {
            let mut f = [0.0f32; SUBFRAME_STEREO_SAMPLES];
            for i in 0..SUBFRAME_FRAMES {
                let n = s as usize * SUBFRAME_FRAMES + i;
                let v = ((n as f32) * std::f32::consts::TAU * 100.0 / 48_000.0).sin() * 0.5;
                f[i * 2] = v;
                f[i * 2 + 1] = v;
            }
            assert!(seq100.push_primary(s, &f));
            assert_eq!(seq100.pop_next(s, &mut out), STATUS_PRIMARY_LOSSLESS);
            prev = out[SUBFRAME_STEREO_SAMPLES - 2];
        }
        assert_eq!(seq100.pop_next(6, &mut out), STATUS_EMPTY);
        let mut max_step = 0.0f32;
        for i in 0..SUBFRAME_FRAMES {
            max_step = max_step.max((out[i * 2] - prev).abs());
            prev = out[i * 2];
        }
        assert!(max_step < 0.08, "100 Hz PLC onset clicked with step {max_step}");
    }

    #[test]
    fn test_finalize_external_wsola_to_celt_plc_crossfades_smoothly() {
        let mut seq = ZeroJitterSequencer::new();
        let loud = [0.6f32; SUBFRAME_STEREO_SAMPLES];
        assert!(seq.push_primary(0, &loud));
        let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        assert_eq!(seq.pop_next(0, &mut out), STATUS_PRIMARY_LOSSLESS);
        assert_eq!(seq.pop_next(1, &mut out), STATUS_EMPTY);
        assert_eq!(seq.consecutive_plc_count(), 1);
        let mut celt_plc = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        seq.finalize_external_frame(&mut celt_plc, true);
        assert_eq!(seq.consecutive_plc_count(), 0);
        // Sample 0 must crossfade from active WSOLA tail rather than snapping to 0.0.
        assert!(celt_plc[0] > 0.4, "WSOLA->CELT PLC snapped to {}", celt_plc[0]);
    }

    #[test]
    fn test_finalize_external_celt_plc_fades_to_silence_and_declicks_recovery() {
        let mut seq = ZeroJitterSequencer::new();
        let mut prime = [0.5f32; SUBFRAME_STEREO_SAMPLES];
        seq.finalize_external_frame(&mut prime, false);
        for tick in 1..=26u32 {
            let mut plc = [0.5f32; SUBFRAME_STEREO_SAMPLES];
            seq.finalize_external_frame(&mut plc, true);
            if tick <= 16 {
                assert!((plc[0] - 0.5).abs() < 1e-6);
            } else if tick <= 24 {
                assert!(plc[SUBFRAME_STEREO_SAMPLES - 1] < plc[0]);
            } else {
                assert!(plc.iter().all(|&s| s == 0.0));
            }
        }
        let mut rec = [0.8f32; SUBFRAME_STEREO_SAMPLES];
        seq.finalize_external_frame(&mut rec, false);
        assert!(rec[0].abs() < 0.05, "expected recovery declick splice from 0.0, got {}", rec[0]);
        assert!((rec[SUBFRAME_STEREO_SAMPLES - 1] - 0.8).abs() < 1e-6);
    }
}
