#[cfg(target_os = "linux")]
use std::io::Write;
use std::net::UdpSocket;
#[cfg(target_os = "linux")]
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use audioshare_protocol::dsp::{BurstDecouplerRing, ClockDriftResampler, TruePeakLimiter};
use audioshare_protocol::lpc_lossless::{
    decode_fec_subframe_to_f32, decode_lossless_pcm24_to_f32, decode_lossless_subframe,
    decode_vbr_subframe_to_f32, TAG_VBR_BFP,
};
use audioshare_protocol::opus_codec::{OpusStreamDecoder, TAG_OPUS_RLD};
use audioshare_protocol::sequencer::ZeroJitterSequencer;
use audioshare_protocol::wire::{CodecMode, ParsedPacket};
use audioshare_protocol::{MAX_SAFE_UDP_PAYLOAD, SUBFRAME_FRAMES, SUBFRAME_STEREO_SAMPLES};

use crate::{configure_realtime_udp_socket, promote_thread_realtime};

pub const PHONE_MIC_PORT: u16 = 65532;

/// Minimum decoupler cushion (6 subframes = 15 ms); scales up automatically if
/// the OS output callback period is larger (e.g. 1024 frames = 8.5 subframes).
const MIC_FIFO_MIN_TARGET_SUBFRAMES: f32 = 6.0;
/// Upper bound so a pathologically large callback cannot add unbounded call
/// latency. 20 subframes = 50 ms.
const MIC_FIFO_MAX_TARGET_SUBFRAMES: f32 = 20.0;

/// Compute the decoupler cushion from the native output callback size,
/// expressed in 48 kHz-equivalent frames.
///
/// Covering one full OS callback plus 3 subframes (7.5 ms) of network jitter
/// margin keeps the FIFO from underrunning on the next pull. The 240-frame
/// (5 ms) low-latency callback stays at the 15 ms floor; 1024/2048-frame
/// desktop callbacks grow to ~11.5/20 subframes instead of draining dry.
#[inline]
fn mic_fifo_target_subframes(observed_cb_frames: usize) -> f32 {
    let cb_subframes = observed_cb_frames as f32 / SUBFRAME_FRAMES as f32;
    (cb_subframes + 3.0).clamp(MIC_FIFO_MIN_TARGET_SUBFRAMES, MIC_FIFO_MAX_TARGET_SUBFRAMES)
}

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

/// True when the first length-prefixed subframe in a primary bundle is Opus.
///
/// ADPCM FEC must never heal an Opus stream: Opus carries its own 2.5 ms
/// lookahead/PLC state, so splicing a zero-lookahead ADPCM frame into the
/// stream would jump the waveform by one subframe.
#[inline]
fn primary_bundle_is_opus(primary: &[u8]) -> bool {
    primary.get(2) == Some(&TAG_OPUS_RLD)
}

