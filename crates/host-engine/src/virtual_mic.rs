#[cfg(target_os = "linux")]
use std::io::Write;
use std::net::UdpSocket;
#[cfg(target_os = "linux")]
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use audioshare_protocol::dsp::{BurstDecouplerRing, TruePeakLimiter};
use audioshare_protocol::lpc_lossless::{decode_fec_subframe_to_f32, decode_lossless_subframe};
use audioshare_protocol::sequencer::ZeroJitterSequencer;
use audioshare_protocol::wire::ParsedPacket;
use audioshare_protocol::{MAX_SAFE_UDP_PAYLOAD, SUBFRAME_STEREO_SAMPLES};

use crate::{configure_realtime_udp_socket, promote_thread_realtime};

pub const PHONE_MIC_PORT: u16 = 65532;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhoneRxRoutingMode {
    /// Routes phone mic into a dedicated Virtual Microphone for Zoom / Discord / Google Meet calls (no self-echo!)
    VirtualMicForCalls = 0,
    /// Plays incoming phone audio directly on PC Speakers / Headphones (Phone-to-PC media streaming!)
    PlayOnPcSpeakers = 1,
}

/// Number of buffered subframes between the highest received sequence and the
/// next sequence to play, using wrapping 16-bit arithmetic.
fn buffered_subframes(latest_seq: u16, next_seq: u16) -> i16 {
    (latest_seq.wrapping_add(1).wrapping_sub(next_seq) as i16).max(0)
}

/// Phone-mic playhead rule: advance only when the slot contained audio or a
/// newer packet is already buffered.
///
/// The receiver ticks from the PC `Instant` clock. If the PC clock runs faster
/// than the phone (or a brief Wi-Fi pause leaves the timer running), blindly
/// incrementing `next_seq` walks the playhead ahead of `latest_seq`; every
/// later phone packet is then rejected as "past" even though it is the newest
/// audio available. Holding the playhead while the slot is empty and no newer
/// packet exists lets the phone sequence catch up.
#[inline]
fn should_advance_mic_playhead(status: u32, latest_seq: u16, next_seq: u16) -> bool {
    status != audioshare_protocol::sequencer::STATUS_EMPTY
        || (latest_seq.wrapping_sub(next_seq) as i16) >= 0
}

/// Decode every length-prefixed subframe in a primary bundle.
///
/// The phone transmitter may bundle up to four 2.5 ms subframes per datagram.
/// Each decoded subframe is reported with its index so the caller can map it
/// to `base_seq + index`; reading only index 0 drops half of the audio in the
/// two-subframe Party Mode / PhoneAudioCaster path.
fn decode_primary_bundle(
    primary: &[u8],
    subframe_count: u8,
    mut on_subframe: impl FnMut(usize, &[f32; SUBFRAME_STEREO_SAMPLES]),
) -> usize {
    let mut cursor = 0usize;
    let mut decoded = 0usize;
    for sub_idx in 0..subframe_count as usize {
        if cursor + 2 > primary.len() {
            break;
        }
        let sub_len = u16::from_be_bytes([primary[cursor], primary[cursor + 1]]) as usize;
        cursor += 2;
        if sub_len == 0 || cursor + sub_len > primary.len() {
            break;
        }

        let mut pcm_i16 = [0i16; SUBFRAME_STEREO_SAMPLES];
        if decode_lossless_subframe(&primary[cursor..cursor + sub_len], &mut pcm_i16).is_ok() {
            let mut pcm_f32 = [0.0f32; SUBFRAME_STEREO_SAMPLES];
            for (dst, &s) in pcm_f32.iter_mut().zip(pcm_i16.iter()) {
                *dst = (s as f32) * (1.0 / 32768.0);
            }
            on_subframe(sub_idx, &pcm_f32);
            decoded += 1;
        }
        cursor += sub_len;
    }
    decoded
}

/// Feeds the received phone-mic PCM into a PulseAudio/PipeWire null sink with
/// `pacat`, while `module-remap-source` exposes that sink's monitor as the
/// `AudioShare_Phone_Mic` recording device.
#[cfg(target_os = "linux")]
struct LinuxVirtualMicOutput {
    child: Child,
    writer: Option<std::thread::JoinHandle<()>>,
    running: Arc<AtomicBool>,
}

