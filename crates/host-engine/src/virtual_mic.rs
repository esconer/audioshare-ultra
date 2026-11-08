use std::net::UdpSocket;
#[cfg(target_os = "linux")]
use std::io::Write;
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
        let socket = UdpSocket::bind(("0.0.0.0", port))
            .map_err(|e| format!("Failed to bind Phone Mic UDP port {}: {}", port, e))?;
        configure_realtime_udp_socket(&socket)
            .map_err(|e| format!("Failed to configure Phone Mic socket: {}", e))?;
        let _ = socket.set_nonblocking(true);

        #[cfg(target_os = "linux")]
        let pactl_mic_module_id = {
            // Create the sink that pacat feeds.
            Command::new("pactl")
                .args([
                    "load-module",
                    "module-null-sink",
                    "sink_name=AudioShare_Phone_Mic_Sink",
                    "sink_properties=device.description=AudioShare_Phone_Microphone",
                ])
                .output()
                .ok()
                .and_then(|o| {
                    if o.status.success() {
                        Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                    } else {
                        None
                    }
                })
        };

        #[cfg(target_os = "linux")]
        let pactl_mic_source_module_id = {
            // Expose the sink's monitor as a normal recording device.
            Command::new("pactl")
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
                })
        };

        let running = Arc::new(AtomicBool::new(true));
        let mic_packets_received = Arc::new(AtomicU64::new(0));
        let mic_peak_level_pct = Arc::new(AtomicU64::new(0));
        let last_chirp_rx_timestamp_us = Arc::new(AtomicU64::new(0));

        let mic_fifo = Arc::new(BurstDecouplerRing::new());

        #[cfg(target_os = "linux")]
        let linux_output = LinuxVirtualMicOutput::start(
            Arc::clone(&mic_fifo),
            Arc::clone(&running),
        );
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
                let mut limiter = TruePeakLimiter::new(6.0); // +6 dB clean mic pre-amp
                let mut mic_gate = audioshare_protocol::sync_and_eq::MicRumbleAndNoiseGate::new();

                // On Linux the pacat writer consumes the FIFO and feeds the
                // AudioShare_Phone_Mic_Sink. On Windows, prefer VB-Cable's
                // "CABLE Input" endpoint; otherwise use the default output.
                #[cfg(not(target_os = "android"))]
                let _mic_out = {
                    #[cfg(target_os = "linux")]
                    {
                        if linux_output_active {
                            None
                        } else {
                            crate::ClientPreviewSink::spawn(Arc::clone(&mic_fifo_thread)).ok()
                        }
                    }
                    #[cfg(target_os = "windows")]
                    {
                        crate::ClientPreviewSink::spawn_preferred(
                            Arc::clone(&mic_fifo_thread),
                            Some("CABLE Input"),
                        )
                        .ok()
                    }
                    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
                    {
                        crate::ClientPreviewSink::spawn(Arc::clone(&mic_fifo_thread)).ok()
                    }
                };

                let mut recv_buf = [0u8; MAX_SAFE_UDP_PAYLOAD];
                let mut initialized = false;
                let mut next_seq: u16 = 0;
                let mut next_tick = Instant::now() + Duration::from_micros(2500);

                while run_clone.load(Ordering::Relaxed) {
                    while let Ok((len, _)) = socket.recv_from(&mut recv_buf) {
                        if let Ok(parsed) = ParsedPacket::parse(&recv_buf[..len]) {
                            pkts_clone.fetch_add(1, Ordering::Relaxed);
                            let base_seq = parsed.header.sequence;
                            if !initialized {
                                next_seq = base_seq;
                                initialized = true;
                            }

                            if !parsed.fec_n1_payload.is_empty() {
                                let mut f1 = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                                if decode_fec_subframe_to_f32(parsed.fec_n1_payload, &mut f1) {
                                    sequencer.try_heal_with_fec(base_seq.wrapping_sub(1), &f1);
                                }
                            }

                            let prim = parsed.primary_payload;
                            if prim.len() >= 2 {
                                let sub_len = u16::from_be_bytes([prim[0], prim[1]]) as usize;
                                if prim.len() >= 2 + sub_len {
                                    let mut pcm_i16 = [0i16; SUBFRAME_STEREO_SAMPLES];
                                    if decode_lossless_subframe(
                                        &prim[2..2 + sub_len],
                                        &mut pcm_i16,
                                    )
                                    .is_ok()
                                    {
                                        let mut pcm_f32 = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                                        let mut max_peak = 0.0f32;
                                        for (dst, &s) in pcm_f32.iter_mut().zip(pcm_i16.iter()) {
                                            let v = (s as f32) * (1.0 / 32768.0);
                                            *dst = v;
                                            if v.abs() > max_peak {
                                                max_peak = v.abs();
                                            }
                                        }
                                        peak_clone.store(
                                            ((max_peak * 100.0).min(100.0)) as u64,
                                            Ordering::Relaxed,
                                        );
                                        if audioshare_protocol::sync_and_eq::LatencySonar::detect_chirp_return(&pcm_f32) {
                                            let now_us = std::time::SystemTime::now()
                                                .duration_since(std::time::UNIX_EPOCH)
                                                .unwrap_or_default()
                                                .as_micros() as u64;
                                            chirp_clone.store(now_us, Ordering::Relaxed);
                                        }
                                        sequencer.push_primary(base_seq, &pcm_f32);
                                    }
                                }
                            }
                        }
                    }

                    let now = Instant::now();
                    while now >= next_tick {
                        next_tick += Duration::from_micros(2500);
                        if initialized {
                            let mut frame = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                            sequencer.pop_next(next_seq, &mut frame);
                            next_seq = next_seq.wrapping_add(1);
                            mic_gate.process_in_place(&mut frame);
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
                let _ = Command::new("pactl")
                    .args(["unload-module", &id])
                    .output();
            }
            if let Some(id) = self.pactl_mic_module_id.take() {
                let _ = Command::new("pactl")
                    .args(["unload-module", &id])
                    .output();
            }
        }
    }
}

impl Drop for PhoneMicReceiver {
    fn drop(&mut self) {
        self.stop();
    }
}
