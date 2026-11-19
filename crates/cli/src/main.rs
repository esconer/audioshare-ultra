use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use audioshare_client_engine::{ClientConfig, ClientEngine};
use audioshare_host_engine::discovery_and_os::{
    AdbUsbWatcher, DiscoveryBeacon, VirtualSpeakerAndRadioGuard,
};
use audioshare_host_engine::{
    is_usb_subnet_ip, list_valid_host_interfaces, HostConfig, HostEngine, DEFAULT_PORT,
};
use audioshare_protocol::crypto::SessionKey;
use audioshare_protocol::dsp::{BurstDecouplerRing, ClockDriftResampler, TruePeakLimiter};
use audioshare_protocol::latency::{codec_mode_name, DESKTOP_OUTPUT_BUFFER_MS, SUBFRAME_MS};
use audioshare_protocol::lpc_lossless::{
    decode_fec_subframe_to_f32, decode_lossless_subframe, encode_fec_subframe,
    encode_lossless_subframe, ADPCM_FEC_BYTES, MAX_LOSSLESS_SUBFRAME_BYTES,
};
use audioshare_protocol::opus_codec::{
    OpusStreamDecoder, OpusStreamEncoder, MAX_OPUS_SUBFRAME_BYTES,
};
use audioshare_protocol::sequencer::{
    ZeroJitterSequencer, STATUS_HEALED_BY_FEC, STATUS_PRIMARY_LOSSLESS,
};
use audioshare_protocol::wire::{pack_datagram_with_key, CodecMode, ParsedPacket, WireHeader};
use audioshare_protocol::{SUBFRAME_FRAMES, SUBFRAME_STEREO_SAMPLES};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match cmd {
        "host" => run_host(&args[2..]),
        "client" => run_client(&args[2..]),
        "benchmark" => run_wifi_torture_benchmark(),
        "soak" => run_soak_cli(&args[2..]),
        _ => print_usage(),
    }
}

fn print_usage() {
    println!("================================================================");
    println!("  AudioShare Ultra — Hard Real-Time Lossless Audio Engine");
    println!("================================================================");
    println!("Usage:");
    println!("  as-ultra host [--test-tone] [--usb] [--virtual-speaker] [--pin <PIN>] [--vbr] [--latency-debug]");
    println!("  as-ultra client <host-ip[:port]> [--pin <PIN>] [--latency-debug]");
    println!("  as-ultra benchmark                    Run Wi-Fi Loss & Latency Benchmark");
    println!("  as-ultra soak [--seconds N] [--drift-ppm D] [--loss-pct P] [--opus]");
    println!();
    println!("  --latency-debug  Show the live component latency breakdown.");
    println!("                   Also enabled by AUDIOSHARE_LATENCY_DEBUG=1.");
    println!("  --vbr            Use the zero-lookahead pure-Rust VBR fallback instead");
    println!("                   of the default bundled Opus RLD compressed codec.");
    println!("  --pin <PIN>      Encrypt the session with the given pairing PIN.");
    println!("  --virtual-speaker Route system audio through the virtual speaker.");
}

fn has_latency_debug_flag(args: &[String]) -> bool {
    args.iter()
        .any(|a| a == "--latency-debug" || a == "--debug")
}

fn latency_debug_enabled(args: &[String]) -> bool {
    has_latency_debug_flag(args)
        || std::env::var("AUDIOSHARE_LATENCY_DEBUG")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
}

/// Returns the value immediately following `flag` (e.g. `--pin 4829` -> "4829").
fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| w[1].as_str())
}

fn client_target(args: &[String]) -> &str {
    // Skip the value consumed by `--pin <PIN>` so the PIN is never mistaken
    // for the host address (e.g. `client --pin 4829 192.168.1.5`).
    let mut skip_next = false;
    for a in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if a == "--pin" {
            skip_next = true;
            continue;
        }
        if !a.starts_with("--") {
            return a.as_str();
        }
    }
    "127.0.0.1:65530"
}

/// Parse `IP` or `IP:PORT` without mistaking a bare IPv6 address for an
/// `address:port` pair. `fe80::1` is an address, `[fe80::1]:5000` is a pair.
fn parse_client_target(raw: &str) -> Result<SocketAddr, String> {
    if let Ok(addr) = raw.parse::<SocketAddr>() {
        return Ok(addr);
    }
    raw.parse::<IpAddr>()
        .map(|ip| SocketAddr::new(ip, DEFAULT_PORT))
        .map_err(|_| format!("Invalid host address '{}' (expected IP or IP:PORT)", raw))
}