#[cfg(target_os = "linux")]
impl LinuxVirtualMicOutput {
    fn start(fifo: Arc<BurstDecouplerRing>, running: Arc<AtomicBool>) -> Option<Self> {
        let mut child = Command::new("pacat")
            .args([
                "--playback",
                "--device=AudioShare_Phone_Mic_Sink",
                "--format=float32le",
                "--rate=48000",
                "--channels=2",
                "--raw",
                "--latency-msec=20",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        let Some(mut stdin) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        };

        let writer_running = Arc::clone(&running);
        let writer = match std::thread::Builder::new()
            .name("as-ultra-phone-mic-sink".into())
            .spawn(move || {
                let mut rng = 0x2468_ACE0u32;
                let mut frame = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                let mut bytes = vec![0u8; SUBFRAME_STEREO_SAMPLES * 4];
                while writer_running.load(Ordering::Relaxed) {
                    if fifo.available_samples() >= SUBFRAME_STEREO_SAMPLES {
                        fifo.pop_exact_or_dither(&mut frame, &mut rng);
                        for (i, sample) in frame.iter().enumerate() {
                            bytes[i * 4..i * 4 + 4].copy_from_slice(&sample.to_le_bytes());
                        }
                        if stdin.write_all(&bytes).is_err() {
                            break;
                        }
                    } else {
                        std::thread::sleep(Duration::from_micros(500));
                    }
                }
            }) {
            Ok(handle) => handle,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        };

        Some(Self {
            child,
            writer: Some(writer),
            running,
        })
    }
}

#[cfg(target_os = "linux")]
impl Drop for LinuxVirtualMicOutput {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(handle) = self.writer.take() {
            let _ = handle.join();
        }
    }
}

