use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU16, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use audioshare_host_engine::{configure_realtime_udp_socket, promote_thread_realtime};
use audioshare_protocol::crypto::SessionKey;
use audioshare_protocol::dsp::{BurstDecouplerRing, ClockDriftResampler, TruePeakLimiter};
use audioshare_protocol::lpc_lossless::{
    decode_fec_subframe_to_f32, decode_lossless_pcm24_to_f32, decode_lossless_subframe,
    decode_vbr_subframe_to_f32, TAG_VBR_BFP,
};
use audioshare_protocol::sequencer::{
    ZeroJitterSequencer, STATUS_EMPTY, STATUS_HEALED_BY_FEC, STATUS_PRIMARY_LOSSLESS,
};
use audioshare_protocol::wire::{
    ClientFeedbackPacket, CodecMode, ParsedPacket, CONTROL_FEEDBACK_LEN,
};
use audioshare_protocol::{MAX_SAFE_UDP_PAYLOAD, SAMPLE_RATE, SUBFRAME_STEREO_SAMPLES};

#[cfg(not(target_os = "android"))]
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

const CONTROL_MAGIC_HELLO: &[u8; 8] = b"ASULTRA1";

#[derive(Debug, Clone, Copy)]
pub struct ClientConfig {
    pub server_addr: SocketAddr,
    pub target_buffer_subframes: f32,
    pub preamp_gain_db: f32,
}

pub struct ClientTelemetry {
    pub packets_received: AtomicU64,
    pub primary_frames_played: AtomicU64,
    pub fec_healed_frames: AtomicU64,
    pub plc_extrapolated_frames: AtomicU64,
    pub auth_failures: AtomicU64,
    pub clock_drift_ppm: AtomicI32,
    pub preamp_gain_db_tenths: AtomicI32,
    pub eq_bass_db_tenths: AtomicI32,
    pub eq_presence_db_tenths: AtomicI32,
    pub eq_treble_db_tenths: AtomicI32,
    pub channel_role: AtomicU8,
    pub enable_binaural_crossfeed: AtomicBool,
    pub enable_night_mode: AtomicBool,
    pub gaming_mode_hard_lock: AtomicBool,
    pub downstream_bt_delay_ms: AtomicU16,
    /// Hardware/DAC latency measured with `AAudioStream_getTimestamp` on
    /// Android. Kept separate from the A2DP codec estimate reported by Kotlin;
    /// consumers use the larger of the two.
    pub downstream_hw_delay_ms: AtomicU16,
    pub pending_media_command: AtomicU8,
    pub host_volume: AtomicU8,
    pub max_group_delay_ms: AtomicU16,
    pub seconds_since_last_packet: AtomicU16,
    pub server_dtx_silent: AtomicBool,
}

pub struct ClientEngine {
    running: Arc<AtomicBool>,
    pub telemetry: Arc<ClientTelemetry>,
    session_key: Arc<Mutex<Option<SessionKey>>>,
    rx_handle: Option<std::thread::JoinHandle<()>>,
}

impl ClientEngine {
    pub fn connect(config: ClientConfig) -> Result<Self, String> {
        Self::connect_with_pin(config, None)
    }

    pub fn connect_with_pin(config: ClientConfig, pin: Option<&str>) -> Result<Self, String> {
        let running = Arc::new(AtomicBool::new(true));
        let telemetry = Arc::new(ClientTelemetry {
            packets_received: AtomicU64::new(0),
            primary_frames_played: AtomicU64::new(0),
            fec_healed_frames: AtomicU64::new(0),
            plc_extrapolated_frames: AtomicU64::new(0),
            auth_failures: AtomicU64::new(0),
            clock_drift_ppm: AtomicI32::new(0),
            preamp_gain_db_tenths: AtomicI32::new((config.preamp_gain_db * 10.0) as i32),
            eq_bass_db_tenths: AtomicI32::new(0),
            eq_presence_db_tenths: AtomicI32::new(0),
            eq_treble_db_tenths: AtomicI32::new(0),
            channel_role: AtomicU8::new(0),
            enable_binaural_crossfeed: AtomicBool::new(false),
            enable_night_mode: AtomicBool::new(false),
            gaming_mode_hard_lock: AtomicBool::new(false),
            downstream_bt_delay_ms: AtomicU16::new(0),
            downstream_hw_delay_ms: AtomicU16::new(0),
            pending_media_command: AtomicU8::new(0),
            host_volume: AtomicU8::new(100),
            max_group_delay_ms: AtomicU16::new(0),
            seconds_since_last_packet: AtomicU16::new(0),
            server_dtx_silent: AtomicBool::new(false),
        });

        let session_key = Arc::new(Mutex::new(pin.map(SessionKey::from_pin)));

        let socket = UdpSocket::bind("0.0.0.0:0")
            .map_err(|e| format!("Failed to bind client UDP socket: {}", e))?;
        configure_realtime_udp_socket(&socket)
            .map_err(|e| format!("Failed to configure real-time client socket: {}", e))?;
        let _ = socket.set_read_timeout(Some(Duration::from_millis(5)));

        let _ = socket.send_to(CONTROL_MAGIC_HELLO, config.server_addr);

        let burst_fifo = Arc::new(BurstDecouplerRing::new());
        let running_clone = Arc::clone(&running);
        let telemetry_clone = Arc::clone(&telemetry);
        let key_clone = Arc::clone(&session_key);

        let rx_handle = std::thread::Builder::new()
            .name("as-ultra-client-rx".into())
            .spawn(move || {
                promote_thread_realtime();

                #[cfg(not(target_os = "android"))]
                let _audio_stream = Self::spawn_desktop_output_sink(Arc::clone(&burst_fifo)).ok();

                #[cfg(target_os = "android")]
                let mut _android_aaudio =
                    android_aaudio::AAudioOutputGuard::open_low_latency(Arc::clone(&burst_fifo));

                Self::realtime_rx_and_dsp_loop(
                    socket,
                    config,
                    burst_fifo,
                    #[cfg(target_os = "android")]
                    &mut _android_aaudio,
                    running_clone,
                    telemetry_clone,
                    key_clone,
                );
            })
            .map_err(|e| format!("Failed to spawn client RX thread: {}", e))?;

        Ok(Self {
            running,
            telemetry,
            session_key,
            rx_handle: Some(rx_handle),
        })
    }

