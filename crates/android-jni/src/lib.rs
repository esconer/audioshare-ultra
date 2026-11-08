use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[cfg(target_os = "android")]
use audioshare_client_engine::android_aaudio::AAudioInputGuard;
use audioshare_client_engine::{ClientConfig, ClientEngine};
use audioshare_host_engine::discovery_and_os::DiscoveryScanner;
use audioshare_host_engine::virtual_mic::PHONE_MIC_PORT;
use audioshare_host_engine::{configure_realtime_udp_socket, promote_thread_realtime};
use audioshare_protocol::dsp::BurstDecouplerRing;
use audioshare_protocol::lpc_lossless::{
    encode_fec_subframe, encode_lossless_subframe, ADPCM_FEC_BYTES, MAX_LOSSLESS_SUBFRAME_BYTES,
};
use audioshare_protocol::wire::{pack_datagram, CodecMode, WireHeader};
use audioshare_protocol::{MAX_SAFE_UDP_PAYLOAD, SUBFRAME_STEREO_SAMPLES};

static GLOBAL_ANDROID_CLIENT: Mutex<Option<ClientEngine>> = Mutex::new(None);
static GLOBAL_PHONE_MIC_TX: Mutex<Option<PhoneMicTransmitter>> = Mutex::new(None);
static GLOBAL_SCANNER: OnceLock<DiscoveryScanner> = OnceLock::new();

pub fn get_or_start_scanner() -> &'static DiscoveryScanner {
    GLOBAL_SCANNER.get_or_init(DiscoveryScanner::start)
}

pub struct PhoneMicTransmitter {
    running: Arc<AtomicBool>,
    pub mic_input_ring: Arc<BurstDecouplerRing>,
    #[cfg(target_os = "android")]
    _mic_input_stream: Option<AAudioInputGuard>,
    handle: Option<std::thread::JoinHandle<()>>,
}

pub fn parse_target_endpoints(target_hosts: &str) -> Vec<SocketAddr> {
    let mut targets = Vec::new();
    for item in target_hosts.split([',', ';', ' ']) {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let host_only = item.split(':').next().unwrap_or(item);
        if let Ok(addr) = format!("{}:{}", host_only, PHONE_MIC_PORT).parse::<SocketAddr>() {
            if !targets.contains(&addr) {
                targets.push(addr);
            }
        }
    }
    targets
}

