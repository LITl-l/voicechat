use anyhow::{anyhow, Result};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    XChaCha20Poly1305, XNonce,
};

pub const AUTH_TAG_LEN: usize = 16;
const SLIDING_WINDOW_SIZE: u64 = 64;

/// Derives a 256-bit key from a passphrase using Argon2id.
pub fn derive_key_from_passphrase(passphrase: &str) -> Result<[u8; 32]> {
    use argon2::Argon2;

    let salt = b"voicechat-v1\0\0\0\0"; // 16 bytes, fixed salt per spec
    let mut key = [0u8; 32];

    let argon2 = Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        argon2::Params::new(65536, 3, 1, Some(32)).map_err(|e| anyhow!("argon2 params: {e}"))?,
    );

    argon2
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| anyhow!("argon2 hash: {e}"))?;

    Ok(key)
}

/// Constructs a 24-byte XChaCha20 nonce deterministically.
///
/// Layout:
///   session_id (8) || peer_id (1) || direction (1) || seq_num (2) || counter (4) || zeros (8)
fn build_nonce(
    session_id: &[u8; 8],
    peer_id: u8,
    direction: Direction,
    seq_num: u16,
    counter: u32,
) -> [u8; 24] {
    let mut nonce = [0u8; 24];
    nonce[0..8].copy_from_slice(session_id);
    nonce[8] = peer_id;
    nonce[9] = direction as u8;
    nonce[10..12].copy_from_slice(&seq_num.to_be_bytes());
    nonce[12..16].copy_from_slice(&counter.to_be_bytes());
    // bytes 16..24 remain zero (padding)
    nonce
}

#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub enum Direction {
    Send = 0x00,
    Recv = 0x01,
}

/// Per-session encryption context.
pub struct CryptoContext {
    cipher: XChaCha20Poly1305,
    session_id: [u8; 8],
    /// Monotonic send counter (separate from seq_num, never wraps in practice for u32).
    send_counter: u32,
}

impl CryptoContext {
    pub fn new(key: &[u8; 32], session_id: [u8; 8]) -> Self {
        let cipher = XChaCha20Poly1305::new(key.into());
        Self {
            cipher,
            session_id,
            send_counter: 0,
        }
    }

    /// Reserve and return the next monotonic counter value.
    ///
    /// The caller must stamp this value into the `counter` field of the
    /// packet header *before* calling [`CryptoContext::encrypt`], so the
    /// AAD and the XChaCha20 nonce agree on the same counter. The receiver
    /// then reads `header.counter` to reconstruct the nonce.
    pub fn next_counter(&mut self) -> Result<u32> {
        let counter = self.send_counter;
        self.send_counter = self
            .send_counter
            .checked_add(1)
            .ok_or_else(|| anyhow!("send counter overflow"))?;
        Ok(counter)
    }

    /// Encrypts a packet payload.
    ///
    /// `header` (8 bytes) is sent in cleartext but authenticated (AAD). The
    /// caller must have already placed `counter` into the `counter` field of
    /// the header so the receiver can reconstruct the nonce.
    ///
    /// `payload` is the plaintext to encrypt (Opus data for audio, or a
    /// control-packet payload).
    pub fn encrypt(
        &self,
        header: &[u8; 8],
        payload: &[u8],
        peer_id: u8,
        seq_num: u16,
        counter: u32,
    ) -> Result<Vec<u8>> {
        let nonce_bytes = build_nonce(&self.session_id, peer_id, Direction::Send, seq_num, counter);
        let nonce = XNonce::from_slice(&nonce_bytes);

        let ciphertext = self
            .cipher
            .encrypt(
                nonce,
                Payload {
                    msg: payload,
                    aad: header,
                },
            )
            .map_err(|e| anyhow!("encrypt failed: {e}"))?;

        Ok(ciphertext)
    }

    /// Decrypts and authenticates an audio packet.
    ///
    /// `header` is the cleartext 8-byte header (used as AAD).
    /// `ciphertext_with_tag` is the encrypted payload + 16-byte auth tag.
    pub fn decrypt(
        &self,
        header: &[u8; 8],
        ciphertext_with_tag: &[u8],
        peer_id: u8,
        seq_num: u16,
        counter: u32,
    ) -> Result<Vec<u8>> {
        // Reconstruct the sender's nonce: same peer_id + Direction::Send
        let nonce_bytes = build_nonce(&self.session_id, peer_id, Direction::Send, seq_num, counter);
        let nonce = XNonce::from_slice(&nonce_bytes);

        self.cipher
            .decrypt(
                nonce,
                Payload {
                    msg: ciphertext_with_tag,
                    aad: header,
                },
            )
            .map_err(|e| anyhow!("decrypt/auth failed: {e}"))
    }
}