pub struct PhoneMicReceiver {
    running: Arc<AtomicBool>,
    pub mic_packets_received: Arc<AtomicU64>,
    pub mic_peak_level_pct: Arc<AtomicU64>,
    pub last_chirp_rx_timestamp_us: Arc<AtomicU64>,
    #[cfg(target_os = "linux")]
    pactl_mic_module_id: Option<String>,
    #[cfg(target_os = "linux")]
    pactl_mic_source_module_id: Option<String>,
    #[cfg(target_os = "linux")]
    linux_output: Option<LinuxVirtualMicOutput>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl PhoneMicReceiver {
    pub fn start(port: u16) -> Result<Self, String> {
        Self::start_with_mode(port, PhoneRxRoutingMode::VirtualMicForCalls)
    }

    pub fn start_with_mode(port: u16, mode: PhoneRxRoutingMode) -> Result<Self, String> {
        let _ = mode;
        let socket = UdpSocket::bind(("0.0.0.0", port))
            .map_err(|e| format!("Failed to bind Phone Mic UDP port {}: {}", port, e))?;
        configure_realtime_udp_socket(&socket)
            .map_err(|e| format!("Failed to configure Phone Mic socket: {}", e))?;
        let _ = socket.set_nonblocking(true);

        #[cfg(target_os = "linux")]
        let (pactl_mic_module_id, pactl_mic_source_module_id) =
            if mode == PhoneRxRoutingMode::VirtualMicForCalls {
                // 1. Create underlying null sink to receive the phone's audio stream silently
                let sink_id = Command::new("pactl")
                    .args([
                        "load-module",
                        "module-null-sink",
                        "sink_name=AudioShare_Phone_Mic_Sink",
                        "sink_properties=device.description=AudioShare_Phone_Sink",
                    ])
                    .output()
                    .ok()
                    .and_then(|o| {
                        if o.status.success() {
                            Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                        } else {
                            None
                        }
                    });

                // 2. Create a true non-monitor Virtual Microphone (`module-remap-source`)
                //    so Zoom, Discord, Google Meet, Teams & Chrome show it as a real Microphone Input!
                let remap_id = Command::new("pactl")
                    .args([
                        "load-module",
                        "module-remap-source",
                        "master=AudioShare_Phone_Mic_Sink.monitor",
                        "source_name=AudioShare_Phone_Mic",
                        "source_properties=device.description=AudioShare_Phone_Microphone",
                    ])
                    .output()
                    .ok()
                    .and_then(|o| {
                        if o.status.success() {
                            Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                        } else {
                            None
                        }
                    });

                let _ = Command::new("pactl")
                    .args(["set-default-source", "AudioShare_Phone_Mic"])
                    .output();
                (sink_id, remap_id)
            } else {
                (None, None)
            };

        let running = Arc::new(AtomicBool::new(true));
        let mic_packets_received = Arc::new(AtomicU64::new(0));
        let mic_peak_level_pct = Arc::new(AtomicU64::new(0));
        let last_chirp_rx_timestamp_us = Arc::new(AtomicU64::new(0));

        let mic_fifo = Arc::new(BurstDecouplerRing::new());

        #[cfg(target_os = "linux")]
        let linux_output = if mode == PhoneRxRoutingMode::VirtualMicForCalls {
            LinuxVirtualMicOutput::start(Arc::clone(&mic_fifo), Arc::clone(&running))
        } else {
            None
        };
        #[cfg(target_os = "linux")]
        let linux_output_active = linux_output.is_some();

        let run_clone = Arc::clone(&running);
        let pkts_clone = Arc::clone(&mic_packets_received);
        let peak_clone = Arc::clone(&mic_peak_level_pct);
        let chirp_clone = Arc::clone(&last_chirp_rx_timestamp_us);
        let mic_fifo_thread = Arc::clone(&mic_fifo);

        let handle = std::thread::Builder::new()
            .name("as-ultra-phone-mic-rx".into())
            .spawn(move || {
                promote_thread_realtime();
                let mut sequencer = ZeroJitterSequencer::new();
                // +6 dB is a voice-mic pre-amp. Music cast to PC speakers must
                // stay at unity gain and only use the true-peak ceiling.
                let preamp_db = if mode == PhoneRxRoutingMode::VirtualMicForCalls {
                    6.0
                } else {
                    0.0
                };
                let mut limiter = TruePeakLimiter::new(preamp_db);
                let mut mic_gate = audioshare_protocol::sync_and_eq::MicRumbleAndNoiseGate::new();

                // On Linux the pacat writer consumes the FIFO and feeds the
                // AudioShare_Phone_Mic_Sink. On Windows, prefer VB-Cable's
                // "CABLE Input" endpoint; otherwise use the default output.
                #[cfg(not(target_os = "android"))]
                let _mic_out = {
                    #[cfg(target_os = "linux")]
                    {
                        if mode == PhoneRxRoutingMode::VirtualMicForCalls {
                            std::env::set_var("PULSE_SINK", "AudioShare_Phone_Mic_Sink");
                            if linux_output_active {
                                None
                            } else {
                                crate::ClientPreviewSink::spawn_preferred(
                                    Arc::clone(&mic_fifo_thread),
                                    Some("AudioShare_Phone_Mic_Sink"),
                                )
                                .ok()
                            }
                        } else {
                            std::env::remove_var("PULSE_SINK");
                            crate::ClientPreviewSink::spawn(Arc::clone(&mic_fifo_thread)).ok()
                        }
                    }
                    #[cfg(target_os = "windows")]
                    {
                        if mode == PhoneRxRoutingMode::VirtualMicForCalls {
                            crate::ClientPreviewSink::spawn_preferred(
                                Arc::clone(&mic_fifo_thread),
                                Some("CABLE Input"),
                            )
                            .ok()
                        } else {
                            crate::ClientPreviewSink::spawn(Arc::clone(&mic_fifo_thread)).ok()
                        }
                    }
                    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
                    {
                        crate::ClientPreviewSink::spawn(Arc::clone(&mic_fifo_thread)).ok()
                    }
                };

                let mut recv_buf = [0u8; MAX_SAFE_UDP_PAYLOAD];
                let mut initialized = false;
                let mut prebuffer_ready = false;
                let mut next_seq: u16 = 0;
                let mut latest_seq: u16 = 0;
                let mut next_tick = Instant::now() + Duration::from_micros(2500);

                while run_clone.load(Ordering::Relaxed) {
                    while let Ok((len, _)) = socket.recv_from(&mut recv_buf) {
                        if let Ok(parsed) = ParsedPacket::parse(&recv_buf[..len]) {
                            pkts_clone.fetch_add(1, Ordering::Relaxed);
                            let base_seq = parsed.header.sequence;
                            if !initialized {
                                next_seq = base_seq;
                                latest_seq = base_seq;
                                initialized = true;
                            }

                            // The phone transmitter can restart (or the phone
                            // can reconnect) with a new sequence base. Rebase
                            // immediately instead of waiting for the playhead
                            // to walk through hundreds of empty subframes.
                            let seq_delta = base_seq.wrapping_sub(next_seq) as i16;
                            if seq_delta > 48 || seq_delta < -32 {
                                sequencer.reset();
                                next_seq = base_seq;
                                latest_seq = base_seq;
                                prebuffer_ready = false;
                            }

                            if !parsed.fec_n1_payload.is_empty() {
                                let s1 = base_seq.wrapping_sub(1);
                                if (s1.wrapping_sub(next_seq) as i16) >= 0
                                    && sequencer.is_slot_missing(s1)
                                {
                                    let mut f1 = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                                    if decode_fec_subframe_to_f32(parsed.fec_n1_payload, &mut f1) {
                                        sequencer.try_heal_with_fec(s1, &f1);
                                    }
                                }
                            }
                            if !parsed.fec_n2_payload.is_empty() {
                                let s2 = base_seq.wrapping_sub(2);
                                if (s2.wrapping_sub(next_seq) as i16) >= 0
                                    && sequencer.is_slot_missing(s2)
                                {
                                    let mut f2 = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                                    if decode_fec_subframe_to_f32(parsed.fec_n2_payload, &mut f2) {
                                        sequencer.try_heal_with_fec(s2, &f2);
                                    }
                                }
                            }

                            // Decode every bundled subframe, not just index 0.
                            // Party Mode / PhoneAudioCaster sends two
                            // subframes per datagram; reading only the first
                            // one discarded 50% of the audio.
                            decode_primary_bundle(
                                parsed.primary_payload,
                                parsed.header.subframe_count,
                                |sub_idx, pcm_f32| {
                                    let mut max_peak = 0.0f32;
                                    for &v in pcm_f32.iter() {
                                        let a = v.abs();
                                        if a > max_peak {
                                            max_peak = a;
                                        }
                                    }
                                    peak_clone.store(
                                        ((max_peak * 100.0).min(100.0)) as u64,
                                        Ordering::Relaxed,
                                    );
                                    if audioshare_protocol::sync_and_eq::LatencySonar::detect_chirp_return(pcm_f32) {
                                        let now_us = std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_micros() as u64;
                                        chirp_clone.store(now_us, Ordering::Relaxed);
                                    }

                                    let s = base_seq.wrapping_add(sub_idx as u16);
                                    if (s.wrapping_sub(next_seq) as i16) >= 0 {
                                        sequencer.push_primary(s, pcm_f32);
                                    }
                                    if (s.wrapping_sub(latest_seq) as i16) > 0 {
                                        latest_seq = s;
                                    }
                                },
                            );

                            // Wait for a 7.5 ms cushion before starting
                            // playback. This absorbs the first network jitter
                            // burst without adding noticeable call latency.
                            // If a burst pushed the backlog beyond 40 ms, keep
                            // only the newest 10 ms so call latency cannot
                            // ratchet upwards.
                            let buf_depth = buffered_subframes(latest_seq, next_seq);
                            if buf_depth > 16 {
                                next_seq = latest_seq.wrapping_add(1).wrapping_sub(4);
                            }
                            if buf_depth >= 3 {
                                prebuffer_ready = true;
                            }
                        }
                    }

                    let now = Instant::now();
                    while now >= next_tick {
                        next_tick += Duration::from_micros(2500);
                        if initialized && prebuffer_ready {
                            let mut frame = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                            let status = sequencer.pop_next(next_seq, &mut frame);
                            if should_advance_mic_playhead(status, latest_seq, next_seq) {
                                next_seq = next_seq.wrapping_add(1);
                            }
                            // The rumble/noise gate is a voice-call processor.
                            // Music cast to PC speakers must keep its sub-bass
                            // and quiet passages.
                            if mode == PhoneRxRoutingMode::VirtualMicForCalls {
                                mic_gate.process_in_place(&mut frame);
                            }
                            limiter.process_in_place(&mut frame);
                            mic_fifo_thread.push_samples(&frame);
                        }
                    }

                    std::thread::sleep(Duration::from_micros(600));
                }
            })
            .map_err(|e| format!("Failed to spawn PhoneMicReceiver thread: {}", e))?;

        Ok(Self {
            running,
            mic_packets_received,
            mic_peak_level_pct,
            last_chirp_rx_timestamp_us,
            #[cfg(target_os = "linux")]
            pactl_mic_module_id,
            #[cfg(target_os = "linux")]
            pactl_mic_source_module_id,
            #[cfg(target_os = "linux")]
            linux_output,
            handle: Some(handle),
        })
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        #[cfg(target_os = "linux")]
        {
            // Stop pacat first, then remove the remap source and sink.
            drop(self.linux_output.take());
            if let Some(id) = self.pactl_mic_source_module_id.take() {
                let _ = Command::new("pactl").args(["unload-module", &id]).output();
            }
            if let Some(id) = self.pactl_mic_module_id.take() {
                let _ = Command::new("pactl").args(["unload-module", &id]).output();
            }
        }
    }
}

impl Drop for PhoneMicReceiver {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audioshare_protocol::lpc_lossless::{
        encode_lossless_subframe, MAX_LOSSLESS_SUBFRAME_BYTES,
    };

