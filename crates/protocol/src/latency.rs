//! Shared live latency breakdown for the desktop GUI, Android HUD, and CLI.
//!
//! The snapshot is assembled from lock-free atomics in the client engine. It
//! deliberately separates measured values (network jitter, target FIFO, AAudio
//! DAC queue) from configured/estimated values (sender packetization, desktop
//! output buffer, A2DP 145 ms). The total is an ingress-to-speaker estimate,
//! not a glass-to-glass measurement; use the acoustic sonar for that.

/// One 48 kHz subframe is 2.5 ms.
pub const SUBFRAME_MS: f32 = 2.5;

/// Desktop CPAL asks for a 240-frame (5 ms) callback buffer. If the device
/// rejects that period size the backend falls back to its default, so this is
/// the configured floor rather than a measured DAC queue.
pub const DESKTOP_OUTPUT_BUFFER_MS: f32 = 5.0;

/// Fixed A2DP/SCO/BLE downstream estimate used by the Android UI. It is only
/// reported when media is actually routed to Bluetooth.
pub const BLUETOOTH_A2DP_ESTIMATE_MS: u16 = 145;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ClientLatencySnapshot {
    /// Sender bundling interval (`subframe_count * 2.5 ms`).
    pub packet_interval_ms: f32,
    /// RFC 3550 inter-arrival jitter estimate. This sizes the FIFO; it is not
    /// added to the total separately.
    pub network_jitter_ms: f32,
    /// Adaptive network cushion before group-sync alignment.
    pub adaptive_buffer_ms: f32,
    /// Final clamped FIFO target, including effective group-sync alignment.
    pub target_buffer_ms: f32,
    /// Live combined sequencer + output FIFO depth.
    pub buffered_audio_ms: f32,
    /// Effective group-sync addition after the 4..12 subframe FIFO clamp.
    pub group_sync_extra_ms: f32,
    /// Measured AAudio DAC/HAL queue (0 when unavailable or not yet polled).
    pub hardware_delay_ms: u16,
    /// Fixed A2DP estimate reported by Android when media is routed to BT.
    pub bluetooth_delay_ms: u16,
    /// Desktop configured output buffer; 0 on Android where the DAC queue is
    /// measured instead.
    pub output_buffer_ms: f32,
    pub prebuffer_ready: bool,
    pub codec_mode: u8,
}

impl ClientLatencySnapshot {
    /// Downstream stage: the largest of the measured DAC queue, the fixed A2DP
    /// estimate, and the desktop output buffer.
    ///
    /// These overlap (the AAudio queue can include the Bluetooth HAL's own
    /// buffering), so taking the max matches the group-sync logic and avoids
    /// double-counting the A2DP estimate on top of the measured queue.
    pub fn downstream_stage_ms(&self) -> f32 {
        (self.hardware_delay_ms as f32)
            .max(self.bluetooth_delay_ms as f32)
            .max(self.output_buffer_ms)
    }

    /// Estimated time from a packet entering the client socket to the speaker.
    ///
    /// `packet_interval + target FIFO + downstream stage`. The target FIFO
    /// already contains the effective group-sync addition, so it is not added
    /// again. Unknown one-way network transit and the acoustic air path are not
    /// included; the sonar is the only measurement that covers those.
    pub fn estimated_one_way_ms(&self) -> f32 {
        self.packet_interval_ms + self.target_buffer_ms + self.downstream_stage_ms()
    }

    pub fn codec_name(&self) -> &'static str {
        codec_mode_name(self.codec_mode)
    }
}

pub fn codec_mode_name(mode: u8) -> &'static str {
    match mode {
        0 => "Lossless LPC",
        1 => "Raw PCM16",
        2 => "VBR Compressed",
        3 => "DTX Silence",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downstream_stage_uses_max_not_sum() {
        let snap = ClientLatencySnapshot {
            hardware_delay_ms: 20,
            bluetooth_delay_ms: BLUETOOTH_A2DP_ESTIMATE_MS,
            output_buffer_ms: DESKTOP_OUTPUT_BUFFER_MS,
            ..Default::default()
        };
        assert_eq!(snap.downstream_stage_ms(), 145.0);
    }

    #[test]
    fn estimate_adds_packetization_target_and_downstream() {
        let snap = ClientLatencySnapshot {
            packet_interval_ms: 5.0,
            target_buffer_ms: 15.0,
            hardware_delay_ms: 17,
            output_buffer_ms: DESKTOP_OUTPUT_BUFFER_MS,
            ..Default::default()
        };
        assert_eq!(snap.estimated_one_way_ms(), 37.0);
    }

    #[test]
    fn desktop_output_buffer_is_the_fallback_when_dac_is_unmeasured() {
        let snap = ClientLatencySnapshot {
            output_buffer_ms: DESKTOP_OUTPUT_BUFFER_MS,
            ..Default::default()
        };
        assert_eq!(snap.downstream_stage_ms(), DESKTOP_OUTPUT_BUFFER_MS);
    }

    #[test]
    fn group_sync_is_already_inside_target_buffer() {
        let snap = ClientLatencySnapshot {
            packet_interval_ms: 2.5,
            target_buffer_ms: 22.5,
            group_sync_extra_ms: 7.5,
            output_buffer_ms: DESKTOP_OUTPUT_BUFFER_MS,
            ..Default::default()
        };
        assert_eq!(snap.estimated_one_way_ms(), 30.0);
    }

    #[test]
    fn codec_names_cover_all_wire_modes() {
        assert_eq!(codec_mode_name(0), "Lossless LPC");
        assert_eq!(codec_mode_name(1), "Raw PCM16");
        assert_eq!(codec_mode_name(2), "VBR Compressed");
        assert_eq!(codec_mode_name(3), "DTX Silence");
        assert_eq!(codec_mode_name(99), "Unknown");
    }
}