/// Decode every length-prefixed subframe in a primary bundle.
///
/// The phone transmitter may bundle up to four 2.5 ms subframes per datagram.
/// Each decoded subframe is reported with its index so the caller can map it
/// to `base_seq + index`; reading only index 0 drops half of the audio in the
/// two-subframe Party Mode / PhoneAudioCaster path.
///
/// `base_seq` and `opus_decoder` are needed for the stateful Opus backend:
/// Opus must see frames in stream order, so the tagged payload is decoded with
/// `decode_subframe_with_plc`. The other codecs are self-identifying and stay
/// stateless.
///
/// `on_plc` receives every CELT concealment frame generated for a sequence gap,
/// phase-aligned with the Opus lookahead, so the caller can heal empty
/// sequencer slots. It must run before `on_subframe` for the frame that
/// exposed the gap; the packet's own audio is reported afterwards.
fn decode_primary_bundle(
    primary: &[u8],
    subframe_count: u8,
    base_seq: u16,
    mut opus_decoder: Option<&mut Option<OpusStreamDecoder>>,
    mut on_plc: impl FnMut(u16, &[f32; SUBFRAME_STEREO_SAMPLES]),
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

        let payload = &primary[cursor..cursor + sub_len];
        let mut pcm_f32 = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        let seq = base_seq.wrapping_add(sub_idx as u16);
        let ok = if payload.first() == Some(&TAG_OPUS_RLD) {
            // Opus is stateful, so a decoder must be supplied. Without one,
            // treat the frame as undecodable instead of guessing a codec.
            if let Some(decoder_slot) = opus_decoder.as_deref_mut() {
                if decoder_slot.is_none() {
                    *decoder_slot = OpusStreamDecoder::new();
                }
                decoder_slot
                    .as_mut()
                    .map(|decoder| {
                        decoder.decode_subframe_with_plc(seq, payload, &mut pcm_f32, &mut on_plc)
                    })
                    .unwrap_or(false)
            } else {
                false
            }
        } else {
            // A non-Opus payload means the host switched back to VBR or
            // lossless. Drop the Opus decoder so a later switch starts with
            // clean MDCT/band-energy state instead of stale history.
            if let Some(decoder_slot) = opus_decoder.as_deref_mut() {
                *decoder_slot = None;
            }
            if payload.first() == Some(&TAG_VBR_BFP) {
                // The Party host switches between lossless and VBR without
                // changing the header's mode. A VBR subframe is
                // self-identifying through its tag, so sniff every payload
                // instead of trusting one codec mode for the whole stream.
                decode_vbr_subframe_to_f32(payload, &mut pcm_f32)
            } else if decode_lossless_pcm24_to_f32(payload, &mut pcm_f32) {
                true
            } else {
                let mut pcm_i16 = [0i16; SUBFRAME_STEREO_SAMPLES];
                if decode_lossless_subframe(payload, &mut pcm_i16).is_ok() {
                    for (dst, &s) in pcm_f32.iter_mut().zip(pcm_i16.iter()) {
                        *dst = (s as f32) * (1.0 / 32768.0);
                    }
                    true
                } else {
                    false
                }
            }
        };
        if ok {
            on_subframe(sub_idx, &pcm_f32);
            decoded += 1;
        }
        cursor += sub_len;
    }
    decoded
}