    #[test]
    fn test_decode_primary_bundle_returns_every_bundled_subframe() {
        let mut first = [0i16; SUBFRAME_STEREO_SAMPLES];
        let mut second = [0i16; SUBFRAME_STEREO_SAMPLES];
        for i in 0..SUBFRAME_STEREO_SAMPLES {
            first[i] = ((i as i32 * 97) % 30_000 - 15_000) as i16;
            second[i] = ((i as i32 * 53) % 20_000 - 10_000) as i16;
        }

        let mut enc1 = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
        let mut enc2 = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
        let len1 = encode_lossless_subframe(&first, &mut enc1);
        let len2 = encode_lossless_subframe(&second, &mut enc2);

        let mut bundle = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES * 2 + 4];
        let mut cursor = 0usize;
        for (enc, len) in [(&enc1, len1), (&enc2, len2)] {
            bundle[cursor..cursor + 2].copy_from_slice(&(len as u16).to_be_bytes());
            cursor += 2;
            bundle[cursor..cursor + len].copy_from_slice(&enc[..len]);
            cursor += len;
        }

        let mut decoded = Vec::new();
        let count = decode_primary_bundle(&bundle[..cursor], 2, |idx, pcm| {
            let mut samples = [0i16; SUBFRAME_STEREO_SAMPLES];
            for (dst, &s) in samples.iter_mut().zip(pcm.iter()) {
                *dst = (s * 32768.0).round() as i16;
            }
            decoded.push((idx, samples));
        });

