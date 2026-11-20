//! Adversarial protocol tests: authentication, replay, malformed input and
//! sequence wraparound. These exercise the security fixes from p4-C2, P1 and
//! Patch A without needing any hardware or network.

use audioshare_protocol::crypto::{SessionKey, AEAD_TAG_LEN};
use audioshare_protocol::sequencer::{ZeroJitterSequencer, STATUS_PRIMARY_LOSSLESS};
use audioshare_protocol::wire::{
    feedback_is_replay, ClientFeedbackPacket, ParsedPacket, WireHeader, CONTROL_FEEDBACK_LEN,
    HEADER_LEN,
};
use audioshare_protocol::SUBFRAME_STEREO_SAMPLES;

fn feedback(seq: u32, mode: u8) -> ClientFeedbackPacket {
    ClientFeedbackPacket {
        fec_rate_permille: 0,
        bluetooth_delay_ms: 0,
        client_volume: 100,
        requested_mode: mode,
        hardware_delay_ms: 0,
        feedback_seq: seq,
        auth_tag: [0u8; AEAD_TAG_LEN],
    }
}

fn signed_bytes(key: &SessionKey, packet: &ClientFeedbackPacket) -> [u8; CONTROL_FEEDBACK_LEN] {
    let mut buf = [0u8; CONTROL_FEEDBACK_LEN];
    packet.encode_signed(Some(key), &mut buf);
    buf
}

/// p4-C2: two phones joining with the same PIN both start at feedback_seq = 1.
/// With the old Poly1305 construction this reused the one-time auth key on
/// different bodies. HMAC-SHA256 must authenticate both independently.
#[test]
fn two_clients_same_pin_and_seq_authenticate_independently() {
    let pin = "4829";
    let key_a = SessionKey::from_pin(pin);
    let key_b = SessionKey::from_pin(pin);
    assert_eq!(key_a, key_b, "same PIN must derive the same authentication key");

    // Two phones, same PIN, same seq, different feedback bodies (VBR vs Opus).
    let tag_a = key_a.sign_feedback(&[0u8; 20], 1);
    let tag_b = key_b.sign_feedback(&[1u8; 20], 1);
    assert_ne!(tag_a, tag_b, "different bodies must produce different tags");

    // Each tag verifies against its own body under either client's key.
    assert!(key_a.verify_feedback(&[0u8; 20], 1, &tag_a));
    assert!(key_b.verify_feedback(&[0u8; 20], 1, &tag_a));
    assert!(key_a.verify_feedback(&[1u8; 20], 1, &tag_b));
    // ...and neither may be transplanted onto the other's body.
    assert!(!key_a.verify_feedback(&[1u8; 20], 1, &tag_a));
    assert!(!key_b.verify_feedback(&[0u8; 20], 1, &tag_b));
}

#[test]
fn feedback_tag_is_bound_to_sequence_and_body() {
    let key = SessionKey::from_pin("1234");
    let body = [7u8; 20];
    let tag = key.sign_feedback(&body, 42);

    assert!(key.verify_feedback(&body, 42, &tag));
    assert!(!key.verify_feedback(&body, 43, &tag), "seq must be bound");
    assert!(!key.verify_feedback(&[8u8; 20], 42, &tag), "body must be bound");

    let mut forged = tag;
    forged[0] ^= 0x01;
    assert!(!key.verify_feedback(&body, 42, &forged), "bit flip must fail");

    let wrong = SessionKey::from_pin("9999");
    assert!(!wrong.verify_feedback(&body, 42, &tag), "wrong PIN must fail");
}

#[test]
fn encoded_feedback_rejects_forged_and_wrong_pin() {
    let key = SessionKey::from_pin("1234");
    let good = signed_bytes(&key, &feedback(7, 20));

    assert!(ClientFeedbackPacket::decode_verified(&good, Some(&key)).is_some());

    let mut tampered = good;
    tampered[CONTROL_FEEDBACK_LEN - 1] ^= 0x80;
    assert!(ClientFeedbackPacket::decode_verified(&tampered, Some(&key)).is_none());

    let wrong = SessionKey::from_pin("9999");
    assert!(ClientFeedbackPacket::decode_verified(&good, Some(&wrong)).is_none());
}

/// P1 / Patch A: replay guard must reject older sequences and accept fresh
/// ones, including the seq=1 restart after a kick.
#[test]
fn replay_guard_accepts_fresh_and_rejects_stale_sequences() {
    assert!(!feedback_is_replay(true, 1, 0), "first seq=1 must be accepted");
    assert!(!feedback_is_replay(true, 5, 0), "higher first seq accepted");
    assert!(feedback_is_replay(true, 5, 5), "duplicate seq rejected");
    assert!(feedback_is_replay(true, 4, 5), "older seq rejected");
    assert!(!feedback_is_replay(true, 6, 5), "newer seq accepted");
    // Unauthenticated LAN/party mode is intentionally replay-tolerant.
    assert!(!feedback_is_replay(false, 1, 5));
}

/// Fuzz every public decoder with deterministic random and truncated input.
/// A malformed packet must never panic or allocate wildly.
#[test]
fn malformed_packets_never_panic() {
    let key = SessionKey::from_pin("4829");
    let mut rng = 0x1234_5678u32;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 17;
        rng ^= rng << 5;
        rng
    };

    for len in 0..256 {
        let mut buf = vec![0u8; len];
        for b in &mut buf {
            *b = next() as u8;
        }
        let _ = WireHeader::decode(&buf);
        let _ = ClientFeedbackPacket::decode(&buf);
        let _ = ClientFeedbackPacket::decode_verified(&buf, Some(&key));
        let mut mutable = buf.clone();
        let _ = ParsedPacket::parse_and_decrypt(&mut mutable, Some(&key));

        // Truncations of a structurally plausible wire header.
        if len >= HEADER_LEN {
            let mut hdr = [0u8; HEADER_LEN];
            hdr.copy_from_slice(&buf[..HEADER_LEN]);
            let mut out = [0u8; HEADER_LEN];
            WireHeader::decode(&hdr).ok().map(|h| h.encode(&mut out));
        }
    }
}

/// u16 sequence wraparound: the sequencer uses wrapping arithmetic; pushing
/// across 65535 -> 0 must not lose or alias slots.
#[test]
fn sequencer_handles_u16_sequence_wrap() {
    let mut seq = ZeroJitterSequencer::new();
    let frame = [0.25f32; SUBFRAME_STEREO_SAMPLES];
    for s in [65534u16, 65535, 0, 1, 2] {
        assert!(seq.push_primary(s, &frame), "push failed at seq {s}");
    }
    for s in [65534u16, 65535, 0, 1, 2] {
        assert!(seq.has_primary(s), "slot {s} lost across wrap");
    }
    let mut out = [0.0f32; SUBFRAME_STEREO_SAMPLES];
    for s in [65534u16, 65535, 0, 1, 2] {
        assert_eq!(
            seq.pop_next(s, &mut out),
            STATUS_PRIMARY_LOSSLESS,
            "wrong status at seq {s}"
        );
    }
}
