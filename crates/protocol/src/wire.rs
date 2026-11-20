use crate::crypto::{SessionKey, AEAD_TAG_LEN, NONCE_SALT_LEN};
use crate::MAX_SAFE_UDP_PAYLOAD;

/// Protocol v2 magic. Bumped from 0xA3 so legacy 0.2.1 clients fail cleanly.
pub const WIRE_MAGIC: u8 = 0xA4;
pub const HEADER_LEN: usize = 16;
/// Encrypted trailer: 4-byte per-session salt + 16-byte Poly1305 tag.
pub const MAC_TAG_LEN: usize = NONCE_SALT_LEN + AEAD_TAG_LEN;

/// Protocol v2 feedback magic (legacy "ASULCTRL" is rejected).
pub const CONTROL_FEEDBACK_MAGIC: &[u8; 8] = b"ASULCTR2";
pub const CONTROL_FEEDBACK_LEN: usize = 36;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecMode {
    /// 0.0ms Lookahead Reversible Mid-Side + 2nd-Order LPC Lossless
    StudioLosslessLpc = 0,
    /// Uncompressed Raw 16-bit PCM
    RawPcm16 = 1,
    /// Low-bitrate compressed mode. The primary payload is self-identifying:
    /// `TAG_VBR_BFP` for the pure-Rust VBR codec or `TAG_OPUS_RLD` for Opus.
    /// Used by hosts that do not distinguish the two backends at the header
    /// level (and for backward compatibility).
    CompressedLowDelay = 2,
    /// Smart DTX Silence Heartbeat (Zero audio payload, keeps NAT & clock locked)
    SilenceHeartbeat = 3,
    /// Compressed mode encoded by the bundled pure-Rust Opus (`opus-rs`)
    /// backend. The payload still starts with `TAG_OPUS_RLD`, so per-packet
    /// auto-detection remains authoritative. Older clients reject this mode
    /// cleanly instead of misreading Opus bytes as legacy IMA-ADPCM.
    OpusRld = 4,
}

impl CodecMode {
    #[inline]
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            0 => Some(Self::StudioLosslessLpc),
            1 => Some(Self::RawPcm16),
            2 => Some(Self::CompressedLowDelay),
            3 => Some(Self::SilenceHeartbeat),
            4 => Some(Self::OpusRld),
            _ => None,
        }
    }
}

/// 36-Byte Client -> Host Authenticated & Replay-Protected Feedback Packet
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientFeedbackPacket {
    /// Rolling FEC recovery rate in permille (0..=1000, where 50 = 5.0% packet loss)
    pub fec_rate_permille: u16,
    /// Genuine downstream Bluetooth A2DP presentation delay in milliseconds
    /// (0 when the phone is not routing audio to Bluetooth).
    pub bluetooth_delay_ms: u16,
    /// Client volume (0..=100) for bidirectional volume sync
    pub client_volume: u8,
    /// Client requested quality profile (0 = no change, 1 = Smart Auto,
    /// 2 = Studio Lossless, 3 = Low-Latency Compressed, 4 = Max Stability 10ms,
    /// 5 = Studio Master 24-bit), a media transport command (10 = play/pause,
    /// 11 = next, 12 = previous), a compressed-codec backend selection
    /// (20 = pure-Rust VBR default, 21 = bundled Opus RLD), or a compressed
    /// bitrate target (30 = 96, 31 = 160, 32 = 256, 33 = 320 kbps).
    pub requested_mode: u8,
    /// Measured downstream hardware/DAC presentation latency in milliseconds.
    /// This is intentionally separate from `bluetooth_delay_ms`: the host uses
    /// it for multi-device acoustic alignment, while the Bluetooth UI/lip-sync
    /// path must only react to a genuine A2DP route.
    pub hardware_delay_ms: u16,
    /// Strictly monotonic per-client sequence number preventing command replay.
    pub feedback_seq: u32,
    /// 128-bit truncated HMAC-SHA256 tag (zeros when unencrypted).
    pub auth_tag: [u8; AEAD_TAG_LEN],
}

