//! Host lifecycle simulations: repeated start/stop, kick/reconnect and
//! telemetry hygiene across PIN changes.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use audioshare_host_engine::{HostConfig, HostEngine};
use audioshare_protocol::crypto::{SessionKey, AEAD_TAG_LEN};
use audioshare_protocol::wire::{ClientFeedbackPacket, CONTROL_FEEDBACK_LEN};

fn start_host() -> (HostEngine, u16) {
    for _ in 0..32 {
        let probe = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let config = HostConfig {
            bind_port: port,
            use_test_tone: true,
            subframes_per_packet: 1,
            enable_smart_dtx: false,
        };
        if let Ok(engine) = HostEngine::start(config, vec![]) {
            return (engine, port);
        }
    }
    panic!("no free UDP port for test host");
}

fn send_feedback(sock: &UdpSocket, dest: SocketAddr, key: &SessionKey, seq: u32) {
    let packet = ClientFeedbackPacket {
        fec_rate_permille: 0,
        bluetooth_delay_ms: 0,
        client_volume: 100,
        requested_mode: 21,
        hardware_delay_ms: 0,
        feedback_seq: seq,
        auth_tag: [0u8; AEAD_TAG_LEN],
    };
    let mut buf = [0u8; CONTROL_FEEDBACK_LEN];
    packet.encode_signed(Some(key), &mut buf);
    sock.send_to(&buf, dest).unwrap();
}

#[test]
fn repeated_host_start_stop_does_not_leak_or_exhaust_ports() {
    for i in 0..20 {
        let (mut host, _port) = start_host();
        assert_eq!(host.connected_clients().len(), 0, "iteration {i}");
        host.stop();
    }
}

#[test]
fn kicked_peer_can_reconnect_at_feedback_seq_one() {
    let (mut host, port) = start_host();
    host.set_encryption_pin(Some("4829"));
    let key = SessionKey::from_pin("4829");

    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
    client
        .set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    let addr = client.local_addr().unwrap();
    let dest = SocketAddr::from(([127, 0, 0, 1], port));

    send_feedback(&client, dest, &key, 500);
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline
        && !host.connected_clients().iter().any(|c| c.addr == addr)
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        host.connected_clients().iter().any(|c| c.addr == addr),
        "first feedback did not register the client"
    );

    host.kick_client(addr);
    assert!(host.connected_clients().is_empty(), "kick failed");

    // Replay high-water mark lived on the removed client entry, so seq 1 must
    // be accepted for the fresh session.
    send_feedback(&client, dest, &key, 1);
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline
        && !host.connected_clients().iter().any(|c| c.addr == addr)
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        host.connected_clients().iter().any(|c| c.addr == addr),
        "seq=1 reconnect was rejected as a replay"
    );
    host.stop();
}

#[test]
fn changing_pin_clears_stale_rejection_counter() {
    let (mut host, _port) = start_host();
    host.set_encryption_pin(Some("1111"));
    host.telemetry
        .rejected_handshakes
        .store(7, Ordering::Relaxed);
    host.set_encryption_pin(Some("2222"));
    assert_eq!(
        host.telemetry.rejected_handshakes.load(Ordering::Relaxed),
        0,
        "stale firewall/PIN warning would stay visible forever"
    );
    host.stop();
}