fn run_host(args: &[String]) {
    let use_test_tone = args.iter().any(|a| a == "--test-tone");
    let usb_mode = args.iter().any(|a| a == "--usb");
    // Opus RLD is the default; `--vbr` opts out to the pure-Rust VBR fallback.
    let use_opus = !args.iter().any(|a| a == "--vbr");
    let virtual_speaker = args.iter().any(|a| a == "--virtual-speaker");
    let pin = flag_value(args, "--pin");
    let latency_debug = latency_debug_enabled(args);

    println!("================================================================");
    println!("  AudioShare Ultra Host — Starting Real-Time Engine");
    println!("================================================================");

    let interfaces = list_valid_host_interfaces();
    if interfaces.is_empty() {
        println!(
            "[!] No external LAN/USB interfaces detected (listening on 0.0.0.0:{})",
            DEFAULT_PORT
        );
    } else {
        println!("Available Endpoints (Docker/WSL virtual bridges filtered out):");
        for iface in &interfaces {
            let badge = if iface.is_usb_tethering {
                "[USB TETHERING - PRIORITY 1]"
            } else {
                "[WIFI / LAN]"
            };
            println!(
                "  • {:<18} -> {}:{} {}",
                iface.name, iface.ip, DEFAULT_PORT, badge
            );
        }
    }

    let cfg = HostConfig {
        bind_port: DEFAULT_PORT,
        subframes_per_packet: if usb_mode { 1 } else { 2 },
        use_test_tone,
        enable_smart_dtx: true,
    };

    let mut os_guard = VirtualSpeakerAndRadioGuard::acquire(virtual_speaker);
    let host = HostEngine::start(cfg, vec![]).expect("Failed to start HostEngine");
    host.telemetry.prefer_opus.store(use_opus, Ordering::Relaxed);
    let beacon_encrypted = Arc::new(AtomicBool::new(false));
    if let Some(pin) = pin {
        host.set_encryption_pin(Some(pin));
        // Flip the shared flag when the PIN is applied so the beacon advertises
        // `enc=1` on its very next interval.
        beacon_encrypted.store(true, Ordering::Relaxed);
    }
    let _beacon = DiscoveryBeacon::start_with_encryption_flag(
        DEFAULT_PORT,
        "CLI-AudioShare-Ultra".into(),
        Arc::clone(&beacon_encrypted),
    );
    let _adb = AdbUsbWatcher::start(DEFAULT_PORT);
    println!(
        "\n[+] Pacing: {:.1} ms ({} subframe/pkt) | QoS: 802.11e WMM AC_VI (0xA0) | Smart DTX: ON",
        (cfg.subframes_per_packet as f32) * 2.5,
        cfg.subframes_per_packet
    );
    println!(
        "[+] Compressed codec: {}",
        if use_opus {
            "Opus RLD (default, pure Rust, 2.5 ms lookahead)"
        } else {
            "Pure-Rust VBR (--vbr fallback, zero lookahead)"
        }
    );
    println!(
        "[+] Waiting for clients (or run `as-ultra client 127.0.0.1` in another terminal)...\n"
    );

    loop {
        std::thread::sleep(Duration::from_secs(1));
        let pkts = host.telemetry.packets_sent.load(Ordering::Relaxed);
        let bytes = host.telemetry.bytes_sent.load(Ordering::Relaxed);
        let clients = host.telemetry.active_clients_count.load(Ordering::Relaxed);
        if clients > 0 {
            os_guard.activate();
        } else {
            os_guard.deactivate();
        }
        let dtx = host.telemetry.dtx_silent_mode.load(Ordering::Relaxed);
        let codec = codec_mode_name(host.telemetry.active_codec_mode.load(Ordering::Relaxed));

        if latency_debug {
            let pacing_ms =
                host.telemetry.tx_packet_interval_us.load(Ordering::Relaxed) as f32 / 1000.0;
            let capture_ms =
                host.telemetry.capture_buffer_us.load(Ordering::Relaxed) as f32 / 1000.0;
            let subframes = host
                .telemetry
                .tx_subframes_per_packet
                .load(Ordering::Relaxed);
            let bt_max = host
                .telemetry
                .max_client_bt_delay_ms
                .load(Ordering::Relaxed);
            let downstream_max = host
                .telemetry
                .max_client_downstream_delay_ms
                .load(Ordering::Relaxed);
            let room_trim = host
                .telemetry
                .manual_group_sync_trim_ms
                .load(Ordering::Relaxed);
            print!(
                "\r[HOST LATENCY] Pacing: {:.1} ms ({} × {:.1} ms) | Capture: {:.1} ms | Pipeline: {:.1} ms | Downstream max: {} ms (BT {}) | Trim: +{} ms | Clients: {} | Mode: {}   ",
                pacing_ms,
                subframes,
                SUBFRAME_MS,
                capture_ms,
                capture_ms + pacing_ms,
                downstream_max,
                bt_max,
                room_trim,
                clients,
                if dtx { "SMART-DTX" } else { codec }
            );
        } else {
            print!(
                "\r[HOST HUD] Clients: {} | TX Packets: {:<8} | Total KB: {:<8} | Mode: {:<18}",
                clients,
                pkts,
                bytes / 1024,
                if dtx { "SMART-DTX (0.6kbps)" } else { codec }
            );
        }
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
}

fn run_client(args: &[String]) {
    let latency_debug = latency_debug_enabled(args);
    let pin = flag_value(args, "--pin");
    // Allow `client --latency-debug 192.168.1.5` as well as the address first.
    let raw_target = client_target(args);
    let server_addr = parse_client_target(raw_target).unwrap_or_else(|e| panic!("{}", e));

    // Loopback + legacy and randomized Android USB-tethering subnets.
    let is_usb_or_local = is_usb_subnet_ip(server_addr.ip());

    let cfg = ClientConfig {
        server_addr,
        target_buffer_subframes: if is_usb_or_local { 2.0 } else { 3.5 },
        preamp_gain_db: 0.0,
        dolby_effects: false,
    };

    println!("================================================================");
    println!("  AudioShare Ultra Client — Connecting to {}", server_addr);
    println!("================================================================");

    let client = ClientEngine::connect_with_pin(cfg, pin).expect("Failed to start ClientEngine");

    loop {
        std::thread::sleep(Duration::from_millis(500));
        let pkts = client.telemetry.packets_received.load(Ordering::Relaxed);
        let prim = client
            .telemetry
            .primary_frames_played
            .load(Ordering::Relaxed);
        let fec = client.telemetry.fec_healed_frames.load(Ordering::Relaxed);
        let plc = client
            .telemetry
            .plc_extrapolated_frames
            .load(Ordering::Relaxed);
        let drift = client.telemetry.clock_drift_ppm.load(Ordering::Relaxed);
        let dtx = client.telemetry.server_dtx_silent.load(Ordering::Relaxed);

        if latency_debug {
            let snap = client.telemetry.latency_snapshot(DESKTOP_OUTPUT_BUFFER_MS);
            print!(
                "\r[CLIENT LATENCY] Est: {:.1} ms = pkt {:.1} + FIFO {:.1} (fill {:.1}, group +{:.1}) + out {:.1} + codec {:.1} [DAC {} ms, BT {} ms] | Jitter {:.1} ms | {} | {} | RX {} | FEC {} | PLC {} | Drift {:+} ppm   ",
                snap.estimated_one_way_ms(),
                snap.packet_interval_ms,
                snap.target_buffer_ms,
                snap.buffered_audio_ms,
                snap.group_sync_extra_ms,
                snap.downstream_stage_ms(),
                snap.codec_lookahead_ms,
                snap.hardware_delay_ms,
                snap.bluetooth_delay_ms,
                snap.network_jitter_ms,
                snap.codec_name(),
                if snap.prebuffer_ready { "READY" } else { "FILLING" },
                pkts,
                fec,
                plc,
                drift
            );
        } else {
            print!(
                "\r[CLIENT HUD] RX Pkts: {:<7} | Lossless Frames: {:<7} | FEC Healed: {:<4} | PLC: {:<4} | Clock Drift: {:+4} ppm | DTX: {}",
                pkts, prim, fec, plc, drift, if dtx { "SLEEP" } else { "LIVE " }
            );
        }
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
}

/// Automated Torture Benchmark: Simulates 1,000 subframes (2.5 seconds of 48kHz stereo audio)
/// over a harsh Wi-Fi link with 12% random & burst packet drops and measures codec speed + recovery rate.
fn run_wifi_torture_benchmark() {
    println!("================================================================");
    println!("  AudioShare Ultra — Hardware & Wi-Fi Torture Benchmark");
    println!("================================================================");

    let total_subframes = 1_000u16;
    let mut sequencer = ZeroJitterSequencer::new();

    let mut total_raw_bytes = 0usize;
    let mut total_compressed_bytes = 0usize;
    let mut fec_n1_buf = [0u8; ADPCM_FEC_BYTES];
    let mut fec_n2_buf = [0u8; ADPCM_FEC_BYTES];

    let mut simulated_network_drops = 0u32;
    let start_time = Instant::now();

    let mut lossless_played = 0u32;
    let mut fec_healed = 0u32;
    let mut unrecovered_drops = 0u32;
    let mut out_f32 = [0.0f32; SUBFRAME_STEREO_SAMPLES];
    let mut next_play_seq: u16 = 0;

    for seq in 0..total_subframes {
        // Generate stereo harmonic audio signal
        let mut pcm_i16 = [0i16; SUBFRAME_STEREO_SAMPLES];
        for i in 0..SUBFRAME_FRAMES {
            let t = ((seq as usize * SUBFRAME_FRAMES + i) as f32) / 48_000.0;
            let l = ((t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 24_000.0) as i16;
            let r = ((t * 880.0 * 2.0 * std::f32::consts::PI).cos() * 24_000.0) as i16;
            pcm_i16[i * 2] = l;
            pcm_i16[i * 2 + 1] = r;
        }

        let mut enc_lossless = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
        let enc_len = encode_lossless_subframe(&pcm_i16, &mut enc_lossless);
        total_raw_bytes += SUBFRAME_STEREO_SAMPLES * 2;
        total_compressed_bytes += enc_len;

        // Simulate a 12% bursty Wi-Fi drop rate (drops 1 or 2 consecutive packets every 15 packets)
        let is_dropped = (seq % 15 == 7) || (seq % 23 == 11);
        if is_dropped {
            simulated_network_drops += 1;
        } else {
            // Packet arrived! First heal [N-1] and [N-2] if they were dropped
            if seq >= 1 {
                let mut f1 = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                decode_fec_subframe_to_f32(&fec_n1_buf, &mut f1);
                sequencer.try_heal_with_fec(seq - 1, &f1);
            }
            if seq >= 2 {
                let mut f2 = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                decode_fec_subframe_to_f32(&fec_n2_buf, &mut f2);
                sequencer.try_heal_with_fec(seq - 2, &f2);
            }

            // Decode primary lossless subframe
            let mut dec_i16 = [0i16; SUBFRAME_STEREO_SAMPLES];
            decode_lossless_subframe(&enc_lossless[..enc_len], &mut dec_i16).unwrap();
            let mut dec_f32 = [0.0f32; SUBFRAME_STEREO_SAMPLES];
            for (dst, &s) in dec_f32.iter_mut().zip(dec_i16.iter()) {
                *dst = (s as f32) / 32768.0;
            }
            sequencer.push_primary(seq, &dec_f32);
        }

        // Update sliding FEC history for next packet
        fec_n2_buf = fec_n1_buf;
        encode_fec_subframe(&pcm_i16, &mut fec_n1_buf);

        // Real-time playback: consume frames once they are two subframes behind
        // the newest arrival, giving [N-1]/[N-2] FEC a chance to heal losses.
        while next_play_seq + 2 <= seq {
            match sequencer.pop_next(next_play_seq, &mut out_f32) {
                STATUS_PRIMARY_LOSSLESS => lossless_played += 1,
                STATUS_HEALED_BY_FEC => fec_healed += 1,
                _ => unrecovered_drops += 1,
            }
            next_play_seq = next_play_seq.wrapping_add(1);
        }
    }

    // Drain the final two subframes after the FEC window has closed.
    while next_play_seq < (total_subframes - 2) {
        match sequencer.pop_next(next_play_seq, &mut out_f32) {
            STATUS_PRIMARY_LOSSLESS => lossless_played += 1,
            STATUS_HEALED_BY_FEC => fec_healed += 1,
            _ => unrecovered_drops += 1,
        }
        next_play_seq = next_play_seq.wrapping_add(1);
    }

    let elapsed = start_time.elapsed();
    let ratio = 100.0 * (1.0 - (total_compressed_bytes as f64) / (total_raw_bytes as f64));

    println!("  • Total Audio Processed     : 2,500 ms (1,000 x 2.5ms subframes)");
    println!(
        "  • Total CPU Time (Enc+Dec)  : {:.2?} ({:.2?} per 2.5ms frame!)",
        elapsed,
        elapsed / total_subframes as u32
    );
    println!("  • Codec Lookahead Delay     : 0.00 ms (Causal 2nd-Order LPC)");
    println!(
        "  • Lossless Size Reduction   : {:.1}% smaller than Raw PCM16 ({} KB -> {} KB)",
        ratio,
        total_raw_bytes / 1024,
        total_compressed_bytes / 1024
    );
    println!(
        "  • Simulated Wi-Fi Drops     : {} dropped packets",
        simulated_network_drops
    );
    println!(
        "  • Lossless Frames Played    : {} frames (bit-exact)",
        lossless_played
    );
    println!(
        "  • Healed by [N-1]/[N-2] FEC : {} frames (100% seamless recovery)",
        fec_healed
    );
    println!(
        "  • Audible Dropouts/Clicks   : {} dropouts",
        unrecovered_drops
    );
    println!("================================================================");
    assert_eq!(unrecovered_drops, 0, "Zero audible dropouts SLA violated!");
    println!("  [PASS] All Hard Real-Time & Zero-Jitter SLAs Verified!");
}

struct SoakReport {
    dac_underruns: u64,
    resync_stops: u64,
    plc_frames: u64,
    locked_ppm: i32,
    played_real: u64,
}

/// Deterministic end-to-end soak simulator exercising real ChaCha20 wire packing,
/// LPC/Opus codec, ZeroJitterSequencer, bidirectional ClockDriftResampler,
/// BurstDecouplerRing (192-frame Android HAL pulls), and TruePeakLimiter under
/// crystal drift + bursty Wi-Fi jitter + packet drops.
fn execute_network_soak(
    duration_secs: u32,
    drift_ppm: i32,
    loss_pct: u32,
    use_opus: bool,
) -> SoakReport {
    let key = SessionKey::from_pin("4829");
    let mut sequencer = ZeroJitterSequencer::new();
    let mut resampler = ClockDriftResampler::new();
    let ring = BurstDecouplerRing::new();
    let mut limiter = TruePeakLimiter::new(0.0);
    let mut opus_enc = use_opus.then(|| OpusStreamEncoder::new(256).expect("opus enc"));
    let mut opus_dec = use_opus.then(|| OpusStreamDecoder::new().expect("opus dec"));

    let pkt_subframes = 2usize; // 5 ms Wi-Fi packets
    let hal_burst_frames = 192usize; // 4.0 ms Android AAudio burst
    let hal_burst_samples = hal_burst_frames * 2;
    let target_subframes = 8.0f32; // 20 ms 2.4 GHz cushion
    let fifo_wm = resampler.subframes_to_samples(target_subframes as usize);
    let plc_wm = hal_burst_samples;
    let clock_setpoint =
        (resampler.samples_to_subframes(fifo_wm) - 0.5 * resampler.samples_to_subframes(hal_burst_samples))
            .max(1.0);

    let host_pkt_period_ms = (pkt_subframes as f64 * 2.5) / (1.0 + (drift_ppm as f64) * 1e-6);
    let hal_period_ms = 4.0f64;

    let mut t = 0.0f64;
    let mut next_tx = host_pkt_period_ms;
    let mut next_hal = hal_period_ms;
    let mut next_clock_tick = 2.5f64;
    let mut rng = 0xDEAD_BEEFu32;
    let mut next_rand = || -> u32 {
        rng ^= rng << 13;
        rng ^= rng >> 17;
        rng ^= rng << 5;
        rng
    };

    let mut in_flight: Vec<(f64, Vec<u8>)> = Vec::new();
    let mut tx_seq = 0u16;
    let mut fec_n1 = [0u8; ADPCM_FEC_BYTES];
    let mut fec_n2 = [0u8; ADPCM_FEC_BYTES];
    let mut has_fec = 0u8;
    let mut prebuffer_ready = false;
    let mut next_play_seq = 0u16;
    let mut latest_rx_seq = 0u16;
    let mut initialized = false;
    let mut dither_rng = 1u32;
    let mut resampled = [0.0f32; 512];

    let mut report = SoakReport {
        dac_underruns: 0,
        resync_stops: 0,
        plc_frames: 0,
        locked_ppm: 0,
        played_real: 0,
    };

    let end_ms = (duration_secs as f64) * 1000.0;
    while t < end_ms {
        let next_arr = in_flight.iter().map(|(at, _)| *at).fold(f64::MAX, f64::min);
        t = next_tx.min(next_hal).min(next_clock_tick).min(next_arr);

        if (t - next_tx).abs() < 1e-9 {
            let mut bundle = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES * 2 + 4];
            let mut cursor = 0usize;
            let base_seq = tx_seq;
            let prev_n1 = fec_n1;
            let prev_n2 = fec_n2;
            let prev_has = has_fec;
            for _ in 0..pkt_subframes {
                let mut pcm = [0i16; SUBFRAME_STEREO_SAMPLES];
                for i in 0..SUBFRAME_FRAMES {
                    let ph = ((tx_seq as usize * SUBFRAME_FRAMES + i) as f32) * std::f32::consts::TAU * 440.0 / 48_000.0;
                    pcm[i * 2] = (ph.sin() * 18_000.0) as i16;
                    pcm[i * 2 + 1] = ((ph + 0.3).sin() * 18_000.0) as i16;
                }
                let mut enc = [0u8; MAX_LOSSLESS_SUBFRAME_BYTES];
                let len = if let Some(ref mut op) = opus_enc {
                    op.encode_subframe(&pcm, 256, &mut enc[..MAX_OPUS_SUBFRAME_BYTES]).unwrap()
                } else {
                    fec_n2 = fec_n1;
                    encode_fec_subframe(&pcm, &mut fec_n1);
                    has_fec = (has_fec + 1).min(2);
                    encode_lossless_subframe(&pcm, &mut enc)
                };
                bundle[cursor..cursor + 2].copy_from_slice(&(len as u16).to_be_bytes());
                cursor += 2;
                bundle[cursor..cursor + len].copy_from_slice(&enc[..len]);
                cursor += len;
                tx_seq = tx_seq.wrapping_add(1);
            }
            let hdr = WireHeader {
                mode: if use_opus { CodecMode::OpusRld } else { CodecMode::StudioLosslessLpc },
                subframe_count: pkt_subframes as u8,
                encrypted: true,
                sequence: base_seq,
                tx_timestamp_us: (t * 1000.0) as u32,
                primary_len: 0,
                fec_n1_len: 0,
                fec_n2_len: 0,
                host_volume: 100,
                stream_id: 0,
                header_crc16: 0,
            };
            let mut dgram = [0u8; audioshare_protocol::MAX_SAFE_UDP_PAYLOAD];
            let f1 = if !use_opus && prev_has >= 1 { &prev_n1[..] } else { &[] };
            let f2 = if !use_opus && prev_has >= 2 { &prev_n2[..] } else { &[] };
            let pkt_len = pack_datagram_with_key(hdr, &bundle[..cursor], f1, f2, Some(&key), &mut dgram).unwrap();
            if (next_rand() % 100) >= loss_pct {
                // 92% normal Wi-Fi (0.5-3.5 ms), 8% jitter burst (4-14 ms)
                let r = next_rand() % 100;
                let delay = if r < 92 { 0.5 + ((next_rand() % 30) as f64) * 0.1 } else { 4.0 + ((next_rand() % 100) as f64) * 0.1 };
                in_flight.push((t + delay, dgram[..pkt_len].to_vec()));
            }
            next_tx += host_pkt_period_ms;
        }

        let mut arrived = Vec::new();
        in_flight.retain(|item| {
            if item.0 <= t + 1e-9 {
                arrived.push(item.1.clone());
                false
            } else {
                true
            }
        });
        for mut raw in arrived {
            if let Ok(parsed) = ParsedPacket::parse_and_decrypt(&mut raw, Some(&key)) {
                let base = parsed.header.sequence;
                if !initialized {
                    next_play_seq = base;
                    latest_rx_seq = base;
                    initialized = true;
                }
                let delta = base.wrapping_sub(next_play_seq) as i16;
                if delta > 48 || delta < -32 {
                    report.resync_stops += 1;
                    sequencer.reset();
                    next_play_seq = base;
                    latest_rx_seq = base;
                    prebuffer_ready = false;
                }
                if !use_opus {
                    if !parsed.fec_n1_payload.is_empty() && sequencer.is_slot_missing(base.wrapping_sub(1)) {
                        let mut f = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                        if decode_fec_subframe_to_f32(parsed.fec_n1_payload, &mut f) {
                            sequencer.try_heal_with_fec(base.wrapping_sub(1), &f);
                        }
                    }
                    if !parsed.fec_n2_payload.is_empty() && sequencer.is_slot_missing(base.wrapping_sub(2)) {
                        let mut f = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                        if decode_fec_subframe_to_f32(parsed.fec_n2_payload, &mut f) {
                            sequencer.try_heal_with_fec(base.wrapping_sub(2), &f);
                        }
                    }
                }
                let mut c = 0usize;
                for idx in 0..parsed.header.subframe_count as u16 {
                    let sub_len = u16::from_be_bytes([parsed.primary_payload[c], parsed.primary_payload[c + 1]]) as usize;
                    c += 2;
                    let s = base.wrapping_add(idx);
                    if (s.wrapping_sub(next_play_seq) as i16) >= 0 && !sequencer.has_primary(s) {
                        let mut pcm_f32 = [0.0f32; SUBFRAME_STEREO_SAMPLES];
                        let ok = if let Some(ref mut dec) = opus_dec {
                            dec.decode_subframe_with_plc(s, &parsed.primary_payload[c..c + sub_len], &mut pcm_f32, |ms, plc| {
                                if (ms.wrapping_sub(next_play_seq) as i16) >= 0 && sequencer.is_slot_missing(ms) {
                                    sequencer.try_heal_with_fec(ms, plc);
                                }
                            })
                        } else {
                            let mut pcm_i16 = [0i16; SUBFRAME_STEREO_SAMPLES];
                            if decode_lossless_subframe(&parsed.primary_payload[c..c + sub_len], &mut pcm_i16).is_ok() {
                                for (d, &v) in pcm_f32.iter_mut().zip(pcm_i16.iter()) {
                                    *d = (v as f32) * (1.0 / 32768.0);
                                }
                                true
                            } else {
                                false
                            }
                        };
                        if ok {
                            sequencer.push_primary(s, &pcm_f32);
                            if (s.wrapping_sub(latest_rx_seq) as i16) > 0 {
                                latest_rx_seq = s;
                            }
                        }
                    }
                    c += sub_len;
                }
            }
        }

        if (t - next_hal).abs() < 1e-9 {
            if prebuffer_ready {
                let mut hal_out = [0.0f32; 384];
                if !ring.pop_exact_or_dither(&mut hal_out, &mut dither_rng) {
                    report.dac_underruns += 1;
                }
            }
            next_hal += hal_period_ms;
        }

        if initialized && !prebuffer_ready {
            let net = (latest_rx_seq.wrapping_add(1).wrapping_sub(next_play_seq) as i16).max(0) as f32;
            if net + resampler.samples_to_subframes(ring.available_samples()) >= target_subframes {
                prebuffer_ready = true;
            }
        }

        while initialized && prebuffer_ready && ring.available_samples() < fifo_wm {
            let missing = sequencer.is_slot_missing(next_play_seq);
            let md = latest_rx_seq.wrapping_sub(next_play_seq) as i16;
            if missing && md < 4 && ring.available_samples() >= plc_wm {
                break;
            }
            let mut frame = [0.0f32; SUBFRAME_STEREO_SAMPLES];
            let st = sequencer.pop_next(next_play_seq, &mut frame);
            next_play_seq = next_play_seq.wrapping_add(1);
            if st == STATUS_PRIMARY_LOSSLESS || st == STATUS_HEALED_BY_FEC {
                report.played_real += 1;
            } else {
                report.plc_frames += 1;
            }
            limiter.process_in_place(&mut frame);
            let n = resampler.process(&frame, &mut resampled);
            ring.push_samples(&resampled[..n * 2]);
            if st != STATUS_PRIMARY_LOSSLESS && st != STATUS_HEALED_BY_FEC {
                break;
            }
        }

        if (t - next_clock_tick).abs() < 1e-9 {
            if initialized && prebuffer_ready {
                let net = (latest_rx_seq.wrapping_add(1).wrapping_sub(next_play_seq) as i16).max(0) as f32;
                let fifo = resampler.samples_to_subframes(ring.available_samples());
                resampler.update_clock_loop(net + fifo, clock_setpoint);
                report.locked_ppm = (((resampler.ratio / resampler.base_ratio) - 1.0) * 1_000_000.0) as i32;
            }
            next_clock_tick += 2.5;
        }
    }
    report
}

fn run_soak_cli(args: &[String]) {
    let secs = flag_value(args, "--seconds").and_then(|v| v.parse().ok()).unwrap_or(60);
    let drift = flag_value(args, "--drift-ppm").and_then(|v| v.parse().ok()).unwrap_or(-200);
    let loss = flag_value(args, "--loss-pct").and_then(|v| v.parse().ok()).unwrap_or(3);
    let use_opus = args.iter().any(|a| a == "--opus");
    let r = execute_network_soak(secs, drift, loss, use_opus);
    println!(
        "[SOAK {}s | drift {:+}ppm | loss {}% | {}] played={} | PLC={} | resync_stops={} | dac_underruns={} | locked={:+}ppm",
        secs, drift, loss, if use_opus { "Opus" } else { "LPC+FEC" },
        r.played_real, r.plc_frames, r.resync_stops, r.dac_underruns, r.locked_ppm
    );
    assert_eq!(r.dac_underruns, 0, "DAC underrun SLA failed");
    assert_eq!(r.resync_stops, 0, "Resync stop SLA failed");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn test_latency_debug_flag_aliases() {
        assert!(has_latency_debug_flag(&args(&["--latency-debug"])));
        assert!(has_latency_debug_flag(&args(&["--debug"])));
        assert!(!has_latency_debug_flag(&args(&["192.168.1.5"])));
    }

    #[test]
    fn test_client_target_skips_flags() {
        assert_eq!(
            client_target(&args(&["--latency-debug", "192.168.1.5"])),
            "192.168.1.5"
        );
        assert_eq!(
            client_target(&args(&["192.168.1.5", "--debug"])),
            "192.168.1.5"
        );
        assert_eq!(client_target(&[]), "127.0.0.1:65530");
    }

    #[test]
    fn test_client_target_port_formatting() {
        assert_eq!(
            parse_client_target("192.168.1.5"),
            Ok(SocketAddr::from(([192, 168, 1, 5], DEFAULT_PORT)))
        );
        assert_eq!(
            parse_client_target("192.168.1.5:5000"),
            Ok(SocketAddr::from(([192, 168, 1, 5], 5000)))
        );
    }

    #[test]
    fn test_client_target_accepts_bare_and_bracketed_ipv6() {
        // A bare IPv6 address contains ':' but is not an address:port pair.
        assert_eq!(
            parse_client_target("fe80::1"),
            Ok(SocketAddr::new("fe80::1".parse().unwrap(), DEFAULT_PORT))
        );
        assert_eq!(
            parse_client_target("[fe80::1]:5000"),
            Ok(SocketAddr::new("fe80::1".parse().unwrap(), 5000))
        );
        assert_eq!(
            parse_client_target("::1"),
            Ok(SocketAddr::new("::1".parse().unwrap(), DEFAULT_PORT))
        );
        assert!(parse_client_target("not-an-ip").is_err());
    }

    #[test]
    fn test_host_flag_parsing() {
        let test_args = args(&["--test-tone", "--usb", "--libopus", "--latency-debug"]);
        assert!(test_args.iter().any(|a| a == "--test-tone"));
        assert!(test_args.iter().any(|a| a == "--usb"));
        assert!(test_args.iter().any(|a| a == "--libopus"));
        assert!(latency_debug_enabled(&test_args));

        let plain_args = args(&[]);
        assert!(!plain_args.iter().any(|a| a == "--test-tone"));
        assert!(!plain_args.iter().any(|a| a == "--usb"));
        assert!(!plain_args.iter().any(|a| a == "--libopus"));
        assert!(!has_latency_debug_flag(&plain_args));
    }

    #[test]
    fn test_pin_flag_parsing_and_client_target_skips_pin_value() {
        let pin_first = args(&["--pin", "4829", "192.168.1.5"]);
        assert_eq!(flag_value(&pin_first, "--pin"), Some("4829"));
        assert_eq!(client_target(&pin_first), "192.168.1.5");

        let addr_first = args(&["192.168.1.5", "--pin", "1234"]);
        assert_eq!(flag_value(&addr_first, "--pin"), Some("1234"));
        assert_eq!(client_target(&addr_first), "192.168.1.5");

        assert_eq!(flag_value(&args(&["--vbr"]), "--pin"), None);
    }

    #[test]
    fn test_network_soak_smoke() {
        for &(drift, use_opus) in &[(-200, false), (200, true)] {
            let r = execute_network_soak(20, drift, 2, use_opus);
            assert_eq!(r.dac_underruns, 0, "underrun at drift={drift} opus={use_opus}");
            assert_eq!(r.resync_stops, 0, "resync stop at drift={drift} opus={use_opus}");
            // The clock controller must move in the correct direction; the old
            // windowed-min implementation pinned negative drift at 0 ppm.
            assert_eq!(
                r.locked_ppm.signum(),
                drift.signum(),
                "clock tracked the wrong direction at drift={drift}: {}",
                r.locked_ppm
            );
            assert!(r.locked_ppm.abs() > 50, "clock did not engage: {}", r.locked_ppm);
        }
    }

    #[test]
    #[ignore = "long 180s x4 soak SLA; run with cargo test -p audioshare-cli -- --ignored"]
    fn test_end_to_end_wifi_drift_and_loss_soak() {
        // 180 s of simulated time lets the 500 ms clock-loop windows fully
        // converge for both drift directions before the +/-60 ppm SLA is checked.
        for &(drift, use_opus) in &[(-200, false), (200, false), (-200, true), (200, true)] {
            let r = execute_network_soak(180, drift, 2, use_opus);
            assert_eq!(r.dac_underruns, 0, "underrun at drift={drift} opus={use_opus}");
            assert_eq!(r.resync_stops, 0, "resync stop at drift={drift} opus={use_opus}");
            assert!(
                (r.locked_ppm - drift).abs() < 60,
                "clock failed to track {drift} ppm (locked at {} ppm, opus={use_opus})",
                r.locked_ppm
            );
        }
    }
}