impl ClientFeedbackPacket {
    pub fn encode(&self, out: &mut [u8; CONTROL_FEEDBACK_LEN]) {
        out[0..8].copy_from_slice(CONTROL_FEEDBACK_MAGIC);
        out[8..10].copy_from_slice(&self.fec_rate_permille.to_be_bytes());
        out[10..12].copy_from_slice(&self.bluetooth_delay_ms.to_be_bytes());
        out[12] = self.client_volume;
        out[13] = self.requested_mode;
        out[14..16].copy_from_slice(&self.hardware_delay_ms.to_be_bytes());
        out[16..20].copy_from_slice(&self.feedback_seq.to_be_bytes());
        out[20..36].copy_from_slice(&self.auth_tag);
    }

    /// Encode, signing the 20-byte body when a session key is active. Without
    /// a key the tag is explicitly zeroed (legacy unencrypted LAN/party mode).
    pub fn encode_signed(&self, key: Option<&SessionKey>, out: &mut [u8; CONTROL_FEEDBACK_LEN]) {
        self.encode(out);
        let tag = key
            .map(|k| {
                let body: &[u8; 20] = out[..20].try_into().expect("20-byte feedback body");
                k.sign_feedback(body, self.feedback_seq)
            })
            .unwrap_or([0u8; AEAD_TAG_LEN]);
        out[20..36].copy_from_slice(&tag);
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < CONTROL_FEEDBACK_LEN || &buf[0..8] != CONTROL_FEEDBACK_MAGIC {
            return None;
        }
        let mut auth_tag = [0u8; AEAD_TAG_LEN];
        auth_tag.copy_from_slice(&buf[20..36]);
        Some(Self {
            fec_rate_permille: u16::from_be_bytes([buf[8], buf[9]]),
            bluetooth_delay_ms: u16::from_be_bytes([buf[10], buf[11]]),
            client_volume: buf[12],
            requested_mode: buf[13],
            hardware_delay_ms: u16::from_be_bytes([buf[14], buf[15]]),
            feedback_seq: u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]),
            auth_tag,
        })
    }

    /// Decode and verify the 128-bit HMAC-SHA256 tag. The sequence is bound
    /// into the MAC input so a captured packet cannot be replayed with a new seq.
    pub fn decode_verified(buf: &[u8], key: Option<&SessionKey>) -> Option<Self> {
        let packet = Self::decode(buf)?;
        if let Some(key) = key {
            let body: &[u8; 20] = buf[..20].try_into().ok()?;
            if !key.verify_feedback(body, packet.feedback_seq, &packet.auth_tag) {
                return None;
            }
        }
        Some(packet)
    }
}

/// Replay gate for authenticated feedback. Unencrypted LAN/party feedback has
/// no sequence guarantee, so the check applies only when a session key is
/// active. Used by both HostEngine and PhoneMicTransmitter.
#[inline]
pub fn feedback_is_replay(authenticated: bool, seq: u32, last_seq: u32) -> bool {
    authenticated && seq <= last_seq
}

/// 16-Byte Wire Header (64-bit aligned, big-endian network serialization)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireHeader {
    pub mode: CodecMode,
    pub subframe_count: u8,
    pub encrypted: bool,
    pub sequence: u16,
    pub tx_timestamp_us: u32,
    pub primary_len: u16,
    pub fec_n1_len: u8,
    pub fec_n2_len: u8,
    pub host_volume: u8,
    pub stream_id: u8,
    pub header_crc16: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    PacketTooShort,
    PacketExceedsMtu,
    InvalidMagic,
    InvalidCodecMode,
    InvalidSubframeCount,
    HeaderCrcMismatch,
    PayloadLengthMismatch,
    AuthenticationFailed,
}

impl WireHeader {
    /// Branchless L1-cache nibble lookup table for CRC-16-CCITT (poly 0x1021)
    const CRC16_LUT: [u16; 16] = [
        0x0000, 0x1021, 0x2042, 0x3063, 0x4084, 0x50A5, 0x60C6, 0x70E7, 0x8108, 0x9129, 0xA14A,
        0xB16B, 0xC18C, 0xD1AD, 0xE1CE, 0xF1EF,
    ];

