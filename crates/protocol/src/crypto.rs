use crate::wire::WireHeader;

/// 256-bit symmetric session key derived from a Pairing PIN or QR Token
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SessionKey([u32; 8]);

impl SessionKey {
    /// Derive a 256-bit key from a human-readable PIN (e.g., "4829" or passphrase)
    /// using 1,000 rounds of ChaCha20-based PBKDF mixing (zero external crate dependencies).
    pub fn from_pin(pin: &str) -> Self {
        let mut state = [
            0x6170_7865,
            0x3320_646e,
            0x7962_2d32,
            0x6b20_6574,
            0x0102_0304,
            0x0506_0708,
            0x090a_0b0c,
            0x0d0e_0f10,
        ];

        for (i, b) in pin.as_bytes().iter().enumerate() {
            state[i % 8] ^= (*b as u32)
                .wrapping_mul(0x9E37_79B9)
                .rotate_left((i as u32) * 5);
        }

        // 1,000 mixing rounds to harden short PINs
        for round in 0..1000u32 {
            state[0] = state[0].wrapping_add(round);
            let block = chacha20_block(&state, round, [0xA3A3_A3A3, 0x5A5A_5A5A, round]);
            for i in 0..8 {
                state[i] ^= block[i] ^ block[i + 8];
            }
        }

        Self(state)
    }

    /// Encrypt or decrypt payload in-place using RFC 8439 ChaCha20 keystream
    /// keyed by the packet's verified WireHeader (Sequence + Timestamp + StreamID).
    #[inline]
    pub fn apply_keystream(&self, header: &WireHeader, payload: &mut [u8]) {
        let nonce = [
            ((header.sequence as u32) << 16) | (header.stream_id as u32),
            header.tx_timestamp_us,
            0x4153_554C, // "ASUL" domain separator
        ];

        let mut block_counter = 1u32;
        for chunk in payload.chunks_mut(64) {
            let keystream_words = chacha20_block(&self.0, block_counter, nonce);
            block_counter = block_counter.wrapping_add(1);

            let mut keystream_bytes = [0u8; 64];
            for (i, &w) in keystream_words.iter().enumerate() {
                let le = w.to_le_bytes();
                keystream_bytes[i * 4..i * 4 + 4].copy_from_slice(&le);
            }

            for (dst, &k) in chunk.iter_mut().zip(keystream_bytes.iter()) {
                *dst ^= k;
            }
        }
    }

    /// Compute a fast 32-bit keyed MAC tag over the header + ciphertext to reject wrong PINs
    #[inline]
    pub fn compute_mac32(&self, header: &WireHeader, ciphertext: &[u8]) -> u32 {
        let nonce = [
            header.sequence as u32,
            header.tx_timestamp_us,
            ciphertext.len() as u32,
        ];
        let poly_key = chacha20_block(&self.0, 0, nonce);
        let mut acc1 = poly_key[0] ^ 0x736f_6d65;
        let mut acc2 = poly_key[1] ^ 0x646f_7261;

        for chunk in ciphertext.chunks(4) {
            let mut word_bytes = [0u8; 4];
            word_bytes[..chunk.len()].copy_from_slice(chunk);
            let v = u32::from_le_bytes(word_bytes);
            acc1 = acc1
                .wrapping_add(v)
                .wrapping_mul(poly_key[2] | 1)
                .rotate_left(13);
            acc2 = (acc2 ^ acc1).wrapping_mul(poly_key[3] | 1).rotate_right(11);
        }

        acc1 ^ acc2 ^ poly_key[4]
    }
}

#[inline(always)]
fn quarter_round(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(16);

    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(12);

    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(8);

    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(7);
}

/// RFC 8439 ChaCha20 20-Round Core Block Function (SIMD auto-vectorizable)
#[inline]
fn chacha20_block(key: &[u32; 8], counter: u32, nonce: [u32; 3]) -> [u32; 16] {
    let initial: [u32; 16] = [
        0x6170_7865,
        0x3320_646e,
        0x7962_2d32,
        0x6b20_6574,
        key[0],
        key[1],
        key[2],
        key[3],
        key[4],
        key[5],
        key[6],
        key[7],
        counter,
        nonce[0],
        nonce[1],
        nonce[2],
    ];

    let mut working = initial;
    for _ in 0..10 {
        // Column rounds
        quarter_round(&mut working, 0, 4, 8, 12);
        quarter_round(&mut working, 1, 5, 9, 13);
        quarter_round(&mut working, 2, 6, 10, 14);
        quarter_round(&mut working, 3, 7, 11, 15);
        // Diagonal rounds
        quarter_round(&mut working, 0, 5, 10, 15);
        quarter_round(&mut working, 1, 6, 11, 12);
        quarter_round(&mut working, 2, 7, 8, 13);
        quarter_round(&mut working, 3, 4, 9, 14);
    }

    for i in 0..16 {
        working[i] = working[i].wrapping_add(initial[i]);
    }
    working
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::CodecMode;

    #[test]
    fn test_chacha20_pin_encryption_and_mac_verification() {
        let key_correct = SessionKey::from_pin("8492");
        let key_wrong = SessionKey::from_pin("0000");

        let hdr = WireHeader {
            mode: CodecMode::StudioLosslessLpc,
            subframe_count: 2,
            encrypted: true,
            sequence: 105,
            tx_timestamp_us: 500_000,
            primary_len: 128,
            fec_n1_len: 0,
            fec_n2_len: 0,
            host_volume: 100,
            stream_id: 1,
            header_crc16: 0,
        };

        let original = [0x42u8; 256];
        let mut buf = original;

        // Encrypt in place
        key_correct.apply_keystream(&hdr, &mut buf);
        assert_ne!(buf, original, "Ciphertext must not match plaintext");

        let mac = key_correct.compute_mac32(&hdr, &buf);
        assert_ne!(
            mac,
            key_wrong.compute_mac32(&hdr, &buf),
            "Wrong PIN must fail MAC verification"
        );

        // Decrypt in place
        key_correct.apply_keystream(&hdr, &mut buf);
        assert_eq!(
            buf, original,
            "Decrypted payload must match original bit-for-bit"
        );
    }
}
