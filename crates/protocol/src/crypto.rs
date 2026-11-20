//! RFC 8439 ChaCha20-Poly1305 AEAD for AudioShare Ultra v2 wire format.
//!
//! The session key is PBKDF2-HMAC-SHA256(65,536 rounds) over the pairing PIN
//! with a fixed domain salt. Each `SessionKey::from_pin` also draws a random
//! 4-byte per-session nonce salt, so restarting a host with the same PIN never
//! reuses a keystream (no two-time pad). Audio uses a 12-byte nonce derived from
//! that salt; feedback uses a truncated HMAC-SHA256 (nonce-misuse resistant).

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce, Tag};
use hmac::{Hmac, Mac};
use pbkdf2::pbkdf2_hmac_array;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::wire::WireHeader;

const PIN_KDF_SALT: &[u8] = b"AudioShareUltra-v2-PBKDF2-SHA256";
const PIN_KDF_ROUNDS: u32 = 65_536;

/// Per-session nonce salt length (also transmitted in the AEAD trailer).
pub const NONCE_SALT_LEN: usize = 4;
/// Tag length: Poly1305 for audio, truncated HMAC-SHA256 for feedback.
/// The full wire trailer is salt + tag = 20 bytes.
pub const AEAD_TAG_LEN: usize = 16;

/// 256-bit symmetric session key derived from a pairing PIN or QR token.
#[derive(Clone, Copy)]
pub struct SessionKey {
    key: [u8; 32],
    session_salt: [u8; NONCE_SALT_LEN],
}

impl std::fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionKey(<redacted>)")
    }
}

impl PartialEq for SessionKey {
    fn eq(&self, other: &Self) -> bool {
        // Constant-time comparison of the derived key only: the random nonce
        // salt differs per instance even for the same PIN.
        bool::from(self.key.ct_eq(&other.key))
    }
}
impl Eq for SessionKey {}

impl SessionKey {
    /// Derive a 256-bit key with 65,536 PBKDF2-HMAC-SHA256 rounds and draw a
    /// fresh OS-CPRNG 4-byte session nonce salt.
    pub fn from_pin(pin: &str) -> Self {
        let key = pbkdf2_hmac_array::<Sha256, 32>(pin.as_bytes(), PIN_KDF_SALT, PIN_KDF_ROUNDS);
        let mut session_salt = [0u8; NONCE_SALT_LEN];
        if getrandom::getrandom(&mut session_salt).is_err() {
            let fallback = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u32;
            session_salt = fallback.to_be_bytes();
        }
        Self { key, session_salt }
    }

    /// Rotate the 4-byte session nonce salt to a fresh value while leaving the
    /// derived key untouched. Draws from the OS CSPRNG; if that fails or the
    /// RNG happens to return the current salt, advance the previous salt by
    /// one instead so the salt is always different (no nonce reuse).
    pub fn rotate_session_salt(&mut self) {
        let prev = self.session_salt;
        let mut fresh = [0u8; NONCE_SALT_LEN];
        let changed = getrandom::getrandom(&mut fresh).is_ok() && fresh != prev;
        self.session_salt = if changed {
            fresh
        } else {
            u32::from_be_bytes(prev).wrapping_add(1).to_be_bytes()
        };
    }

    /// 12-byte nonce for an audio packet: salt(4) | timestamp(4) | seq(2) |
    /// stream_id(1) | domain(1). Nonce uniqueness is per-session.
    #[inline]
    fn audio_nonce(salt: &[u8; NONCE_SALT_LEN], header: &WireHeader) -> [u8; 12] {
        let ts = header.tx_timestamp_us.to_be_bytes();
        let seq = header.sequence.to_be_bytes();
        [
            salt[0],
            salt[1],
            salt[2],
            salt[3],
            ts[0],
            ts[1],
            ts[2],
            ts[3],
            seq[0],
            seq[1],
            header.stream_id,
            0xA3,
        ]
    }

    /// Encrypt `payload` in place, authenticating the 14-byte wire header and
    /// the 4-byte session salt as AAD. Returns the salt and detached Poly1305 tag.
    #[inline]
    pub fn encrypt_audio_in_place(
        &self,
        header_14: &[u8; 14],
        header: &WireHeader,
        payload: &mut [u8],
    ) -> ([u8; NONCE_SALT_LEN], [u8; AEAD_TAG_LEN]) {
        let cipher = ChaCha20Poly1305::new_from_slice(&self.key).expect("32-byte key");
        let nonce_bytes = Self::audio_nonce(&self.session_salt, header);
        let mut aad = [0u8; 14 + NONCE_SALT_LEN];
        aad[..14].copy_from_slice(header_14);
        aad[14..].copy_from_slice(&self.session_salt);
        let tag = cipher
            .encrypt_in_place_detached(Nonce::from_slice(&nonce_bytes), &aad, payload)
            .expect("AEAD encrypt in place");
        let mut tag_out = [0u8; AEAD_TAG_LEN];
        tag_out.copy_from_slice(tag.as_slice());
        (self.session_salt, tag_out)
    }