    #[inline]
    pub fn compute_crc16(bytes_14: &[u8; 14]) -> u16 {
        let mut crc: u16 = 0xFFFF;
        for &b in bytes_14 {
            let idx_hi = ((crc >> 12) ^ ((b >> 4) as u16)) & 0x0F;
            crc = (crc << 4) ^ Self::CRC16_LUT[idx_hi as usize];
            let idx_lo = ((crc >> 12) ^ ((b & 0x0F) as u16)) & 0x0F;
            crc = (crc << 4) ^ Self::CRC16_LUT[idx_lo as usize];
        }
        crc
    }

    #[inline]
    pub fn encode(&self, out: &mut [u8; HEADER_LEN]) {
        let flags = ((self.mode as u8) & 0x07)
            | ((self.subframe_count & 0x07) << 3)
            | (if self.encrypted { 0x40 } else { 0 });

        let seq_be = self.sequence.to_be_bytes();
        let ts_be = self.tx_timestamp_us.to_be_bytes();
        let prim_be = self.primary_len.to_be_bytes();

        let first_14: [u8; 14] = [
            WIRE_MAGIC,
            flags,
            seq_be[0],
            seq_be[1],
            ts_be[0],
            ts_be[1],
            ts_be[2],
            ts_be[3],
            prim_be[0],
            prim_be[1],
            self.fec_n1_len,
            self.fec_n2_len,
            self.host_volume,
            self.stream_id,
        ];

        let crc = Self::compute_crc16(&first_14).to_be_bytes();
        out[..14].copy_from_slice(&first_14);
        out[14] = crc[0];
        out[15] = crc[1];
    }

    #[inline]
    pub fn decode(buf: &[u8]) -> Result<Self, WireError> {
        if buf.len() < HEADER_LEN {
            return Err(WireError::PacketTooShort);
        }
        if buf[0] != WIRE_MAGIC {
            return Err(WireError::InvalidMagic);
        }

        let first_14: &[u8; 14] = buf[..14].try_into().unwrap();
        let expected_crc = Self::compute_crc16(first_14);
        let actual_crc = u16::from_be_bytes([buf[14], buf[15]]);
        if expected_crc != actual_crc {
            return Err(WireError::HeaderCrcMismatch);
        }

        let flags = buf[1];
        let mode = CodecMode::from_u8(flags & 0x07).ok_or(WireError::InvalidCodecMode)?;
        let subframe_count = (flags >> 3) & 0x07;
        // The host bundles at most four 2.5ms subframes per datagram, and a
        // non-heartbeat packet must carry at least one. Reject impossible
        // counts before callers iterate over `subframe_count` payloads.
        if subframe_count > 4 || (mode != CodecMode::SilenceHeartbeat && subframe_count == 0) {
            return Err(WireError::InvalidSubframeCount);
        }
        let encrypted = (flags & 0x40) != 0;

        let sequence = u16::from_be_bytes([buf[2], buf[3]]);
        let tx_timestamp_us = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let primary_len = u16::from_be_bytes([buf[8], buf[9]]);
        let fec_n1_len = buf[10];
        let fec_n2_len = buf[11];
        let host_volume = buf[12];
        let stream_id = buf[13];

        let mac_extra = if encrypted { MAC_TAG_LEN } else { 0 };
        let total_expected = HEADER_LEN
            + (primary_len as usize)
            + (fec_n1_len as usize)
            + (fec_n2_len as usize)
            + mac_extra;

        if buf.len() < total_expected {
            return Err(WireError::PayloadLengthMismatch);
        }

        Ok(Self {
            mode,
            subframe_count,
            encrypted,
            sequence,
            tx_timestamp_us,
            primary_len,
            fec_n1_len,
            fec_n2_len,
            host_volume,
            stream_id,
            header_crc16: actual_crc,
        })
    }
}

pub struct ParsedPacket<'a> {
    pub header: WireHeader,
    pub primary_payload: &'a [u8],
    pub fec_n1_payload: &'a [u8],
    pub fec_n2_payload: &'a [u8],
}

impl<'a> ParsedPacket<'a> {
    #[inline]
    pub fn parse(datagram: &'a [u8]) -> Result<Self, WireError> {
        let header = WireHeader::decode(datagram)?;
        let p_start = HEADER_LEN;
        let p_end = p_start + header.primary_len as usize;
        let f1_end = p_end + header.fec_n1_len as usize;
        let f2_end = f1_end + header.fec_n2_len as usize;

        Ok(Self {
            header,
            primary_payload: &datagram[p_start..p_end],
            fec_n1_payload: &datagram[p_end..f1_end],
            fec_n2_payload: &datagram[f1_end..f2_end],
        })
    }