impl PhoneMicTransmitter {
    pub fn start(target_hosts: &str, use_hardware_mic: bool) -> Result<Self, String> {
        let targets = parse_target_endpoints(target_hosts);
        if targets.is_empty() {
            return Err("No valid target endpoints found for transmitter".into());
        }

        let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
        configure_realtime_udp_socket(&socket).map_err(|e| e.to_string())?;

        let running = Arc::new(AtomicBool::new(true));
        let mic_input_ring = Arc::new(BurstDecouplerRing::new());
        let _ = use_hardware_mic;

        #[cfg(target_os = "android")]
        let _mic_input_stream = if use_hardware_mic {
            AAudioInputGuard::open_low_latency_mic(Arc::clone(&mic_input_ring))
        } else {
            None
        };

        let run_clone = Arc::clone(&running);
        let ring_clone = Arc::clone(&mic_input_ring);

        let handle = std::thread::Builder::new()
            .name("as-ultra-phone-mic-tx".into())
            .spawn(move || {
                promote_thread_realtime();
                let start = Instant::now();
                let mut seq: u16 = 0;
                let mut fec_n1 = [0u8; ADPCM_FEC_BYTES];
                let mut has_fec = false;
                let mut dither_rng = 1u32;

                while run_clone.load(Ordering::Relaxed) {
                    if ring_clone.available_samples() >= SUBFRAME_STEREO_SAMPLES {
                        let mut f32_buf = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                        ring_clone.pop_exact_or_dither(&mut f32_buf, &mut dither_rng);

                        let mut i16_buf = [0i16; SUBFRAME_STEREO_SAMPLES];
                        for (dst, &s) in i16_buf.iter_mut().zip(f32_buf.iter()) {
                            *dst = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                        }

                        let mut enc = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
                        let enc_len = encode_lossless_subframe(&i16_buf, &mut enc);

                        let mut primary = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES + 2];
                        let len_be = (enc_len as u16).to_be_bytes();
                        primary[0] = len_be[0];
                        primary[1] = len_be[1];
                        primary[2..2 + enc_len].copy_from_slice(&enc[..enc_len]);

                        let hdr = WireHeader {
                            mode: CodecMode::StudioLosslessLpc,
                            subframe_count: 1,
                            encrypted: false,
                            sequence: seq,
                            tx_timestamp_us: start.elapsed().as_micros() as u32,
                            primary_len: 0,
                            fec_n1_len: 0,
                            fec_n2_len: 0,
                            host_volume: 100,
                            stream_id: 2,
                            header_crc16: 0,
                        };

                        let mut dgram = [0u8; MAX_SAFE_UDP_PAYLOAD];
                        let f1 = if has_fec { &fec_n1[..] } else { &[] };
                        if let Ok(pkt_len) =
                            pack_datagram(hdr, &primary[..2 + enc_len], f1, &[], &mut dgram)
                        {
                            for target in &targets {
                                let _ = socket.send_to(&dgram[..pkt_len], *target);
                            }
                        }

                        encode_fec_subframe(&i16_buf, &mut fec_n1);
                        has_fec = true;
                        seq = seq.wrapping_add(1);
                    } else {
                        std::thread::sleep(Duration::from_micros(800));
                    }
                }
            })
            .map_err(|e| e.to_string())?;