    /// Constant-time verify + decrypt. Returns `false` on any tag mismatch.
    #[inline]
    pub fn decrypt_audio_in_place(
        &self,
        header_14: &[u8; 14],
        header: &WireHeader,
        salt: &[u8; NONCE_SALT_LEN],
        payload: &mut [u8],
        tag: &[u8; AEAD_TAG_LEN],
    ) -> bool {
        let cipher = ChaCha20Poly1305::new_from_slice(&self.key).expect("32-byte key");
        let nonce_bytes = Self::audio_nonce(salt, header);
        let mut aad = [0u8; 14 + NONCE_SALT_LEN];
        aad[..14].copy_from_slice(header_14);
        aad[14..].copy_from_slice(salt);
        cipher
            .decrypt_in_place_detached(
                Nonce::from_slice(&nonce_bytes),
                &aad,
                payload,
                Tag::from_slice(tag),
            )
            .is_ok()
    }

    /// Compute a 128-bit truncated HMAC-SHA256 tag over the 20-byte feedback
    /// body + sequence. Unlike Poly1305, HMAC-SHA256 is nonce-misuse resistant
    /// when multiple clients or reconnects share the same PIN and start at seq=1.
    #[inline]
    pub fn sign_feedback(&self, body_20: &[u8; 20], feedback_seq: u32) -> [u8; AEAD_TAG_LEN] {
        type HmacSha256 = Hmac<Sha256>;
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.key).expect("32-byte key");
        mac.update(b"ASULCTR2-MAC");
        mac.update(&feedback_seq.to_be_bytes());
        mac.update(body_20);
        let digest = mac.finalize().into_bytes();
        let mut out = [0u8; AEAD_TAG_LEN];
        out.copy_from_slice(&digest[..AEAD_TAG_LEN]);
        out
    }

    /// Constant-time 128-bit HMAC-SHA256 feedback tag verification.
    #[inline]
    pub fn verify_feedback(
        &self,
        body_20: &[u8; 20],
        feedback_seq: u32,
        tag: &[u8; AEAD_TAG_LEN],
    ) -> bool {
        let expected = self.sign_feedback(body_20, feedback_seq);
        bool::from(expected.ct_eq(tag))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::CodecMode;

    fn header() -> WireHeader {
        WireHeader {
            mode: CodecMode::StudioLosslessLpc,
            subframe_count: 2,
            encrypted: true,
            sequence: 0,
            tx_timestamp_us: 0,
            primary_len: 64,
            fec_n1_len: 0,
            fec_n2_len: 0,
            host_volume: 100,
            stream_id: 1,
            header_crc16: 0,
        }
    }

    #[test]
    fn test_session_salt_prevents_two_time_pad_and_equality_holds() {
        let a = SessionKey::from_pin("4829");
        let b = SessionKey::from_pin("4829");
        assert_ne!(a.session_salt, b.session_salt, "session salts must be random");
        assert_eq!(a, b, "same PIN must compare equal (key bytes only)");

        let hdr = header();
        let mut hdr_full = [0u8; 16];
        hdr.encode(&mut hdr_full);
        let mut hdr_bytes = [0u8; 14];
        hdr_bytes.copy_from_slice(&hdr_full[..14]);
        let plaintext = [0x42u8; 64];
        let mut ca = plaintext.to_vec();
        let mut cb = plaintext.to_vec();
        let (salt_a, tag_a) = a.encrypt_audio_in_place(&hdr_bytes, &hdr, &mut ca);
        let (salt_b, tag_b) = b.encrypt_audio_in_place(&hdr_bytes, &hdr, &mut cb);
        assert_ne!(salt_a, salt_b);
        assert_ne!(ca, cb, "identical (seq=0,ts=0) packets must have distinct ciphertexts");
        // Each key decrypts the other's ciphertext when the salt travels with it.
        let mut da = ca.clone();
        assert!(b.decrypt_audio_in_place(&hdr_bytes, &hdr, &salt_a, &mut da, &tag_a));
        assert_eq!(da, plaintext);
        let mut db = cb.clone();
        assert!(a.decrypt_audio_in_place(&hdr_bytes, &hdr, &salt_b, &mut db, &tag_b));
        assert_eq!(db, plaintext);
    }

    #[test]
    fn test_feedback_tag_is_domain_separated_and_replay_bound() {
        let key = SessionKey::from_pin("4829");
        let wrong = SessionKey::from_pin("0000");
        let body = [7u8; 20];
        let tag = key.sign_feedback(&body, 1);
        assert!(key.verify_feedback(&body, 1, &tag));
        assert!(!key.verify_feedback(&body, 2, &tag), "feedback seq is bound to tag");
        assert!(!wrong.verify_feedback(&body, 1, &tag));
    }

    #[test]
    fn test_rotate_session_salt_changes_salt_but_preserves_key_identity() {
        let mut key = SessionKey::from_pin("4829");
        let before = key; // SessionKey is Copy
        let original_salt = key.session_salt;

        key.rotate_session_salt();

        assert_ne!(
            key.session_salt, original_salt,
            "rotation must always change the session salt"
        );
        assert_eq!(key, before, "rotation must not change the derived key identity");

        // Repeated rotations keep changing the salt while equality still holds.
        for _ in 0..8 {
            let prev = key.session_salt;
            key.rotate_session_salt();
            assert_ne!(key.session_salt, prev);
            assert_eq!(key, before);
        }
    }
}