        assert_eq!(count, 2);
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].0, 0);
        assert_eq!(decoded[1].0, 1);
        assert_eq!(decoded[0].1, first);
        assert_eq!(decoded[1].1, second);
    }

    #[test]
    fn test_decode_primary_bundle_rejects_truncated_length_prefix() {
        let mut short = [0u8; 4];
        short[0..2].copy_from_slice(&100u16.to_be_bytes());
        let count = decode_primary_bundle(&short, 1, |_, _| panic!("must not decode"));
        assert_eq!(count, 0);
    }

    #[test]
    fn test_phone_mic_playhead_holds_when_pc_clock_runs_ahead() {
        use audioshare_protocol::sequencer::{
            STATUS_EMPTY, STATUS_HEALED_BY_FEC, STATUS_PRIMARY_LOSSLESS,
        };

        // The PC tick found an empty slot and no newer packet: hold so the
        // phone can catch up instead of running the playhead ahead forever.
        assert!(!should_advance_mic_playhead(STATUS_EMPTY, 99, 100));
        // A newer packet is already buffered: skip the hole.
        assert!(should_advance_mic_playhead(STATUS_EMPTY, 100, 100));
        // Real and FEC audio always advance.
        assert!(should_advance_mic_playhead(
            STATUS_PRIMARY_LOSSLESS,
            50,
            100
        ));
        assert!(should_advance_mic_playhead(STATUS_HEALED_BY_FEC, 50, 100));
        // 16-bit wrap: latest 65535 with next 0 means next is one ahead.
        assert!(!should_advance_mic_playhead(STATUS_EMPTY, 65535, 0));
    }

    #[test]
    fn test_buffered_subframes_handles_wraparound() {
        assert_eq!(buffered_subframes(0, 0), 1);
        assert_eq!(buffered_subframes(2, 0), 3);
        assert_eq!(buffered_subframes(0, 1), 0);
        assert_eq!(buffered_subframes(65535, 0), 0);
        assert_eq!(buffered_subframes(0, 65535), 2);
    }
}