    pub fn set_encryption_pin(&self, pin: Option<&str>) {
        let mut guard = self.session_key.lock().unwrap();
        *guard = pin.map(SessionKey::from_pin);
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(h) = self.rx_handle.take() {
            let _ = h.join();
        }
    }

    #[cfg(not(target_os = "android"))]
    fn spawn_desktop_output_sink(
        burst_fifo: Arc<BurstDecouplerRing>,
    ) -> Result<cpal::Stream, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "No audio output device available".to_string())?;
        let default_cfg = device
            .default_output_config()
            .map_err(|e| format!("Output config error: {}", e))?;
        let sample_format = default_cfg.sample_format();

        let cfg = cpal::StreamConfig {
            channels: 2,
            sample_rate: cpal::SampleRate(SAMPLE_RATE),
            buffer_size: cpal::BufferSize::Default,
        };

        let mut dither_rng = 0x9E37_79B9u32;
        let stream = match sample_format {
            cpal::SampleFormat::I16 => device.build_output_stream(
                &cfg,
                move |out: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    let mut tmp = [0.0f32; 4096];
                    let count = out.len().min(4096);
                    burst_fifo.pop_exact_or_dither(&mut tmp[..count], &mut dither_rng);
                    for (dst, &s) in out[..count].iter_mut().zip(tmp[..count].iter()) {
                        *dst = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                    }
                },
                |_| {},
                None,
            ),
            _ => device.build_output_stream(
                &cfg,
                move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    burst_fifo.pop_exact_or_dither(out, &mut dither_rng);
                },
                |_| {},
                None,
            ),
        }
        .map_err(|e| format!("Failed to open audio output: {}", e))?;

        stream
            .play()
            .map_err(|e| format!("Failed to start audio output: {}", e))?;
        Ok(stream)
    }

    fn realtime_rx_and_dsp_loop(
        socket: UdpSocket,
        config: ClientConfig,
        burst_fifo: Arc<BurstDecouplerRing>,
        #[cfg(target_os = "android")] aaudio_guard: &mut Option<android_aaudio::AAudioOutputGuard>,
        running: Arc<AtomicBool>,
        telemetry: Arc<ClientTelemetry>,
        session_key: Arc<Mutex<Option<SessionKey>>>,
    ) {
        let mut sequencer = ZeroJitterSequencer::new();
        let mut resampler = ClockDriftResampler::new();
        let mut limiter = TruePeakLimiter::new(config.preamp_gain_db);
        let mut studio_eq = audioshare_protocol::sync_and_eq::StudioEqualizer::new();
        let mut crossfeed = audioshare_protocol::sync_and_eq::BauerCrossfeedFilter::new();
        let mut night_compressor = audioshare_protocol::sync_and_eq::NightModeCompressor::new();
        let group_sync = audioshare_protocol::sync_and_eq::GroupSyncEstimator::new();

        let mut recv_buf = [0u8; MAX_SAFE_UDP_PAYLOAD];
        let mut stream_initialized = false;
        let mut prebuffer_ready = false;
        let mut next_play_seq: u16 = 0;
        let mut latest_rx_seq: u16 = 0;

        let mut last_feedback = Instant::now();
        let mut last_packet_instant = Instant::now();
        #[cfg(target_os = "android")]
        let mut last_hw_latency_poll = Instant::now();
        let mut window_primary = 0u32;
        let mut window_fec = 0u32;
        // Auto-adaptive jitter cushion: 3.0 (7.5ms) on 5GHz, auto-expands up to 12.0 (30.0ms) on 2.4GHz!
        // Ensure >= 5.5 subframes (13.75ms) on Wi-Fi so Packet [K+1]'s FEC always arrives before Packet [K] plays!
        let initial_wifi_floor = if config.target_buffer_subframes <= 2.5 {
            config.target_buffer_subframes
        } else {
            config.target_buffer_subframes.max(5.5)
        };
        let mut adaptive_wifi_subframes = initial_wifi_floor;

        while running.load(Ordering::Relaxed) {
            // Auto-recover Android AAudio stream within 25ms when headphones/Bluetooth route changes!
            #[cfg(target_os = "android")]
            if aaudio_guard
                .as_ref()
                .map(|g| g.needs_reopen())
                .unwrap_or(true)
            {
                *aaudio_guard =
                    android_aaudio::AAudioOutputGuard::open_low_latency(Arc::clone(&burst_fifo));
                // The previous route's measurement is no longer valid.
                telemetry.downstream_hw_delay_ms.store(0, Ordering::Relaxed);
            }

            // Sample the AAudio hardware timestamp twice a second. This is
            // intentionally done from the RX loop (not the audio callback) so
            // a slow HAL timestamp query can never cause an output glitch.
            #[cfg(target_os = "android")]
            if last_hw_latency_poll.elapsed() >= Duration::from_millis(500) {
                last_hw_latency_poll = Instant::now();
                if let Some(delay_ms) = aaudio_guard
                    .as_ref()
                    .and_then(|guard| guard.measure_hardware_latency_ms())
                {
                    telemetry
                        .downstream_hw_delay_ms
                        .store(delay_ms, Ordering::Relaxed);
                }
            }

            if last_feedback.elapsed() >= Duration::from_millis(500)
                || telemetry.pending_media_command.load(Ordering::Relaxed) != 0
            {
                telemetry.seconds_since_last_packet.store(
                    last_packet_instant.elapsed().as_secs().min(65535) as u16,
                    Ordering::Relaxed,
                );

                let total_win = (window_primary + window_fec).max(1);
                let fec_permille = ((window_fec * 1000) / total_win) as u16;
                window_primary = 0;
                window_fec = 0;

                // If 500ms window had zero drops/late packets, gently decay 2.4GHz cushion back toward floor
                if fec_permille == 0 && adaptive_wifi_subframes > initial_wifi_floor {
                    adaptive_wifi_subframes =
                        (adaptive_wifi_subframes - 0.15).max(initial_wifi_floor);
                }

                let media_cmd = telemetry.pending_media_command.swap(0, Ordering::Relaxed);

                let gain_db =
                    (telemetry.preamp_gain_db_tenths.load(Ordering::Relaxed) as f32) / 10.0;
                let client_vol = if gain_db <= -40.0 {
                    0u8
                } else {
                    (10.0f32.powf(gain_db / 20.0) * 100.0).clamp(0.0, 100.0) as u8
                };

                // Use the larger of the Kotlin A2DP estimate and the measured
                // AAudio hardware/DAC delay, so wired output also reports real
                // hardware latency while Bluetooth keeps its full codec delay.
                let downstream_delay_ms = telemetry
                    .downstream_bt_delay_ms
                    .load(Ordering::Relaxed)
                    .max(telemetry.downstream_hw_delay_ms.load(Ordering::Relaxed));

                let fb = ClientFeedbackPacket {
                    fec_rate_permille: fec_permille,
                    bluetooth_delay_ms: downstream_delay_ms,
                    client_volume: client_vol,
                    requested_mode: media_cmd,
                    auth_mac32: 0,
                };
                let mut fb_bytes = [0u8; CONTROL_FEEDBACK_LEN];
                fb.encode(&mut fb_bytes);

                let _ = socket.send_to(&fb_bytes, config.server_addr);
                if !config.server_addr.ip().is_loopback() {
                    let usb_adb_addr =
                        SocketAddr::from(([127, 0, 0, 1], config.server_addr.port()));
                    let _ = socket.send_to(&fb_bytes, usb_adb_addr);
                }

                last_feedback = Instant::now();
            }

            let active_key = *session_key.lock().unwrap();
            // Block for up to 800us waiting for the first packet (0% idle CPU, <15us wake on arrival),
            // then switch to non-blocking to drain all queued burst packets immediately.
            let _ = socket.set_nonblocking(false);
            let _ = socket.set_read_timeout(Some(Duration::from_micros(800)));

            while let Ok((len, _)) = socket.recv_from(&mut recv_buf) {
                let _ = socket.set_nonblocking(true);
                match ParsedPacket::parse_and_decrypt(&mut recv_buf[..len], active_key.as_ref()) {
                    Ok(parsed) => {
                        last_packet_instant = Instant::now();
                        telemetry.packets_received.fetch_add(1, Ordering::Relaxed);
                        telemetry
                            .host_volume
                            .store(parsed.header.host_volume, Ordering::Relaxed);
                        telemetry
                            .max_group_delay_ms
                            .store((parsed.header.stream_id as u16) * 2, Ordering::Relaxed);

                        let base_seq = parsed.header.sequence;
                        if !stream_initialized {
                            next_play_seq = base_seq;
                            latest_rx_seq = base_seq;
                            stream_initialized = true;
                        }

                        if parsed.header.mode == CodecMode::SilenceHeartbeat {
                            telemetry.server_dtx_silent.store(true, Ordering::Relaxed);
                            prebuffer_ready = false;
                            next_play_seq = base_seq;
                            continue;
                        }

                        // Signed 16-bit sequence delta (RFC 1982): >0 means future, <0 means past/late duplicate
                        let seq_delta = base_seq.wrapping_sub(next_play_seq) as i16;
                        if telemetry.server_dtx_silent.swap(false, Ordering::Relaxed)
                            || seq_delta > 48
                            || seq_delta < -200
                        {
                            next_play_seq = base_seq;
                            latest_rx_seq = base_seq;
                            prebuffer_ready = false;
                        }

                        if !parsed.fec_n1_payload.is_empty() {
                            let mut fec1_f32 = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                            if decode_fec_subframe_to_f32(parsed.fec_n1_payload, &mut fec1_f32) {
                                sequencer.try_heal_with_fec(base_seq.wrapping_sub(1), &fec1_f32);
                            }
                        }
                        if !parsed.fec_n2_payload.is_empty() {
                            let mut fec2_f32 = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                            if decode_fec_subframe_to_f32(parsed.fec_n2_payload, &mut fec2_f32) {
                                sequencer.try_heal_with_fec(base_seq.wrapping_sub(2), &fec2_f32);
                            }
                        }

                        let mut cursor = 0usize;
                        let prim = parsed.primary_payload;
                        for sub_idx in 0..(parsed.header.subframe_count as u16) {
                            if cursor + 2 > prim.len() {
                                break;
                            }
                            let sub_len =
                                u16::from_be_bytes([prim[cursor], prim[cursor + 1]]) as usize;
                            cursor += 2;
                            if cursor + sub_len > prim.len() {
                                break;
                            }

                            let mut pcm_f32 = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                            let decoded_ok = if parsed.header.mode == CodecMode::CompressedLowDelay
                            {
                                let payload = &prim[cursor..cursor + sub_len];
                                if payload.first() == Some(&TAG_VBR_BFP) {
                                    decode_vbr_subframe_to_f32(payload, &mut pcm_f32)
                                } else {
                                    // Backward compatible with hosts that still
                                    // send a raw IMA-ADPCM primary frame.
                                    decode_fec_subframe_to_f32(payload, &mut pcm_f32)
                                }
                            } else {
                                let payload = &prim[cursor..cursor + sub_len];
                                // True 24-bit profile: upper-16-bit LPC plus
                                // the exact 8-bit LSB plane. Falls through to
                                // the legacy 16-bit packer for all other tags.
                                if decode_lossless_pcm24_to_f32(payload, &mut pcm_f32) {
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

                            if decoded_ok {
                                let s = base_seq.wrapping_add(sub_idx);
                                // Only insert if this subframe has not already been played by the DAC
                                if (s.wrapping_sub(next_play_seq) as i16) >= 0 {
                                    sequencer.push_primary(s, &pcm_f32);
                                }
                                if (s.wrapping_sub(latest_rx_seq) as i16) > 0 {
                                    latest_rx_seq = s;
                                }
                            }
                            cursor += sub_len;
                        }

                        let buffered =
                            (latest_rx_seq.wrapping_sub(next_play_seq) as i16).max(0) as f32;
                        if buffered >= initial_wifi_floor {
                            prebuffer_ready = true;
                        }
                    }
                    Err(_) => {
                        telemetry.auth_failures.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }

            // 4. Multi-Phone Group Sync Alignment + Competitive Gaming Hard-Lock
            let base_target = if telemetry.gaming_mode_hard_lock.load(Ordering::Relaxed) {
                2.0
            } else {
                adaptive_wifi_subframes
            };
            let downstream_delay_ms = telemetry
                .downstream_bt_delay_ms
                .load(Ordering::Relaxed)
                .max(telemetry.downstream_hw_delay_ms.load(Ordering::Relaxed));
            let target_buf = group_sync.aligned_target_subframes(
                base_target,
                telemetry.max_group_delay_ms.load(Ordering::Relaxed),
                downstream_delay_ms,
            );

            // Demand-driven by the Hardware Audio DAC:
            // Keep burst_fifo topped up to 4 subframes (960 floats = 480 stereo frames) so no AAudio burst size ever starves!
            let fifo_watermark = if target_buf <= 2.5 {
                SUBFRAME_STEREO_SAMPLES + SUBFRAME_STEREO_SAMPLES / 2 // 360 samples = 1.5 subframes
            } else {
                SUBFRAME_STEREO_SAMPLES * 4 // 960 samples = 4 subframes
            };

            while stream_initialized
                && prebuffer_ready
                && !telemetry.server_dtx_silent.load(Ordering::Relaxed)
                && burst_fifo.available_samples() < fifo_watermark
            {
                let mut raw_subframe = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                let status = sequencer.pop_next(next_play_seq, &mut raw_subframe);
                next_play_seq = next_play_seq.wrapping_add(1);

                match status {
                    STATUS_PRIMARY_LOSSLESS => {
                        window_primary += 1;
                        telemetry
                            .primary_frames_played
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    STATUS_HEALED_BY_FEC => {
                        window_fec += 1;
                        // 2.4GHz Wi-Fi jitter/drop detected: expand cushion by +0.35 subframes (max 12.0 = 30.0ms)
                        adaptive_wifi_subframes = (adaptive_wifi_subframes + 0.35).min(12.0);
                        telemetry.fec_healed_frames.fetch_add(1, Ordering::Relaxed);
                    }
                    STATUS_EMPTY => {
                        window_fec += 1;
                        // 2.4GHz burst delay exceeded cushion: expand cushion by +0.75 subframes (max 12.0 = 30.0ms)
                        adaptive_wifi_subframes = (adaptive_wifi_subframes + 0.75).min(12.0);
                        telemetry
                            .plc_extrapolated_frames
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {}
                }

                // 1. Apply 3-Band Studio Parametric EQ (0.00ms delay)
                studio_eq.update_if_changed(
                    telemetry.eq_bass_db_tenths.load(Ordering::Relaxed),
                    telemetry.eq_presence_db_tenths.load(Ordering::Relaxed),
                    telemetry.eq_treble_db_tenths.load(Ordering::Relaxed),
                );
                studio_eq.process(&mut raw_subframe);

                // 2. Apply Spatial Channel Role (Stereo / Left / Right / Center / Surround)
                let role = audioshare_protocol::sync_and_eq::ChannelRole::from_u8(
                    telemetry.channel_role.load(Ordering::Relaxed),
                );
                role.apply_in_place(&mut raw_subframe);

                // 3. Apply Audiophile Bauer Headphone Crossfeed if enabled (0.00ms delay)
                if telemetry.enable_binaural_crossfeed.load(Ordering::Relaxed) {
                    crossfeed.process_in_place(&mut raw_subframe);
                }

                // 3b. Apply Night-Mode Upward Whisper Compressor & Explosion Tamer (0.00ms delay)
                if telemetry.enable_night_mode.load(Ordering::Relaxed) {
                    night_compressor.process_in_place(&mut raw_subframe);
                }

                let gain_db =
                    (telemetry.preamp_gain_db_tenths.load(Ordering::Relaxed) as f32) / 10.0;
                limiter.set_gain_db(gain_db);
                limiter.process_in_place(&mut raw_subframe);

                // Apply Host Master Volume (0..=100%)
                let host_vol = (telemetry.host_volume.load(Ordering::Relaxed) as f32) / 100.0;
                if (host_vol - 1.0).abs() > 0.005 {
                    for s in &mut raw_subframe {
                        *s *= host_vol;
                    }
                }

                // Measure true network sequencer queue depth (Host Clock vs Client DAC Clock)
                let fifo_subframes =
                    (burst_fifo.available_samples() as f32) / (SUBFRAME_STEREO_SAMPLES as f32);
                let mut net_subframes =
                    (latest_rx_seq.wrapping_sub(next_play_seq) as i16).max(0) as f32;
                if net_subframes > target_buf + 14.0 {
                    // Preserve `target_buf` cushion on fast-forward so 2.4GHz doesn't immediately underrun!
                    let keep = (target_buf.ceil() as u16).max(2);
                    next_play_seq = latest_rx_seq.wrapping_sub(keep);
                    net_subframes = keep as f32;
                }
                // Regulate COMBINED (Sequencer + FIFO) buffer so they never stack in series!
                resampler.update_clock_loop(net_subframes + fifo_subframes, target_buf);

                let ppm = ((resampler.ratio - 1.0) * 1_000_000.0) as i32;
                telemetry.clock_drift_ppm.store(ppm, Ordering::Relaxed);

                let mut resampled_out = [0.0f32; SUBFRAME_STEREO_SAMPLES + 16];
                let written_frames = resampler.process(&raw_subframe, &mut resampled_out);
                burst_fifo.push_samples(&resampled_out[..written_frames * 2]);
            }
        }
    }
}

impl Drop for ClientEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_signed_sequence_delta_rfc1982() {
        // Late duplicate packet arrived after playback moved ahead
        let next_play_seq = 101u16;
        let late_seq = 99u16;
        let delta = late_seq.wrapping_sub(next_play_seq) as i16;
        assert_eq!(delta, -2);
        assert!(delta < 0, "Duplicate/past packet must have negative delta");

        // Forward sequence wrap-around across 65535 boundary
        let next_play_seq = 65535u16;
        let future_seq = 2u16;
        let delta = future_seq.wrapping_sub(next_play_seq) as i16;
        assert_eq!(delta, 3);
        assert!(
            delta > 0,
            "Future packet across wrap boundary must have positive delta"
        );

        // Past sequence across wrap-around boundary
        let next_play_seq = 2u16;
        let past_seq = 65535u16;
        let delta = past_seq.wrapping_sub(next_play_seq) as i16;
        assert_eq!(delta, -3);
        assert!(
            delta < 0,
            "Past packet across wrap boundary must have negative delta"
        );

        // Normal buffered calculation
        let latest_rx = 50u16;
        let next_play = 46u16;
        let buffered = (latest_rx.wrapping_sub(next_play) as i16).max(0) as f32;
        assert_eq!(buffered, 4.0);

        // Negative buffered (late packet or race condition) clamped to 0
        let latest_rx = 40u16;
        let next_play = 45u16;
        let buffered = (latest_rx.wrapping_sub(next_play) as i16).max(0) as f32;
        assert_eq!(buffered, 0.0);
    }

    #[test]
    fn test_instant_burst_spike_fast_forward() {
        let latest_rx = 100u16;
        let mut next_play = 70u16; // 30 subframes lag (>20ms lag)
        let target_buf = 4.0f32;

        let mut net_subframes = (latest_rx.wrapping_sub(next_play) as i16).max(0) as f32;
        assert_eq!(net_subframes, 30.0);

        if net_subframes > target_buf + 14.0 {
            let keep = (target_buf.ceil() as u16).max(2);
            next_play = latest_rx.wrapping_sub(keep);
            net_subframes = keep as f32;
        }

        assert_eq!(net_subframes, 4.0);
        assert_eq!(next_play, 96);
    }

    #[test]
    fn test_adaptive_wifi_cushion_expansion_and_decay() {
        let mut adaptive_wifi_subframes = 5.5f32;
        let initial_floor = 5.5f32;

        // On FEC heal: +0.35
        adaptive_wifi_subframes = (adaptive_wifi_subframes + 0.35).min(12.0);
        assert!((adaptive_wifi_subframes - 5.85).abs() < 1e-4);

        // On empty gap (PLC): +0.75
        adaptive_wifi_subframes = (adaptive_wifi_subframes + 0.75).min(12.0);
        assert!((adaptive_wifi_subframes - 6.60).abs() < 1e-4);

        // On clean 500ms window: decay by 0.15 down to floor
        adaptive_wifi_subframes = (adaptive_wifi_subframes - 0.15).max(initial_floor);
        assert!((adaptive_wifi_subframes - 6.45).abs() < 1e-4);

        // Ceiling cap at 12.0
        for _ in 0..20 {
            adaptive_wifi_subframes = (adaptive_wifi_subframes + 0.75).min(12.0);
        }
        assert_eq!(adaptive_wifi_subframes, 12.0);
    }

    #[test]
    fn test_i16_output_conversion_clamping() {
        let f32_samples: [f32; 4] = [1.5, -1.5, 0.0, 0.5];
        let mut i16_samples = [0i16; 4];
        for (dst, &s) in i16_samples.iter_mut().zip(f32_samples.iter()) {
            *dst = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        }
        assert_eq!(i16_samples[0], 32767);
        assert_eq!(i16_samples[1], -32767);
        assert_eq!(i16_samples[2], 0);
        assert_eq!(i16_samples[3], 16383);
    }
}

/// Direct Zero-Dependency Android NDK `libaaudio.so` Hardware MMAP Binding
#[cfg(target_os = "android")]
pub mod android_aaudio {
    use super::*;
    use core::ffi::c_void;

    #[repr(C)]
    pub struct AAudioStreamBuilder(c_void);
    #[repr(C)]
    pub struct AAudioStream(c_void);

    const AAUDIO_DIRECTION_OUTPUT: i32 = 0;
    const AAUDIO_DIRECTION_INPUT: i32 = 1;
    const AAUDIO_FORMAT_PCM_FLOAT: i32 = 2;
    const AAUDIO_SHARING_MODE_EXCLUSIVE: i32 = 0;
    const AAUDIO_SHARING_MODE_SHARED: i32 = 1;
    const AAUDIO_PERFORMANCE_MODE_LOW_LATENCY: i32 = 12;
    const AAUDIO_USAGE_GAME: i32 = 14;
    const AAUDIO_CALLBACK_RESULT_CONTINUE: i32 = 0;

    #[inline]
    unsafe fn try_set_aaudio_usage(builder: *mut AAudioStreamBuilder, usage: i32) {
        type SetUsageFn = unsafe extern "C" fn(*mut AAudioStreamBuilder, i32);
        static SET_USAGE: std::sync::OnceLock<Option<SetUsageFn>> = std::sync::OnceLock::new();
        let func = SET_USAGE.get_or_init(|| {
            let handle = libc::dlopen(
                b"libaaudio.so\0".as_ptr() as *const libc::c_char,
                libc::RTLD_NOW,
            );
            if !handle.is_null() {
                let sym = libc::dlsym(
                    handle,
                    b"AAudioStreamBuilder_setUsage\0".as_ptr() as *const libc::c_char,
                );
                if !sym.is_null() {
                    Some(core::mem::transmute::<*mut libc::c_void, SetUsageFn>(sym))
                } else {
                    None
                }
            } else {
                None
            }
        });
        if let Some(f) = func {
            f(builder, usage);
        }
    }

    type AAudioDataCallback = extern "C" fn(
        stream: *mut AAudioStream,
        user_data: *mut c_void,
        audio_data: *mut c_void,
        num_frames: i32,
    ) -> i32;

    type AAudioErrorCallback =
        extern "C" fn(stream: *mut AAudioStream, user_data: *mut c_void, error: i32);

    #[link(name = "aaudio")]
    extern "C" {
        fn AAudio_createStreamBuilder(builder: *mut *mut AAudioStreamBuilder) -> i32;
        fn AAudioStreamBuilder_setDirection(builder: *mut AAudioStreamBuilder, dir: i32);
        fn AAudioStreamBuilder_setPerformanceMode(builder: *mut AAudioStreamBuilder, mode: i32);
        fn AAudioStreamBuilder_setSharingMode(builder: *mut AAudioStreamBuilder, mode: i32);
        fn AAudioStreamBuilder_setSampleRate(builder: *mut AAudioStreamBuilder, rate: i32);
        fn AAudioStreamBuilder_setChannelCount(builder: *mut AAudioStreamBuilder, channels: i32);
        fn AAudioStreamBuilder_setFormat(builder: *mut AAudioStreamBuilder, format: i32);
        fn AAudioStreamBuilder_setDataCallback(
            builder: *mut AAudioStreamBuilder,
            callback: AAudioDataCallback,
            user_data: *mut c_void,
        );
        fn AAudioStreamBuilder_setErrorCallback(
            builder: *mut AAudioStreamBuilder,
            callback: AAudioErrorCallback,
            user_data: *mut c_void,
        );
        fn AAudioStreamBuilder_openStream(
            builder: *mut AAudioStreamBuilder,
            stream: *mut *mut AAudioStream,
        ) -> i32;
        fn AAudioStreamBuilder_delete(builder: *mut AAudioStreamBuilder) -> i32;
        fn AAudioStream_getFramesPerBurst(stream: *mut AAudioStream) -> i32;
        fn AAudioStream_setBufferSizeInFrames(stream: *mut AAudioStream, num_frames: i32) -> i32;
        fn AAudioStream_getFramesWritten(stream: *mut AAudioStream) -> i64;
        fn AAudioStream_getTimestamp(
            stream: *mut AAudioStream,
            clockid: i32,
            frame_position: *mut i64,
            time_nanoseconds: *mut i64,
        ) -> i32;
        fn AAudioStream_requestStart(stream: *mut AAudioStream) -> i32;
        fn AAudioStream_requestStop(stream: *mut AAudioStream) -> i32;
        fn AAudioStream_close(stream: *mut AAudioStream) -> i32;
    }

    struct CallbackContext {
        fifo: Arc<BurstDecouplerRing>,
        dither_rng: u32,
        disconnected: AtomicBool,
    }

    extern "C" fn aaudio_output_callback(
        _stream: *mut AAudioStream,
        user_data: *mut c_void,
        audio_data: *mut c_void,
        num_frames: i32,
    ) -> i32 {
        if user_data.is_null() || audio_data.is_null() || num_frames <= 0 {
            return AAUDIO_CALLBACK_RESULT_CONTINUE;
        }
        unsafe {
            let ctx = &mut *(user_data as *mut CallbackContext);
            let total_samples = (num_frames as usize) * 2;
            let out_slice = core::slice::from_raw_parts_mut(audio_data as *mut f32, total_samples);
            ctx.fifo.pop_exact_or_dither(out_slice, &mut ctx.dither_rng);
        }
        AAUDIO_CALLBACK_RESULT_CONTINUE
    }

    extern "C" fn aaudio_error_callback(
        _stream: *mut AAudioStream,
        user_data: *mut c_void,
        _error: i32,
    ) {
        if !user_data.is_null() {
            unsafe {
                let ctx = &*(user_data as *const CallbackContext);
                ctx.disconnected.store(true, Ordering::Release);
            }
        }
    }

    pub struct AAudioOutputGuard {
        stream: *mut AAudioStream,
        ctx_ptr: *mut CallbackContext,
    }

    unsafe impl Send for AAudioOutputGuard {}

    impl AAudioOutputGuard {
        #[inline]
        pub fn needs_reopen(&self) -> bool {
            if self.ctx_ptr.is_null() {
                return true;
            }
            unsafe { (*self.ctx_ptr).disconnected.load(Ordering::Acquire) }
        }

        /// Measure the downstream hardware/DAC pipeline latency from the
        /// difference between frames written and the frame position that
        /// AAudio reports as already presented to the hardware.
        pub fn measure_hardware_latency_ms(&self) -> Option<u16> {
            if self.stream.is_null() {
                return None;
            }
            unsafe {
                let mut frame_position: i64 = 0;
                let mut time_nanoseconds: i64 = 0;
                let result = AAudioStream_getTimestamp(
                    self.stream,
                    libc::CLOCK_MONOTONIC,
                    &mut frame_position,
                    &mut time_nanoseconds,
                );
                if result != 0 || frame_position < 0 || time_nanoseconds <= 0 {
                    return None;
                }
                let written = AAudioStream_getFramesWritten(self.stream);
                let pending_frames = written.saturating_sub(frame_position);
                let delay_ms = ((pending_frames * 1000) / SAMPLE_RATE as i64).clamp(0, 400) as u16;
                Some(delay_ms)
            }
        }

        pub fn open_low_latency(fifo: Arc<BurstDecouplerRing>) -> Option<Self> {
            Self::open_internal(Arc::clone(&fifo), AAUDIO_SHARING_MODE_EXCLUSIVE)
                .or_else(|| Self::open_internal(fifo, AAUDIO_SHARING_MODE_SHARED))
        }

        fn open_internal(fifo: Arc<BurstDecouplerRing>, sharing_mode: i32) -> Option<Self> {
            unsafe {
                let mut builder: *mut AAudioStreamBuilder = core::ptr::null_mut();
                if AAudio_createStreamBuilder(&mut builder) != 0 || builder.is_null() {
                    return None;
                }

                let ctx = Box::new(CallbackContext {
                    fifo,
                    dither_rng: 0x1357_9BDF,
                    disconnected: AtomicBool::new(false),
                });
                let ctx_ptr = Box::into_raw(ctx);

                AAudioStreamBuilder_setDirection(builder, AAUDIO_DIRECTION_OUTPUT);
                AAudioStreamBuilder_setPerformanceMode(
                    builder,
                    AAUDIO_PERFORMANCE_MODE_LOW_LATENCY,
                );
                // Bypasses Android Dolby/Media DSP post-processing and forces ULL Hardware Fast-Track!
                try_set_aaudio_usage(builder, AAUDIO_USAGE_GAME);
                AAudioStreamBuilder_setSharingMode(builder, sharing_mode);
                AAudioStreamBuilder_setSampleRate(builder, SAMPLE_RATE as i32);
                AAudioStreamBuilder_setChannelCount(builder, 2);
                AAudioStreamBuilder_setFormat(builder, AAUDIO_FORMAT_PCM_FLOAT);
                AAudioStreamBuilder_setDataCallback(
                    builder,
                    aaudio_output_callback,
                    ctx_ptr as *mut c_void,
                );
                AAudioStreamBuilder_setErrorCallback(
                    builder,
                    aaudio_error_callback,
                    ctx_ptr as *mut c_void,
                );

                let mut stream: *mut AAudioStream = core::ptr::null_mut();
                let res = AAudioStreamBuilder_openStream(builder, &mut stream);
                AAudioStreamBuilder_delete(builder);

                if res != 0 || stream.is_null() {
                    let _ = Box::from_raw(ctx_ptr);
                    return None;
                }

                // Enforce hardware double-buffering (2 * hardware burst = ~4.0ms internal DAC buffer)
                let burst = AAudioStream_getFramesPerBurst(stream);
                if burst > 0 {
                    AAudioStream_setBufferSizeInFrames(stream, burst * 2);
                }

                AAudioStream_requestStart(stream);
                Some(Self { stream, ctx_ptr })
            }
        }
    }

    impl Drop for AAudioOutputGuard {
        fn drop(&mut self) {
            unsafe {
                if !self.stream.is_null() {
                    AAudioStream_requestStop(self.stream);
                    AAudioStream_close(self.stream);
                }
                if !self.ctx_ptr.is_null() {
                    let _ = Box::from_raw(self.ctx_ptr);
                }
            }
        }
    }

    struct InputContext {
        fifo: Arc<BurstDecouplerRing>,
    }

    extern "C" fn aaudio_input_callback(
        _stream: *mut AAudioStream,
        user_data: *mut c_void,
        audio_data: *mut c_void,
        num_frames: i32,
    ) -> i32 {
        if user_data.is_null() || audio_data.is_null() || num_frames <= 0 {
            return AAUDIO_CALLBACK_RESULT_CONTINUE;
        }
        unsafe {
            let ctx = &mut *(user_data as *mut InputContext);
            let frames = num_frames as usize;
            let mono = core::slice::from_raw_parts(audio_data as *const f32, frames);
            // The ring is interleaved stereo; expand the mono capture stream.
            let mut stereo = [0.0f32; 2048];
            let mut offset = 0usize;
            while offset < frames {
                let chunk = (frames - offset).min(stereo.len() / 2);
                for i in 0..chunk {
                    let sample = mono[offset + i];
                    stereo[i * 2] = sample;
                    stereo[i * 2 + 1] = sample;
                }
                ctx.fifo.push_samples(&stereo[..chunk * 2]);
                offset += chunk;
            }
        }
        AAUDIO_CALLBACK_RESULT_CONTINUE
    }

    pub struct AAudioInputGuard {
        stream: *mut AAudioStream,
        ctx_ptr: *mut InputContext,
    }

    unsafe impl Send for AAudioInputGuard {}

    impl AAudioInputGuard {
        /// Open a low-latency mono microphone stream and expand it to the
        /// stereo ring consumed by `PhoneMicTransmitter`.
        pub fn open_low_latency_mic(fifo: Arc<BurstDecouplerRing>) -> Option<Self> {
            Self::open_internal(Arc::clone(&fifo), AAUDIO_SHARING_MODE_EXCLUSIVE)
                .or_else(|| Self::open_internal(fifo, AAUDIO_SHARING_MODE_SHARED))
        }

        fn open_internal(fifo: Arc<BurstDecouplerRing>, sharing_mode: i32) -> Option<Self> {
            unsafe {
                let mut builder: *mut AAudioStreamBuilder = core::ptr::null_mut();
                if AAudio_createStreamBuilder(&mut builder) != 0 || builder.is_null() {
                    return None;
                }

                let ctx = Box::new(InputContext { fifo });
                let ctx_ptr = Box::into_raw(ctx);

                AAudioStreamBuilder_setDirection(builder, AAUDIO_DIRECTION_INPUT);
                AAudioStreamBuilder_setPerformanceMode(
                    builder,
                    AAUDIO_PERFORMANCE_MODE_LOW_LATENCY,
                );
                AAudioStreamBuilder_setSharingMode(builder, sharing_mode);
                AAudioStreamBuilder_setSampleRate(builder, SAMPLE_RATE as i32);
                AAudioStreamBuilder_setChannelCount(builder, 1);
                AAudioStreamBuilder_setFormat(builder, AAUDIO_FORMAT_PCM_FLOAT);
                AAudioStreamBuilder_setDataCallback(
                    builder,
                    aaudio_input_callback,
                    ctx_ptr as *mut c_void,
                );

                let mut stream: *mut AAudioStream = core::ptr::null_mut();
                let res = AAudioStreamBuilder_openStream(builder, &mut stream);
                AAudioStreamBuilder_delete(builder);

                if res != 0 || stream.is_null() {
                    let _ = Box::from_raw(ctx_ptr);
                    return None;
                }

                // Enforce hardware double-buffering (2 * hardware burst = ~4.0ms internal DAC buffer)
                let burst = AAudioStream_getFramesPerBurst(stream);
                if burst > 0 {
                    AAudioStream_setBufferSizeInFrames(stream, burst * 2);
                }

                AAudioStream_requestStart(stream);
                Some(Self { stream, ctx_ptr })
            }
        }
    }

    impl Drop for AAudioInputGuard {
        fn drop(&mut self) {
            unsafe {
                if !self.stream.is_null() {
                    AAudioStream_requestStop(self.stream);
                    AAudioStream_close(self.stream);
                }
                if !self.ctx_ptr.is_null() {
                    let _ = Box::from_raw(self.ctx_ptr);
                }
            }
        }
    }
}
