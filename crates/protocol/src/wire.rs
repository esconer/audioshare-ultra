use crate::crypto::SessionKey;
use crate::MAX_SAFE_UDP_PAYLOAD;

pub const WIRE_MAGIC: u8 = 0xA3;
pub const HEADER_LEN: usize = 16;
pub const MAC_TAG_LEN: usize = 4;

pub const CONTROL_FEEDBACK_MAGIC: &[u8; 8] = b"ASULCTRL";
pub const CONTROL_FEEDBACK_LEN: usize = 20;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecMode {
    /// 0.0ms Lookahead Reversible Mid-Side + 2nd-Order LPC Lossless
    StudioLosslessLpc = 0,
    /// Uncompressed Raw 16-bit PCM
    RawPcm16 = 1,
    /// Low-Bitrate ADPCM / Opus Compressed Mode (Automatic fallback on weak 2.4GHz Wi-Fi)
    CompressedLowDelay = 2,
    /// Smart DTX Silence Heartbeat (Zero audio payload, keeps NAT & clock locked)
    SilenceHeartbeat = 3,
}

impl CodecMode {
    #[inline]
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            0 => Some(Self::StudioLosslessLpc),
            1 => Some(Self::RawPcm16),
            2 => Some(Self::CompressedLowDelay),
            3 => Some(Self::SilenceHeartbeat),
            _ => None,
        }
    }
}

/// 20-Byte Client -> Host Real-Time Telemetry & Control Feedback Packet (Sent every 500ms)
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
    /// 2 = Studio Lossless, 3 = Low-Latency Compressed, 4 = Max Stability 10ms)
    /// or a media transport command (10 = play/pause, 11 = next, 12 = previous).
    pub requested_mode: u8,
    /// Measured downstream hardware/DAC presentation latency in milliseconds.
    /// This is intentionally separate from `bluetooth_delay_ms`: the host uses
    /// it for multi-device acoustic alignment, while the Bluetooth UI/lip-sync
    /// path must only react to a genuine A2DP route.
    pub hardware_delay_ms: u16,
    /// 32-bit authentication token derived from SessionKey (or 0 if unencrypted)
    pub auth_mac32: u32,
}

impl ClientFeedbackPacket {
    pub fn encode(&self, out: &mut [u8; CONTROL_FEEDBACK_LEN]) {
        out[0..8].copy_from_slice(CONTROL_FEEDBACK_MAGIC);
        out[8..10].copy_from_slice(&self.fec_rate_permille.to_be_bytes());
        out[10..12].copy_from_slice(&self.bluetooth_delay_ms.to_be_bytes());
        out[12] = self.client_volume;
        out[13] = self.requested_mode;
        out[14..16].copy_from_slice(&self.hardware_delay_ms.to_be_bytes());
        out[16..20].copy_from_slice(&self.auth_mac32.to_be_bytes());
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < CONTROL_FEEDBACK_LEN || &buf[0..8] != CONTROL_FEEDBACK_MAGIC {
            return None;
        }
        Some(Self {
            fec_rate_permille: u16::from_be_bytes([buf[8], buf[9]]),
            bluetooth_delay_ms: u16::from_be_bytes([buf[10], buf[11]]),
            client_volume: buf[12],
            requested_mode: buf[13],
            hardware_delay_ms: u16::from_be_bytes([buf[14], buf[15]]),
            auth_mac32: u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]),
        })
    }
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

    /// Parse and decrypt in-place if encrypted with ChaCha20 + MAC32
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
            let tag_bytes: [u8; 4] = datagram[payload_end..payload_end + MAC_TAG_LEN]
                .try_into()
                .map_err(|_| WireError::PacketTooShort)?;
            let received_mac = u32::from_be_bytes(tag_bytes);
            let expected_mac = key.compute_mac32(&header, &datagram[HEADER_LEN..payload_end]);

            if received_mac != expected_mac {
                return Err(WireError::AuthenticationFailed);
            }
            key.apply_keystream(&header, &mut datagram[HEADER_LEN..payload_end]);
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

    if total_len > MAX_SAFE_UDP_PAYLOAD {
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
        key.apply_keystream(&header, &mut out_buf[HEADER_LEN..cursor]);
        let mac = key.compute_mac32(&header, &out_buf[HEADER_LEN..cursor]);
        out_buf[cursor..cursor + MAC_TAG_LEN].copy_from_slice(&mac.to_be_bytes());
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