#[allow(clippy::too_many_arguments)]
fn push_one_subframe(
    next_seq: &mut u16,
    latest_seq: u16,
    mode: PhoneRxRoutingMode,
    sequencer: &mut ZeroJitterSequencer,
    mic_gate: &mut audioshare_protocol::sync_and_eq::MicRumbleAndNoiseGate,
    limiter: &mut TruePeakLimiter,
    resampler: &mut ClockDriftResampler,
    resampled_out: &mut [f32],
    mic_fifo: &BurstDecouplerRing,
) {
    let mut frame = [0.0f32; SUBFRAME_STEREO_SAMPLES];
    let status = sequencer.pop_next(*next_seq, &mut frame);
    if should_advance_mic_playhead(status, latest_seq, *next_seq) {
        *next_seq = next_seq.wrapping_add(1);
    }
    if mode == PhoneRxRoutingMode::VirtualMicForCalls {
        mic_gate.process_in_place(&mut frame);
    }
    limiter.process_in_place(&mut frame);

    let frames = resampler.process(&frame, resampled_out);
    if frames > 0 {
        mic_fifo.push_samples(&resampled_out[..frames * 2]);
    }
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
        // Default to the low-latency 240-frame callback the sink asks for,
        // expressed in 48 kHz-equivalent frames. `ClientPreviewSink`
        // overwrites this with the backend's real callback duration as soon
        // as the first pull happens.
        let observed_cb_frames = Arc::new(AtomicUsize::new(240));

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
                                crate::ClientPreviewSink::spawn_preferred_with_cb_frames(
                                    Arc::clone(&mic_fifo_thread),
                                    Some("AudioShare_Phone_Mic_Sink"),
                                    Some(Arc::clone(&observed_cb_frames)),
                                )
                                .ok()
                            }
                        } else {
                            std::env::remove_var("PULSE_SINK");
                            crate::ClientPreviewSink::spawn_preferred_with_cb_frames(
                                Arc::clone(&mic_fifo_thread),
                                None,
                                Some(Arc::clone(&observed_cb_frames)),
                            )
                            .ok()
                        }
                    }
                    #[cfg(target_os = "windows")]
                    {
                        if mode == PhoneRxRoutingMode::VirtualMicForCalls {
                            crate::ClientPreviewSink::spawn_preferred_with_cb_frames(
                                Arc::clone(&mic_fifo_thread),
                                Some("CABLE Input"),
                                Some(Arc::clone(&observed_cb_frames)),
                            )
                            .ok()
                        } else {
                            crate::ClientPreviewSink::spawn_preferred_with_cb_frames(
                                Arc::clone(&mic_fifo_thread),
                                None,
                                Some(Arc::clone(&observed_cb_frames)),
                            )
                            .ok()
                        }
                    }
                    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
                    {
                        crate::ClientPreviewSink::spawn_preferred_with_cb_frames(
                            Arc::clone(&mic_fifo_thread),
                            None,
                            Some(Arc::clone(&observed_cb_frames)),
                        )
                        .ok()
                    }
                };

                #[cfg(not(target_os = "android"))]
                let pull_sink_active = _mic_out.is_some();
                #[cfg(target_os = "android")]
                let pull_sink_active = false;

                let mut recv_buf = [0u8; MAX_SAFE_UDP_PAYLOAD];
                let mut initialized = false;
                let mut prebuffer_ready = false;
                let mut dtx_silent = false;
                let mut next_seq: u16 = 0;
                let mut latest_seq: u16 = 0;
                let mut next_tick = Instant::now() + Duration::from_micros(2500);
                let mut resampler = ClockDriftResampler::new();
                let mut opus_decoder: Option<OpusStreamDecoder> = None;
                let mut resampled_out = [0.0f32; SUBFRAME_STEREO_SAMPLES + 16];

                while run_clone.load(Ordering::Relaxed) {
                    // Size the decoupler target to cover at least one full OS
                    // callback period plus 3 subframes (7.5 ms) of network
                    // jitter margin. The sink reports the backend's real
                    // callback size, so a 1024/2048-frame desktop callback
                    // automatically gets a deeper cushion than 5 ms AAudio.
                    let cb_frames = observed_cb_frames.load(Ordering::Relaxed);
                    let target_subframes = mic_fifo_target_subframes(cb_frames);
                    let target_depth = target_subframes.ceil() as i16;
                    let fifo_watermark = resampler
                        .subframes_to_samples(target_subframes.ceil() as usize)
                        .min(mic_fifo_thread.capacity_samples());

                    while let Ok((len, _)) = socket.recv_from(&mut recv_buf) {
                        if let Ok(parsed) = ParsedPacket::parse(&recv_buf[..len]) {
                            pkts_clone.fetch_add(1, Ordering::Relaxed);
                            let base_seq = parsed.header.sequence;
                            if !initialized {
                                next_seq = base_seq;
                                latest_seq = base_seq;
                                initialized = true;
                            }

                            // DTX heartbeat: the host stopped sending audio
                            // but kept the NAT mapping and clock alive. Drop
                            // every held slot and park the playhead here, so
                            // the next real packet starts a fresh cushion
                            // instead of replaying stale concealment.
                            if parsed.header.mode == CodecMode::SilenceHeartbeat {
                                sequencer.reset();
                                opus_decoder = None;
                                next_seq = base_seq;
                                latest_seq = base_seq;
                                prebuffer_ready = false;
                                dtx_silent = true;
                                continue;
                            }

                            // The phone transmitter can restart (or the phone
                            // can reconnect) with a new sequence base. Rebase
                            // immediately instead of waiting for the playhead
                            // to walk through hundreds of empty subframes.
                            let seq_delta = base_seq.wrapping_sub(next_seq) as i16;
                            if dtx_silent || seq_delta > 48 || seq_delta < -32 {
                                dtx_silent = false;
                                sequencer.reset();
                                opus_decoder = None;
                                next_seq = base_seq;
                                latest_seq = base_seq;
                                prebuffer_ready = false;
                            }

                            // Opus carries its own lookahead/PLC state; an
                            // ADPCM FEC backup is phase-shifted by one
                            // subframe, so only heal VBR/lossless streams.
                            if !primary_bundle_is_opus(parsed.primary_payload) {
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
                            }

                            // Decode every bundled subframe, not just index 0.
                            // Party Mode / PhoneAudioCaster sends two
                            // subframes per datagram; reading only the first
                            // one discarded 50% of the audio.
                            decode_primary_bundle(
                                parsed.primary_payload,
                                parsed.header.subframe_count,
                                base_seq,
                                Some(&mut opus_decoder),
                                |missed_seq, plc_pcm| {
                                    // Opus PLC is phase-aligned with the
                                    // lookahead, so it can fill an empty slot
                                    // directly. A late primary copy still wins
                                    // because it arrives as a normal push.
                                    if (missed_seq.wrapping_sub(next_seq) as i16) >= 0
                                        && sequencer.is_slot_missing(missed_seq)
                                    {
                                        sequencer.try_heal_with_fec(missed_seq, plc_pcm);
                                    }
                                },
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

                            // Keep only the planned cushion (one callback plus
                            // jitter margin) if a burst pushed the backlog far
                            // past it, then start playback once that cushion is
                            // actually buffered.
                            let buf_depth = buffered_subframes(latest_seq, next_seq);
                            let max_depth = target_depth + 8;
                            if buf_depth > max_depth {
                                next_seq = latest_seq
                                    .wrapping_add(1)
                                    .wrapping_sub(target_depth as u16);
                            }
                            if buf_depth >= target_depth {
                                prebuffer_ready = true;
                            }
                        }
                    }

                    let now = Instant::now();

                    if initialized && prebuffer_ready && pull_sink_active {
                        // When a hardware-clocked CPAL sink is pulling bursts
                        // from `mic_fifo_thread`, immediately transfer ready
                        // subframes from `sequencer` so the decoupler holds the
                        // full `fifo_watermark` cushion before the next callback.
                        while mic_fifo_thread.available_samples() < fifo_watermark
                            && !sequencer.is_slot_missing(next_seq)
                        {
                            push_one_subframe(
                                &mut next_seq,
                                latest_seq,
                                mode,
                                &mut sequencer,
                                &mut mic_gate,
                                &mut limiter,
                                &mut resampler,
                                &mut resampled_out,
                                &mic_fifo_thread,
                            );
                            let planned_cushion =
                                buffered_subframes(latest_seq, next_seq) as f32 + target_subframes;
                            resampler.update_clock_loop(planned_cushion, target_subframes);
                            next_tick = now + Duration::from_micros(2500);
                        }
                    }

                    while now >= next_tick {
                        next_tick += Duration::from_micros(2500);
                        if initialized && prebuffer_ready {
                            if mic_fifo_thread.available_samples() >= fifo_watermark {
                                break;
                            }
                            push_one_subframe(
                                &mut next_seq,
                                latest_seq,
                                mode,
                                &mut sequencer,
                                &mut mic_gate,
                                &mut limiter,
                                &mut resampler,
                                &mut resampled_out,
                                &mic_fifo_thread,
                            );
                            let planned_cushion = if pull_sink_active {
                                buffered_subframes(latest_seq, next_seq) as f32 + target_subframes
                            } else {
                                buffered_subframes(latest_seq, next_seq) as f32
                                    + resampler.samples_to_subframes(
                                        mic_fifo_thread.available_samples(),
                                    )
                            };
                            resampler.update_clock_loop(planned_cushion, target_subframes);
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
        encode_lossless_pcm24_subframe, encode_lossless_subframe, encode_vbr_subframe,
        MAX_LOSSLESS_PCM24_BYTES, MAX_LOSSLESS_SUBFRAME_BYTES, TAG_VBR_BFP,
    };
    use audioshare_protocol::opus_codec::{OpusStreamEncoder, MAX_OPUS_SUBFRAME_BYTES};
    use audioshare_protocol::SAMPLE_RATE;

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
        let count = decode_primary_bundle(
            &bundle[..cursor],
            2,
            0,
            None,
            |_, _| {},
            |idx, pcm| {
                let mut samples = [0i16; SUBFRAME_STEREO_SAMPLES];
                for (dst, &s) in samples.iter_mut().zip(pcm.iter()) {
                    *dst = (s * 32768.0).round() as i16;
                }
                decoded.push((idx, samples));
            },
        );

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
        let count = decode_primary_bundle(
            &short,
            1,
            0,
            None,
            |_, _| {},
            |_, _| panic!("must not decode"),
        );
        assert_eq!(count, 0);
    }

    #[test]
    fn test_primary_bundle_is_opus_reads_first_length_prefixed_tag() {
        let mut opus = [0u8; 3];
        opus[0..2].copy_from_slice(&1u16.to_be_bytes());
        opus[2] = TAG_OPUS_RLD;
        assert!(primary_bundle_is_opus(&opus));

        let vbr = [0u8, 1, TAG_VBR_BFP];
        assert!(!primary_bundle_is_opus(&vbr));
        assert!(!primary_bundle_is_opus(&[0, 1]));
        assert!(!primary_bundle_is_opus(&[]));
    }

    #[test]
    fn test_decode_primary_bundle_resets_opus_decoder_on_non_opus() {
        let mut pcm_i16 = [0i16; SUBFRAME_STEREO_SAMPLES];
        for i in 0..SUBFRAME_FRAMES {
            let t = i as f32 / SAMPLE_RATE as f32;
            pcm_i16[i * 2] = ((t * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
            pcm_i16[i * 2 + 1] = ((t * 554.37 * std::f32::consts::TAU).sin() * 8000.0) as i16;
        }

        let mut enc = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
        let len = encode_vbr_subframe(&pcm_i16, 256, &mut enc);
        let mut bundle = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES + 2];
        bundle[0..2].copy_from_slice(&(len as u16).to_be_bytes());
        bundle[2..2 + len].copy_from_slice(&enc[..len]);

        let mut decoder = Some(OpusStreamDecoder::new().expect("create Opus decoder"));
        let count = decode_primary_bundle(
            &bundle[..2 + len],
            1,
            0,
            Some(&mut decoder),
            |_, _| {},
            |_, _| {},
        );
        assert_eq!(count, 1, "VBR payload must still decode");
        assert!(
            decoder.is_none(),
            "a non-Opus payload must drop stale Opus state"
        );
    }

    #[test]
    fn test_decode_primary_bundle_handles_tagged_vbr() {
        let mut pcm_i16 = [0i16; SUBFRAME_STEREO_SAMPLES];
        for i in 0..(SUBFRAME_STEREO_SAMPLES / 2) {
            let t = i as f32 / 48_000.0;
            pcm_i16[i * 2] = ((t * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
            pcm_i16[i * 2 + 1] = ((t * 554.37 * std::f32::consts::TAU).sin() * 8000.0) as i16;
        }

        let mut enc = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
        let len = encode_vbr_subframe(&pcm_i16, 256, &mut enc);
        assert_eq!(enc[0], TAG_VBR_BFP);

        let mut bundle = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES + 2];
        bundle[0..2].copy_from_slice(&(len as u16).to_be_bytes());
        bundle[2..2 + len].copy_from_slice(&enc[..len]);

        let mut peak = 0.0f32;
        let count = decode_primary_bundle(
            &bundle[..2 + len],
            1,
            0,
            None,
            |_, _| {},
            |idx, pcm| {
                assert_eq!(idx, 0);
                for &v in pcm {
                    peak = peak.max(v.abs());
                }
            },
        );
        assert_eq!(count, 1, "tagged VBR payload must decode");
        assert!(peak > 0.05, "decoded music must not be silence");
    }

    #[test]
    fn test_decode_primary_bundle_handles_24bit_payload() {
        let mut pcm_f32 = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        for (i, sample) in pcm_f32.iter_mut().enumerate() {
            let t = i as f32 / SAMPLE_RATE as f32;
            // The tiny linear term guarantees a non-zero 8-bit LSB plane so
            // the encoder emits the true 24-bit extension instead of falling
            // back to the 16-bit payload.
            *sample = (t * 880.0 * std::f32::consts::TAU).sin() * 0.4 + (i as f32) * 0.000_000_1;
        }

        let mut enc = [0u8; MAX_LOSSLESS_PCM24_BYTES];
        let len = encode_lossless_pcm24_subframe(&pcm_f32, &mut enc);
        assert!(
            len > MAX_LOSSLESS_SUBFRAME_BYTES,
            "payload must carry the 24-bit LSB plane, len={len}"
        );

        let mut bundle = [0u8; MAX_LOSSLESS_PCM24_BYTES + 2];
        bundle[0..2].copy_from_slice(&(len as u16).to_be_bytes());
        bundle[2..2 + len].copy_from_slice(&enc[..len]);

        let mut decoded = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        let count = decode_primary_bundle(
            &bundle[..2 + len],
            1,
            42,
            None,
            |_, _| {},
            |idx, pcm| {
                assert_eq!(idx, 0);
                decoded = *pcm;
            },
        );
        assert_eq!(count, 1, "24-bit payload must decode");
        for (i, (&got, &source)) in decoded.iter().zip(pcm_f32.iter()).enumerate() {
            let q = (source.clamp(-1.0, 1.0) * 8_388_608.0)
                .round()
                .clamp(-8_388_608.0, 8_388_607.0);
            let expected = q / 8_388_608.0;
            assert!(
                (got - expected).abs() < 1e-6,
                "sample {i}: decoded {got} != expected {expected}"
            );
        }
    }

    #[test]
    fn test_decode_primary_bundle_handles_opus_payload() {
        fn tone(frame_index: usize) -> [i16; SUBFRAME_STEREO_SAMPLES] {
            let mut pcm = [0i16; SUBFRAME_STEREO_SAMPLES];
            for i in 0..SUBFRAME_FRAMES {
                let t = ((frame_index * SUBFRAME_FRAMES + i) as f32) / (SAMPLE_RATE as f32);
                let l = (t * 440.0 * std::f32::consts::TAU).sin() * 0.25;
                let r = (t * 554.37 * std::f32::consts::TAU).sin() * 0.25;
                pcm[i * 2] = (l * 32767.0) as i16;
                pcm[i * 2 + 1] = (r * 32767.0) as i16;
            }
            pcm
        }

        let mut encoder = OpusStreamEncoder::new(128).expect("create Opus encoder");
        let mut payloads = [[0u8; MAX_OPUS_SUBFRAME_BYTES]; 2];
        let mut lens = [0usize; 2];
        for (frame_index, (payload, len)) in payloads.iter_mut().zip(lens.iter_mut()).enumerate() {
            *len = encoder
                .encode_subframe(&tone(frame_index), 128, payload)
                .expect("encode Opus subframe");
            assert_eq!(payload[0], TAG_OPUS_RLD);
        }

        let mut bundle = [0u8; (MAX_OPUS_SUBFRAME_BYTES + 2) * 2];
        let mut cursor = 0usize;
        for (payload, &len) in payloads.iter().zip(lens.iter()) {
            bundle[cursor..cursor + 2].copy_from_slice(&(len as u16).to_be_bytes());
            cursor += 2;
            bundle[cursor..cursor + len].copy_from_slice(&payload[..len]);
            cursor += len;
        }

        let mut decoder = Some(OpusStreamDecoder::new().expect("create Opus decoder"));
        let mut frames = Vec::new();
        let count = decode_primary_bundle(
            &bundle[..cursor],
            2,
            100,
            Some(&mut decoder),
            |_, _| {},
            |idx, pcm| frames.push((idx, *pcm)),
        );
        assert_eq!(
            count, 2,
            "both Opus subframes must decode through the bundle"
        );
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].0, 0);
        assert_eq!(frames[1].0, 1);
        // Restricted-low-delay Opus has one 2.5 ms frame of lookahead, so the
        // second decoded frame reconstructs the first tone instead of silence.
        let peak = frames[1].1.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(
            peak > 0.02,
            "second Opus frame must carry real audio, peak={peak}"
        );
    }

    #[test]
    fn test_decode_primary_bundle_delivers_opus_plc_for_missing_slot() {
        fn tone(frame_index: usize) -> [i16; SUBFRAME_STEREO_SAMPLES] {
            let mut pcm = [0i16; SUBFRAME_STEREO_SAMPLES];
            for i in 0..SUBFRAME_FRAMES {
                let t = ((frame_index * SUBFRAME_FRAMES + i) as f32) / (SAMPLE_RATE as f32);
                let l = (t * 440.0 * std::f32::consts::TAU).sin() * 0.25;
                let r = (t * 554.37 * std::f32::consts::TAU).sin() * 0.25;
                pcm[i * 2] = (l * 32767.0) as i16;
                pcm[i * 2 + 1] = (r * 32767.0) as i16;
            }
            pcm
        }

        // Materialize frames 99, 100 and 101 so the decoder has real history
        // and a real future frame around the lost slot.
        let mut encoder = OpusStreamEncoder::new(128).expect("create Opus encoder");
        let mut payloads = [[0u8; MAX_OPUS_SUBFRAME_BYTES]; 3];
        let mut lens = [0usize; 3];
        for frame_index in 0..3 {
            lens[frame_index] = encoder
                .encode_subframe(&tone(frame_index), 128, &mut payloads[frame_index])
                .expect("encode Opus subframe");
        }

        let mut decoder = Some(OpusStreamDecoder::new().expect("create Opus decoder"));
        let mut primed = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        assert!(decoder.as_mut().unwrap().decode_subframe_at_seq(
            99,
            &payloads[0][..lens[0]],
            &mut primed
        ));

        // Packet 100 was lost; packet 101 arrives with the next frame.
        let mut bundle = [0u8; MAX_OPUS_SUBFRAME_BYTES + 2];
        bundle[0..2].copy_from_slice(&(lens[2] as u16).to_be_bytes());
        bundle[2..2 + lens[2]].copy_from_slice(&payloads[2][..lens[2]]);

        let mut plc = Vec::new();
        let mut decoded = Vec::new();
        let count = decode_primary_bundle(
            &bundle[..2 + lens[2]],
            1,
            101,
            Some(&mut decoder),
            |missed_seq, pcm| plc.push((missed_seq, *pcm)),
            |idx, pcm| decoded.push((idx, *pcm)),
        );

        assert_eq!(count, 1);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].0, 0);
        assert_eq!(plc.len(), 1, "one lost slot must produce one PLC frame");
        assert_eq!(plc[0].0, 100, "PLC must land on the missing sequence");
        let plc_peak = plc[0].1.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(plc[0].1.iter().all(|sample| sample.is_finite()));
        assert!(
            plc_peak > 0.001,
            "PLC frame must not be silent, peak={plc_peak}"
        );
    }

    #[test]
    fn test_mic_clock_resampler_depth_conversion() {
        let resampler = ClockDriftResampler::new();
        let one = resampler.samples_to_subframes(SUBFRAME_STEREO_SAMPLES);
        assert!((one - 1.0).abs() < 1e-6);
        let target_subframes = mic_fifo_target_subframes(240);
        assert!((target_subframes - MIC_FIFO_MIN_TARGET_SUBFRAMES).abs() < 1e-6);
        let target_samples = SUBFRAME_STEREO_SAMPLES * target_subframes as usize;
        let target = resampler.samples_to_subframes(target_samples);
        assert!((target - target_subframes).abs() < 1e-6);
        // A 10 ms default desktop callback drains four subframes in one pull;
        // the cushion must cover at least that plus the poll interval.
        assert!(
            target_subframes >= 6.0,
            "mic FIFO target must absorb a 10-15 ms callback burst"
        );
    }

    #[test]
    fn test_mic_fifo_target_scales_with_callback_size() {
        // 5 ms low-latency callback: 2 subframes + 3 margin = 5, floored at 6.
        assert!((mic_fifo_target_subframes(240) - 6.0).abs() < 1e-6);
        // 1024 frames = 8.53 subframes -> 11.53 subframes of cushion.
        let expected_1024 = 1024.0 / SUBFRAME_FRAMES as f32 + 3.0;
        assert!((mic_fifo_target_subframes(1024) - expected_1024).abs() < 1e-6);
        // 2048 frames would ask for 20.07 subframes; clamp to the 50 ms ceiling.
        assert!((mic_fifo_target_subframes(2048) - MIC_FIFO_MAX_TARGET_SUBFRAMES).abs() < 1e-6);
        assert!((mic_fifo_target_subframes(8192) - MIC_FIFO_MAX_TARGET_SUBFRAMES).abs() < 1e-6);
        // The floor still applies to tiny callback sizes and the result is
        // monotonic between 240 and 1024 frames.
        assert!((mic_fifo_target_subframes(64) - MIC_FIFO_MIN_TARGET_SUBFRAMES).abs() < 1e-6);
        assert!(mic_fifo_target_subframes(512) <= mic_fifo_target_subframes(1024));
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

    #[test]
    fn test_post_dtx_rebase_logic() {
        // Without dtx_silent, a gap of 20 subframes (seq_delta = 20) falls
        // within the normal window (seq_delta <= 48) and does NOT rebase.
        let mut dtx_silent = false;
        let base_seq = 180u16;
        let mut next_seq = 160u16;
        let mut latest_seq = 160u16;
        let mut prebuffer_ready = true;

        let seq_delta = base_seq.wrapping_sub(next_seq) as i16;
        assert_eq!(seq_delta, 20);
        let would_rebase_without_dtx = dtx_silent || seq_delta > 48 || seq_delta < -32;
        assert!(!would_rebase_without_dtx);

        // With dtx_silent set by SilenceHeartbeat, it MUST trigger rebase
        dtx_silent = true;
        let should_rebase = dtx_silent || seq_delta > 48 || seq_delta < -32;
        assert!(should_rebase);

        if should_rebase {
            dtx_silent = false;
            next_seq = base_seq;
            latest_seq = base_seq;
            prebuffer_ready = false;
        }

        assert!(!dtx_silent);
        assert_eq!(next_seq, 180);
        assert_eq!(latest_seq, 180);
        assert!(!prebuffer_ready);
    }
}