    /// Parse and verify/decrypt in-place with ChaCha20-Poly1305 AEAD (v2).
    #[inline]
    pub fn parse_and_decrypt(
        datagram: &'a mut [u8],
        session_key: Option<&SessionKey>,
    ) -> Result<Self, WireError> {
        let header = WireHeader::decode(datagram)?;
        let payload_len = (header.primary_len as usize)
            + (header.fec_n1_len as usize)
            + (header.fec_n2_len as usize);
        let payload_end = HEADER_LEN + payload_len;

        if header.encrypted {
            let key = session_key.ok_or(WireError::AuthenticationFailed)?;
            let header_14: [u8; 14] = datagram[..14]
                .try_into()
                .map_err(|_| WireError::PacketTooShort)?;
            let salt: [u8; NONCE_SALT_LEN] = datagram[payload_end..payload_end + NONCE_SALT_LEN]
                .try_into()
                .map_err(|_| WireError::PacketTooShort)?;
            let tag: [u8; AEAD_TAG_LEN] = datagram
                [payload_end + NONCE_SALT_LEN..payload_end + MAC_TAG_LEN]
                .try_into()
                .map_err(|_| WireError::PacketTooShort)?;
            if !key.decrypt_audio_in_place(
                &header_14,
                &header,
                &salt,
                &mut datagram[HEADER_LEN..payload_end],
                &tag,
            ) {
                return Err(WireError::AuthenticationFailed);
            }
        } else if session_key.is_some() {
            return Err(WireError::AuthenticationFailed);
        }

        let p_start = HEADER_LEN;
        let p_end = p_start + header.primary_len as usize;
        let f1_end = p_end + header.fec_n1_len as usize;
        let f2_end = f1_end + header.fec_n2_len as usize;

        Ok(Self {
            header,
            primary_payload: &datagram[p_start..p_end],
            fec_n1_payload: &datagram[p_end..f1_end],
            fec_n2_payload: &datagram[f1_end..f2_end],
        })
    }
}

pub fn pack_datagram(
    header: WireHeader,
    primary: &[u8],
    fec_n1: &[u8],
    fec_n2: &[u8],
    out_buf: &mut [u8; MAX_SAFE_UDP_PAYLOAD],
) -> Result<usize, WireError> {
    pack_datagram_with_key(header, primary, fec_n1, fec_n2, None, out_buf)
}