/// Sliding window replay protection per peer.
pub struct ReplayFilter {
    /// Highest accepted counter value.
    highest: u64,
    /// Bitmap for the window [highest - SLIDING_WINDOW_SIZE + 1 .. highest].
    /// Bit i represents counter value (highest - i).
    bitmap: u64,
}

impl Default for ReplayFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl ReplayFilter {
    pub fn new() -> Self {
        Self {
            highest: 0,
            bitmap: 0,
        }
    }

    /// Returns true if the counter is acceptable (not replayed).
    /// Updates internal state if accepted.
    pub fn check_and_accept(&mut self, counter: u32) -> bool {
        let counter = counter as u64;

        if counter > self.highest {
            let shift = counter - self.highest;
            if shift >= SLIDING_WINDOW_SIZE {
                self.bitmap = 0;
            } else {
                self.bitmap <<= shift;
            }
            self.bitmap |= 1; // mark the new highest as seen
            self.highest = counter;
            true
        } else {
            let diff = self.highest - counter;
            if diff >= SLIDING_WINDOW_SIZE {
                return false; // too old
            }
            let bit = 1u64 << diff;
            if self.bitmap & bit != 0 {
                return false; // duplicate
            }
            self.bitmap |= bit;
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = derive_key_from_passphrase("test-password").unwrap();
        let session_id = [1u8; 8];
        let mut sender = CryptoContext::new(&key, session_id);
        let receiver = CryptoContext::new(&key, session_id);

        let counter = sender.next_counter().unwrap();
        // Header carries the counter so the AAD matches on both sides.
        let header = [0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, counter as u8];
        let payload = b"hello opus data";

        let ciphertext = sender.encrypt(&header, payload, 0, 1, counter).unwrap();
        let decrypted = receiver
            .decrypt(&header, &ciphertext, 0, 1, counter)
            .unwrap();

        assert_eq!(decrypted, payload);
    }

    #[test]
    fn test_tampered_header_fails() {
        let key = derive_key_from_passphrase("test-password").unwrap();
        let session_id = [1u8; 8];
        let mut sender = CryptoContext::new(&key, session_id);
        let receiver = CryptoContext::new(&key, session_id);

        let counter = sender.next_counter().unwrap();
        let header = [0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, counter as u8];
        let payload = b"hello opus data";

        let ciphertext = sender.encrypt(&header, payload, 0, 1, counter).unwrap();

        let mut bad_header = header;
        bad_header[1] = 0xFF;
        assert!(receiver
            .decrypt(&bad_header, &ciphertext, 0, 1, counter)
            .is_err());
    }

    #[test]
    fn test_counter_mismatch_fails() {
        // Regression test: if sender and receiver use different counters,
        // decrypt must fail (this was the bug that silently dropped every
        // audio packet after the first non-audio send).
        let key = derive_key_from_passphrase("test-password").unwrap();
        let session_id = [2u8; 8];
        let mut sender = CryptoContext::new(&key, session_id);
        let receiver = CryptoContext::new(&key, session_id);

        let counter = sender.next_counter().unwrap();
        let header = [0x01, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, counter as u8];
        let payload = b"audio frame";

        let ciphertext = sender.encrypt(&header, payload, 0, 5, counter).unwrap();
        assert!(receiver
            .decrypt(&header, &ciphertext, 0, 5, counter.wrapping_add(1))
            .is_err());
    }

    #[test]
    fn test_replay_filter() {
        let mut filter = ReplayFilter::new();
        assert!(filter.check_and_accept(1));
        assert!(filter.check_and_accept(2));
        assert!(!filter.check_and_accept(1)); // duplicate
        assert!(filter.check_and_accept(3));
        assert!(filter.check_and_accept(100)); // big jump
        assert!(!filter.check_and_accept(30)); // too old (100 - 30 = 70 > 64)
        assert!(filter.check_and_accept(50)); // within window
        assert!(!filter.check_and_accept(50)); // duplicate
    }
}