        Ok(Self {
            running,
            mic_input_ring,
            #[cfg(target_os = "android")]
            _mic_input_stream,
            handle: Some(handle),
        })
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        #[cfg(target_os = "android")]
        if let Some(stream) = self._mic_input_stream.take() {
            drop(stream);
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

pub fn android_start_stream(
    host_ip_port: &str,
    is_usb: bool,
    gain_db: f32,
    pin: Option<&str>,
) -> Result<(), String> {
    let addr: SocketAddr = host_ip_port
        .parse()
        .map_err(|e| format!("Invalid SocketAddr {}: {}", host_ip_port, e))?;

    let mut guard = GLOBAL_ANDROID_CLIENT.lock().unwrap();
    if let Some(mut existing) = guard.take() {
        existing.stop();
    }

    let cfg = ClientConfig {
        server_addr: addr,
        target_buffer_subframes: if is_usb { 2.0 } else { 3.5 },
        preamp_gain_db: gain_db,
    };

    let client = ClientEngine::connect_with_pin(cfg, pin)?;
    *guard = Some(client);
    Ok(())
}

pub fn android_stop_stream() {
    if let Some(mut client) = GLOBAL_ANDROID_CLIENT.lock().unwrap().take() {
        client.stop();
    }
    android_stop_phone_mic();
}

pub fn android_stop_phone_mic() {
    if let Some(mut mic) = GLOBAL_PHONE_MIC_TX.lock().unwrap().take() {
        mic.stop();
    }
}

pub fn android_set_gain_db(gain_db: f32) {
    if let Some(ref client) = *GLOBAL_ANDROID_CLIENT.lock().unwrap() {
        client
            .telemetry
            .preamp_gain_db_tenths
            .store((gain_db * 10.0) as i32, Ordering::Relaxed);
    }
}

pub fn android_report_bt_delay_ms(delay_ms: u16) {
    if let Some(ref client) = *GLOBAL_ANDROID_CLIENT.lock().unwrap() {
        client
            .telemetry
            .downstream_bt_delay_ms
            .store(delay_ms, Ordering::Relaxed);
    }
}

pub fn android_send_media_command(cmd_code: u8) {
    if let Some(ref client) = *GLOBAL_ANDROID_CLIENT.lock().unwrap() {
        client
            .telemetry
            .pending_media_command
            .store(cmd_code, Ordering::Relaxed);
    }
}

pub fn android_configure_dsp(
    bass_db: f32,
    pres_db: f32,
    treb_db: f32,
    role: u8,
    crossfeed: bool,
    night_mode: bool,
) {
    if let Some(ref client) = *GLOBAL_ANDROID_CLIENT.lock().unwrap() {
        client
            .telemetry
            .eq_bass_db_tenths
            .store((bass_db * 10.0) as i32, Ordering::Relaxed);
        client
            .telemetry
            .eq_presence_db_tenths
            .store((pres_db * 10.0) as i32, Ordering::Relaxed);
        client
            .telemetry
            .eq_treble_db_tenths
            .store((treb_db * 10.0) as i32, Ordering::Relaxed);
        client.telemetry.channel_role.store(role, Ordering::Relaxed);
        client
            .telemetry
            .enable_binaural_crossfeed
            .store(crossfeed, Ordering::Relaxed);
        client
            .telemetry
            .enable_night_mode
            .store(night_mode, Ordering::Relaxed);
    }
}

/// Returns newline-delimited list of auto-discovered hosts: "Hostname|IP:Port|USB_or_WiFi"
pub fn android_get_discovered_hosts_csv() -> String {
    let hosts = get_or_start_scanner().discovered_hosts();
    let mut out = String::new();
    for h in hosts {
        let mode = if h.is_usb { "USB" } else { "WiFi" };
        out.push_str(&format!("{}|{}|{}\n", h.hostname, h.addr, mode));
    }
    out
}

#[cfg(target_os = "android")]
pub mod android_native {
    use super::*;
    use jni::objects::{JClass, JString};
    use jni::sys::{jboolean, jfloat, jint, jlongArray, jstring};
    use jni::JNIEnv;

    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeGetDiscoveredHosts(
        env: JNIEnv,
        _class: JClass,
    ) -> jstring {
        let csv = android_get_discovered_hosts_csv();
        env.new_string(csv).unwrap().into_raw()
    }

    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeStartClient(
        mut env: JNIEnv,
        _class: JClass,
        host_addr: JString,
        is_usb: jboolean,
        gain_db: jfloat,
        pin: JString,
    ) -> jboolean {
        let Ok(addr_str) = env.get_string(&host_addr) else {
            return 0;
        };
        let addr_rust: String = addr_str.into();
        let pin_rust: String = env
            .get_string(&pin)
            .map(|value| value.into())
            .unwrap_or_default();
        let pin_opt = if pin_rust.is_empty() {
            None
        } else {
            Some(pin_rust.as_str())
        };
        match android_start_stream(&addr_rust, is_usb != 0, gain_db, pin_opt) {
            Ok(()) => 1,
            Err(_) => 0,
        }
    }

    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeStopClient(
        _env: JNIEnv,
        _class: JClass,
    ) {
        android_stop_stream();
    }

    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeSetGainDb(
        _env: JNIEnv,
        _class: JClass,
        gain_db: jfloat,
    ) {
        android_set_gain_db(gain_db);
    }

    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeReportBluetoothDelayMs(
        _env: JNIEnv,
        _class: JClass,
        delay_ms: jint,
    ) {
        android_report_bt_delay_ms(delay_ms.max(0) as u16);
    }

    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeSendMediaCommand(
        _env: JNIEnv,
        _class: JClass,
        cmd_code: jint,
    ) {
        android_send_media_command(cmd_code as u8);
    }

    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeConfigureDsp(
        _env: JNIEnv,
        _class: JClass,
        bass_db: jfloat,
        pres_db: jfloat,
        treb_db: jfloat,
        role: jint,
        crossfeed: jboolean,
        night_mode: jboolean,
    ) {
        android_configure_dsp(
            bass_db,
            pres_db,
            treb_db,
            role as u8,
            crossfeed != 0,
            night_mode != 0,
        );
    }

    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativePushCapturedAudio(
        env: JNIEnv,
        _class: JClass,
        pcm_float: jni::objects::JFloatArray,
    ) {
        let mut buf = [0.0f32; SUBFRAME_STEREO_SAMPLES];
        if env.get_float_array_region(&pcm_float, 0, &mut buf).is_ok() {
            if let Some(ref tx) = *GLOBAL_PHONE_MIC_TX.lock().unwrap() {
                tx.mic_input_ring.push_samples(&buf);
            }
        }
    }

    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeStartPhoneMic(
        mut env: JNIEnv,
        _class: JClass,
        host_ip: JString,
        use_hardware_mic: jboolean,
    ) -> jboolean {
        let Ok(ip_str) = env.get_string(&host_ip) else {
            return 0;
        };
        let ip_rust: String = ip_str.into();
        android_stop_phone_mic();
        if let Ok(tx) = PhoneMicTransmitter::start(&ip_rust, use_hardware_mic != 0) {
            *GLOBAL_PHONE_MIC_TX.lock().unwrap() = Some(tx);
            1
        } else {
            0
        }
    }

    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeStopPhoneMic(
        _env: JNIEnv,
        _class: JClass,
    ) {
        android_stop_phone_mic();
    }

    #[no_mangle]
    pub extern "system" fn Java_com_audioshare_ultra_service_AudioEngineService_nativeGetTelemetry(
        env: JNIEnv,
        _class: JClass,
    ) -> jlongArray {
        let mut stats = [0i64; 5];
        if let Some(ref c) = *GLOBAL_ANDROID_CLIENT.lock().unwrap() {
            stats[0] = c.telemetry.packets_received.load(Ordering::Relaxed) as i64;
            stats[1] = c.telemetry.primary_frames_played.load(Ordering::Relaxed) as i64;
            stats[2] = c.telemetry.fec_healed_frames.load(Ordering::Relaxed) as i64;
            stats[3] = c.telemetry.plc_extrapolated_frames.load(Ordering::Relaxed) as i64;
            stats[4] = c.telemetry.clock_drift_ppm.load(Ordering::Relaxed) as i64;
        }
        let arr = env.new_long_array(5).unwrap();
        let _ = env.set_long_array_region(&arr, 0, &stats);
        arr.into_raw()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_target_endpoints_party_mode() {
        let input = "192.168.1.100, 192.168.1.101:65532; 10.0.0.5  192.168.1.100";
        let addrs = parse_target_endpoints(input);
        assert_eq!(addrs.len(), 3);
        assert_eq!(addrs[0], "192.168.1.100:65532".parse().unwrap());
        assert_eq!(addrs[1], "192.168.1.101:65532".parse().unwrap());
        assert_eq!(addrs[2], "10.0.0.5:65532".parse().unwrap());
    }

    #[test]
    fn test_client_volume_feedback_roundtrip() {
        use audioshare_protocol::wire::{ClientFeedbackPacket, CONTROL_FEEDBACK_LEN};
        let fb = ClientFeedbackPacket {
            fec_rate_permille: 45,
            bluetooth_delay_ms: 120,
            client_volume: 85,
            requested_mode: 0,
            auth_mac32: 0,
        };
        let mut bytes = [0u8; CONTROL_FEEDBACK_LEN];
        fb.encode(&mut bytes);
        let decoded = ClientFeedbackPacket::decode(&bytes).expect("Failed to decode feedback");
        assert_eq!(decoded.client_volume, 85);
        assert_eq!(decoded.fec_rate_permille, 45);
        assert_eq!(decoded.bluetooth_delay_ms, 120);
    }

    #[test]
    fn test_phone_mic_transmitter_no_hardware_mic_start_stop() {
        let tx = PhoneMicTransmitter::start("127.0.0.1", false);
        assert!(tx.is_ok());
        let mut tx = tx.unwrap();
        tx.stop();
    }
}