pub fn pack_datagram_with_key(
    mut header: WireHeader,
    primary: &[u8],
    fec_n1: &[u8],
    fec_n2: &[u8],
    session_key: Option<&SessionKey>,
    out_buf: &mut [u8; MAX_SAFE_UDP_PAYLOAD],
) -> Result<usize, WireError> {
    header.encrypted = session_key.is_some();
    let mac_extra = if header.encrypted { MAC_TAG_LEN } else { 0 };
    let total_len = HEADER_LEN + primary.len() + fec_n1.len() + fec_n2.len() + mac_extra;

    if total_len > MAX_SAFE_UDP_PAYLOAD
        || fec_n1.len() > u8::MAX as usize
        || fec_n2.len() > u8::MAX as usize
    {
        return Err(WireError::PacketExceedsMtu);
    }

    header.primary_len = primary.len() as u16;
    header.fec_n1_len = fec_n1.len() as u8;
    header.fec_n2_len = fec_n2.len() as u8;

    let mut hdr_bytes = [0u8; HEADER_LEN];
    header.encode(&mut hdr_bytes);
    out_buf[..HEADER_LEN].copy_from_slice(&hdr_bytes);

    let mut cursor = HEADER_LEN;
    out_buf[cursor..cursor + primary.len()].copy_from_slice(primary);
    cursor += primary.len();

    out_buf[cursor..cursor + fec_n1.len()].copy_from_slice(fec_n1);
    cursor += fec_n1.len();

    out_buf[cursor..cursor + fec_n2.len()].copy_from_slice(fec_n2);
    cursor += fec_n2.len();

    if let Some(key) = session_key {
        let header_14: [u8; 14] = hdr_bytes[..14]
            .try_into()
            .expect("14-byte wire header prefix");
        let (salt, tag) =
            key.encrypt_audio_in_place(&header_14, &header, &mut out_buf[HEADER_LEN..cursor]);
        out_buf[cursor..cursor + NONCE_SALT_LEN].copy_from_slice(&salt);
        out_buf[cursor + NONCE_SALT_LEN..cursor + MAC_TAG_LEN].copy_from_slice(&tag);
        cursor += MAC_TAG_LEN;
    }

    Ok(cursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypted_wire_roundtrip_and_tamper_rejection() {
        let key = SessionKey::from_pin("9942");
        let wrong_key = SessionKey::from_pin("1111");
        let mut buf = [0u8; MAX_SAFE_UDP_PAYLOAD];

        let hdr = WireHeader {
            mode: CodecMode::StudioLosslessLpc,
            subframe_count: 1,
            encrypted: true,
            sequence: 77,
            tx_timestamp_us: 123_456,
            primary_len: 0,
            fec_n1_len: 0,
            fec_n2_len: 0,
            host_volume: 100,
            stream_id: 1,
            header_crc16: 0,
        };

        let primary = [0xAAu8; 100];
        let fec1 = [0xBBu8; 124];
        let len = pack_datagram_with_key(hdr, &primary, &fec1, &[], Some(&key), &mut buf).unwrap();

        // Wrong key must fail authentication
        let mut copy_wrong = buf;
        assert_eq!(
            ParsedPacket::parse_and_decrypt(&mut copy_wrong[..len], Some(&wrong_key)).err(),
            Some(WireError::AuthenticationFailed)
        );

        // Correct key decrypts back to exact plaintext
        let parsed = ParsedPacket::parse_and_decrypt(&mut buf[..len], Some(&key)).unwrap();
        assert_eq!(parsed.primary_payload, &primary);
        assert_eq!(parsed.fec_n1_payload, &fec1);
    }

    #[test]
    fn test_mac_binds_header_fields_against_recomputed_crc_tampering() {
        let key = SessionKey::from_pin("4829");
        let hdr = WireHeader {
            mode: CodecMode::StudioLosslessLpc,
            subframe_count: 1,
            encrypted: true,
            sequence: 31,
            tx_timestamp_us: 777_000,
            primary_len: 0,
            fec_n1_len: 0,
            fec_n2_len: 0,
            host_volume: 100,
            stream_id: 3,
            header_crc16: 0,
        };
        let primary = [0x5Au8; 64];
        let mut packed = [0u8; MAX_SAFE_UDP_PAYLOAD];
        let len = pack_datagram_with_key(
            hdr,
            &primary,
            &[],
            &[],
            Some(&key),
            &mut packed,
        )
        .unwrap();

        // The CRC is deliberately unkeyed, so an attacker can rewrite any
        // header byte and repair bytes 14..16. Fields that influence playback
        // must still be covered by the keyed MAC.
        for (offset, tampered) in [(12usize, 5u8), (13, 9)] {
            let mut copy = packed;
            copy[offset] = tampered;
            let crc = WireHeader::compute_crc16(copy[..14].try_into().unwrap()).to_be_bytes();
            copy[14] = crc[0];
            copy[15] = crc[1];

            assert_eq!(
                ParsedPacket::parse_and_decrypt(&mut copy[..len], Some(&key)).err(),
                Some(WireError::AuthenticationFailed),
                "tampered header byte {} must fail authentication",
                offset
            );
        }
    }

    #[test]
    fn test_wire_header_rejects_invalid_subframe_count() {
        let mut buf = [0u8; HEADER_LEN];
        let mut hdr = WireHeader {
            mode: CodecMode::StudioLosslessLpc,
            subframe_count: 0,
            encrypted: false,
            sequence: 1,
            tx_timestamp_us: 0,
            primary_len: 0,
            fec_n1_len: 0,
            fec_n2_len: 0,
            host_volume: 100,
            stream_id: 0,
            header_crc16: 0,
        };

        hdr.encode(&mut buf);
        assert_eq!(
            WireHeader::decode(&buf).err(),
            Some(WireError::InvalidSubframeCount)
        );

        hdr.subframe_count = 5;
        hdr.encode(&mut buf);
        assert_eq!(
            WireHeader::decode(&buf).err(),
            Some(WireError::InvalidSubframeCount)
        );

        // Heartbeats carry no audio payload, so zero is valid for them.
        hdr.mode = CodecMode::SilenceHeartbeat;
        hdr.subframe_count = 0;
        hdr.encode(&mut buf);
        assert!(WireHeader::decode(&buf).is_ok());

        // Four bundled subframes are the maximum valid non-heartbeat packet.
        hdr.mode = CodecMode::StudioLosslessLpc;
        hdr.subframe_count = 4;
        hdr.encode(&mut buf);
        assert!(WireHeader::decode(&buf).is_ok());
    }

    #[test]
    fn test_opus_wire_mode_roundtrip() {
        let mut buf = [0u8; HEADER_LEN];
        let hdr = WireHeader {
            mode: CodecMode::OpusRld,
            subframe_count: 2,
            encrypted: false,
            sequence: 42,
            tx_timestamp_us: 9_000,
            primary_len: 0,
            fec_n1_len: 0,
            fec_n2_len: 0,
            host_volume: 100,
            stream_id: 0,
            header_crc16: 0,
        };
        hdr.encode(&mut buf);
        let decoded = WireHeader::decode(&buf).expect("Opus mode header must decode");
        assert_eq!(decoded.mode, CodecMode::OpusRld);
        assert_eq!(decoded.subframe_count, 2);
    }

    #[test]
    fn test_feedback_authentication_rejects_forgery_and_tampering() {
        let key = SessionKey::from_pin("4829");
        let wrong_key = SessionKey::from_pin("0000");
        let mut buf = [0u8; CONTROL_FEEDBACK_LEN];
        let packet = ClientFeedbackPacket {
            fec_rate_permille: 37,
            bluetooth_delay_ms: 142,
            hardware_delay_ms: 21,
            client_volume: 76,
            requested_mode: 20,
            feedback_seq: 1,
            auth_tag: [0u8; AEAD_TAG_LEN],
        };

        packet.encode_signed(Some(&key), &mut buf);
        let decoded =
            ClientFeedbackPacket::decode_verified(&buf, Some(&key)).expect("signed feedback");
        assert_eq!(decoded.fec_rate_permille, 37);
        assert_eq!(decoded.requested_mode, 20);
        assert_eq!(decoded.bluetooth_delay_ms, 142);

        // Wrong key, unsigned legacy packet, and a tampered command must all
        // be rejected while a key is configured.
        assert!(ClientFeedbackPacket::decode_verified(&buf, Some(&wrong_key)).is_none());
        let unsigned = ClientFeedbackPacket {
            auth_tag: [0u8; AEAD_TAG_LEN],
            ..decoded
        };
        unsigned.encode(&mut buf);
        assert!(ClientFeedbackPacket::decode_verified(&buf, Some(&key)).is_none());

        packet.encode_signed(Some(&key), &mut buf);
        buf[13] = 21; // swap the codec backend command after signing
        assert!(ClientFeedbackPacket::decode_verified(&buf, Some(&key)).is_none());

        // Without a key the control plane remains backward compatible.
        packet.encode_signed(None, &mut buf);
        assert_eq!(&buf[20..36], &[0u8; AEAD_TAG_LEN]);
        assert!(ClientFeedbackPacket::decode_verified(&buf, None).is_some());
    }

    #[test]
    fn test_tamper_any_byte_fails_authentication() {
        let key = SessionKey::from_pin("4829");
        let hdr = WireHeader {
            mode: CodecMode::StudioLosslessLpc,
            subframe_count: 2,
            encrypted: true,
            sequence: 0x1234,
            tx_timestamp_us: 0x0056_78AB,
            primary_len: 0,
            fec_n1_len: 0,
            fec_n2_len: 0,
            host_volume: 100,
            stream_id: 2,
            header_crc16: 0,
        };
        let primary = [0x5Au8; 96];
        let mut packed = [0u8; MAX_SAFE_UDP_PAYLOAD];
        let len = pack_datagram_with_key(hdr, &primary, &[], &[], Some(&key), &mut packed).unwrap();

        // Header AAD bytes 0..14: repair the unkeyed CRC so decode succeeds,
        // proving the AEAD tag (not the CRC) binds those fields.
        // 8..12 are the length fields: changing them alters framing and is
        // rejected structurally before the AEAD step (still a hard failure).
        for offset in [1usize, 2, 3, 4, 5, 6, 7, 12, 13] {
            let mut copy = packed;
            copy[offset] ^= 0x01;
            let crc = WireHeader::compute_crc16(copy[..14].try_into().unwrap()).to_be_bytes();
            copy[14] = crc[0];
            copy[15] = crc[1];
            assert_eq!(
                ParsedPacket::parse_and_decrypt(&mut copy[..len], Some(&key)).err(),
                Some(WireError::AuthenticationFailed),
                "header byte {offset} must be AEAD-authenticated"
            );
        }
        for offset in [8usize, 9, 10, 11] {
            let mut copy = packed;
            copy[offset] ^= 0x01;
            let crc = WireHeader::compute_crc16(copy[..14].try_into().unwrap()).to_be_bytes();
            copy[14] = crc[0];
            copy[15] = crc[1];
            assert!(
                ParsedPacket::parse_and_decrypt(&mut copy[..len], Some(&key)).is_err(),
                "tampered length byte {offset} must be rejected"
            );
        }
        // Salt, ciphertext and tag bits.
        let payload_start = HEADER_LEN + primary.len();
        for offset in [payload_start, payload_start + NONCE_SALT_LEN, payload_start + MAC_TAG_LEN - 1, HEADER_LEN] {
            let mut copy = packed;
            copy[offset] ^= 0x01;
            assert_eq!(
                ParsedPacket::parse_and_decrypt(&mut copy[..len], Some(&key)).err(),
                Some(WireError::AuthenticationFailed),
                "trailer/ciphertext byte {offset} must be AEAD-authenticated"
            );
        }
    }

    #[test]
    fn test_authenticated_feedback_replay_is_rejected() {
        // Acceptance: replaying a captured authenticated packet with
        // feedback_seq <= last_feedback_seq is rejected; a fresh seq passes.
        assert!(feedback_is_replay(true, 5, 5));
        assert!(feedback_is_replay(true, 4, 5));
        assert!(!feedback_is_replay(true, 6, 5));
        // Unencrypted LAN/party feedback is intentionally not replay-checked.
        assert!(!feedback_is_replay(false, 1, 5));
    }

    #[test]
    fn test_v2_magic_rejects_legacy_peers() {
        assert_eq!(WIRE_MAGIC, 0xA4);
        assert_eq!(CONTROL_FEEDBACK_MAGIC, b"ASULCTR2");
        let mut buf = [0u8; HEADER_LEN];
        let hdr = WireHeader {
            mode: CodecMode::StudioLosslessLpc,
            subframe_count: 1,
            encrypted: false,
            sequence: 1,
            tx_timestamp_us: 0,
            primary_len: 0,
            fec_n1_len: 0,
            fec_n2_len: 0,
            host_volume: 100,
            stream_id: 0,
            header_crc16: 0,
        };
        hdr.encode(&mut buf);
        buf[0] = 0xA3; // legacy v1 magic
        let crc = WireHeader::compute_crc16(buf[..14].try_into().unwrap()).to_be_bytes();
        buf[14] = crc[0];
        buf[15] = crc[1];
        assert_eq!(WireHeader::decode(&buf).err(), Some(WireError::InvalidMagic));
    }

    #[test]
    fn test_crc16_lut_equivalence() {
        let bytes: [u8; 14] = [
            0x41, 0x53, 0x01, 0x00, 0x2A, 0x00, 0x01, 0xE2, 0x40, 0x00, 0x78, 0x00, 0x64, 0x01,
        ];
        // Bit-by-bit reference calculation
        let mut ref_crc: u16 = 0xFFFF;
        for &b in &bytes {
            ref_crc ^= (b as u16) << 8;
            for _ in 0..8 {
                if (ref_crc & 0x8000) != 0 {
                    ref_crc = (ref_crc << 1) ^ 0x1021;
                } else {
                    ref_crc <<= 1;
                }
            }
        }
        let lut_crc = WireHeader::compute_crc16(&bytes);
        assert_eq!(
            lut_crc, ref_crc,
            "Nibble LUT CRC must match bitwise CRC-16-CCITT exactly"
        );
    }
}
