use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU16, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use audioshare_host_engine::{configure_realtime_udp_socket, promote_thread_realtime};
use audioshare_protocol::crypto::SessionKey;
use audioshare_protocol::dsp::{BurstDecouplerRing, ClockDriftResampler, TruePeakLimiter};
use audioshare_protocol::lpc_lossless::{decode_fec_subframe_to_f32, decode_lossless_subframe};
use audioshare_protocol::sequencer::{
    ZeroJitterSequencer, STATUS_EMPTY, STATUS_HEALED_BY_FEC, STATUS_PRIMARY_LOSSLESS,
};
use audioshare_protocol::wire::{
    ClientFeedbackPacket, CodecMode, ParsedPacket, CONTROL_FEEDBACK_LEN,
};
use audioshare_protocol::{
    MAX_SAFE_UDP_PAYLOAD, SAMPLE_RATE, SUBFRAME_STEREO_SAMPLES,
};

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
    pub downstream_bt_delay_ms: AtomicU16,
    pub host_volume: AtomicU8,
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
            downstream_bt_delay_ms: AtomicU16::new(0),
            host_volume: AtomicU8::new(100),
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
                let _android_aaudio =
                    android_aaudio::AAudioOutputGuard::open_low_latency(Arc::clone(&burst_fifo));

                Self::realtime_rx_and_dsp_loop(
                    socket,
                    config,
                    burst_fifo,
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

        let cfg = cpal::StreamConfig {
            channels: 2,
            sample_rate: cpal::SampleRate(SAMPLE_RATE),
            buffer_size: cpal::BufferSize::Default,
        };

        let mut dither_rng = 0x9E37_79B9u32;
        let stream = device
            .build_output_stream(
                &cfg,
                move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    burst_fifo.pop_exact_or_dither(out, &mut dither_rng);
                },
                |_| {},
                None,
            )
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
        running: Arc<AtomicBool>,
        telemetry: Arc<ClientTelemetry>,
        session_key: Arc<Mutex<Option<SessionKey>>>,
    ) {
        let mut sequencer = ZeroJitterSequencer::new();
        let mut resampler = ClockDriftResampler::new();
        let mut limiter = TruePeakLimiter::new(config.preamp_gain_db);

        let mut recv_buf = [0u8; MAX_SAFE_UDP_PAYLOAD];
        let mut stream_initialized = false;
        let mut next_play_seq: u16 = 0;

        let mut last_feedback = Instant::now();
        let mut next_dsp_tick = Instant::now() + Duration::from_micros(2500);
        let mut window_primary = 0u32;
        let mut window_fec = 0u32;

        while running.load(Ordering::Relaxed) {
            if last_feedback.elapsed() >= Duration::from_millis(500) {
                let total_win = (window_primary + window_fec).max(1);
                let fec_permille = ((window_fec * 1000) / total_win) as u16;
                window_primary = 0;
                window_fec = 0;

                let fb = ClientFeedbackPacket {
                    fec_rate_permille: fec_permille,
                    bluetooth_delay_ms: telemetry.downstream_bt_delay_ms.load(Ordering::Relaxed),
                    client_volume: 100,
                    requested_mode: 0,
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

            let _ = socket.set_nonblocking(true);
            let active_key = *session_key.lock().unwrap();

            while let Ok((len, _)) = socket.recv_from(&mut recv_buf) {
                match ParsedPacket::parse_and_decrypt(&mut recv_buf[..len], active_key.as_ref()) {
                    Ok(parsed) => {
                        telemetry.packets_received.fetch_add(1, Ordering::Relaxed);
                        telemetry
                            .host_volume
                            .store(parsed.header.host_volume, Ordering::Relaxed);

                        let base_seq = parsed.header.sequence;
                        if !stream_initialized {
                            next_play_seq = base_seq;
                            stream_initialized = true;
                        }

                        if parsed.header.mode == CodecMode::SilenceHeartbeat {
                            telemetry.server_dtx_silent.store(true, Ordering::Relaxed);
                            next_play_seq = base_seq;
                            continue;
                        }

                        telemetry.server_dtx_silent.store(false, Ordering::Relaxed);

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
                                decode_fec_subframe_to_f32(
                                    &prim[cursor..cursor + sub_len],
                                    &mut pcm_f32,
                                )
                            } else {
                                let mut pcm_i16 = [0i16; SUBFRAME_STEREO_SAMPLES];
                                if decode_lossless_subframe(
                                    &prim[cursor..cursor + sub_len],
                                    &mut pcm_i16,
                                )
                                .is_ok()
                                {
                                    for (dst, &s) in pcm_f32.iter_mut().zip(pcm_i16.iter()) {
                                        *dst = (s as f32) * (1.0 / 32768.0);
                                    }
                                    true
                                } else {
                                    false
                                }
                            };

                            if decoded_ok {
                                sequencer.push_primary(base_seq.wrapping_add(sub_idx), &pcm_f32);
                            }
                            cursor += sub_len;
                        }
                    }
                    Err(_) => {
                        telemetry.auth_failures.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }

            let now = Instant::now();
            while now >= next_dsp_tick {
                next_dsp_tick += Duration::from_micros(2500);

                if stream_initialized && !telemetry.server_dtx_silent.load(Ordering::Relaxed) {
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
                            telemetry
                                .fec_healed_frames
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        STATUS_EMPTY => {
                            window_fec += 1;
                            telemetry
                                .plc_extrapolated_frames
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        _ => {}
                    }

                    let gain_db =
                        (telemetry.preamp_gain_db_tenths.load(Ordering::Relaxed) as f32) / 10.0;
                    limiter.set_gain_db(gain_db);
                    limiter.process_in_place(&mut raw_subframe);

                    let queued_subframes =
                        (burst_fifo.available_samples() as f32) / (SUBFRAME_STEREO_SAMPLES as f32);
                    resampler.update_clock_loop(queued_subframes, config.target_buffer_subframes);

                    let ppm = ((resampler.ratio - 1.0) * 1_000_000.0) as i32;
                    telemetry.clock_drift_ppm.store(ppm, Ordering::Relaxed);

                    let mut resampled_out = [0.0f32; SUBFRAME_STEREO_SAMPLES + 16];
                    let written_frames = resampler.process(&raw_subframe, &mut resampled_out);
                    burst_fifo.push_samples(&resampled_out[..written_frames * 2]);
                }
            }

            std::thread::sleep(Duration::from_micros(600));
        }
    }
}

impl Drop for ClientEngine {
    fn drop(&mut self) {
        self.stop();
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
    const AAUDIO_CALLBACK_RESULT_CONTINUE: i32 = 0;

    type AAudioDataCallback = extern "C" fn(
        stream: *mut AAudioStream,
        user_data: *mut c_void,
        audio_data: *mut c_void,
        num_frames: i32,
    ) -> i32;

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
        fn AAudioStreamBuilder_openStream(
            builder: *mut AAudioStreamBuilder,
            stream: *mut *mut AAudioStream,
        ) -> i32;
        fn AAudioStreamBuilder_delete(builder: *mut AAudioStreamBuilder) -> i32;
        fn AAudioStream_requestStart(stream: *mut AAudioStream) -> i32;
        fn AAudioStream_requestStop(stream: *mut AAudioStream) -> i32;
        fn AAudioStream_close(stream: *mut AAudioStream) -> i32;
    }

    struct CallbackContext {
        fifo: Arc<BurstDecouplerRing>,
        dither_rng: u32,
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
            let out_slice =
                core::slice::from_raw_parts_mut(audio_data as *mut f32, total_samples);
            ctx.fifo
                .pop_exact_or_dither(out_slice, &mut ctx.dither_rng);
        }
        AAUDIO_CALLBACK_RESULT_CONTINUE
    }

    pub struct AAudioOutputGuard {
        stream: *mut AAudioStream,
        ctx_ptr: *mut CallbackContext,
    }

    unsafe impl Send for AAudioOutputGuard {}

    impl AAudioOutputGuard {
        pub fn open_low_latency(fifo: Arc<BurstDecouplerRing>) -> Option<Self> {
            unsafe {
                let mut builder: *mut AAudioStreamBuilder = core::ptr::null_mut();
                if AAudio_createStreamBuilder(&mut builder) != 0 || builder.is_null() {
                    return None;
                }

                let ctx = Box::new(CallbackContext {
                    fifo,
                    dither_rng: 0x1357_9BDF,
                });
                let ctx_ptr = Box::into_raw(ctx);

                AAudioStreamBuilder_setDirection(builder, AAUDIO_DIRECTION_OUTPUT);
                AAudioStreamBuilder_setPerformanceMode(
                    builder,
                    AAUDIO_PERFORMANCE_MODE_LOW_LATENCY,
                );
                AAudioStreamBuilder_setSharingMode(builder, AAUDIO_SHARING_MODE_EXCLUSIVE);
                AAudioStreamBuilder_setSampleRate(builder, SAMPLE_RATE as i32);
                AAudioStreamBuilder_setChannelCount(builder, 2);
                AAudioStreamBuilder_setFormat(builder, AAUDIO_FORMAT_PCM_FLOAT);
                AAudioStreamBuilder_setDataCallback(
                    builder,
                    aaudio_output_callback,
                    ctx_ptr as *mut c_void,
                );

                let mut stream: *mut AAudioStream = core::ptr::null_mut();
                let res = AAudioStreamBuilder_openStream(builder, &mut stream);
                AAudioStreamBuilder_delete(builder);

                if res != 0 || stream.is_null() {
                    let _ = Box::from_raw(ctx_ptr);
                    return None;
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
                // Exclusive input is frequently unavailable; shared mode still
                // uses the low-latency fast path when the device supports it.
                AAudioStreamBuilder_setSharingMode(builder, AAUDIO_SHARING_MODE_SHARED);
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
