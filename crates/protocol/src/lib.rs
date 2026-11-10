#![forbid(unsafe_op_in_unsafe_fn)]

pub mod crypto;
pub mod dsp;
pub mod latency;
pub mod lpc_lossless;
pub mod sequencer;
pub mod sync_and_eq;
pub mod wire;

/// Standard real-time operating sample rate (48,000 Hz)
pub const SAMPLE_RATE: u32 = 48_000;

/// Internal DSP & Sub-frame quantum: 2.5 ms at 48 kHz = 120 frames per channel
pub const SUBFRAME_FRAMES: usize = 120;

/// Stereo channel count
pub const CHANNELS: usize = 2;

/// Total interleaved stereo samples per 2.5 ms subframe (120 * 2 = 240 samples)
pub const SUBFRAME_STEREO_SAMPLES: usize = SUBFRAME_FRAMES * CHANNELS;

/// Maximum UDP payload size to strictly prevent IP fragmentation on any Wi-Fi/VLAN MTU (1500B)
pub const MAX_SAFE_UDP_PAYLOAD: usize = 1380;
