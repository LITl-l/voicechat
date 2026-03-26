use anyhow::{anyhow, Result};
use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

/// X25519 ephemeral key exchange for Perfect Forward Secrecy.
///
/// Each peer generates a fresh X25519 keypair per session. Public keys are
/// exchanged over the PSK-authenticated channel. Shared secrets are derived
/// per peer-pair via X25519 DH, then session keys via HKDF-SHA256.
///
/// Compromising the PSK after a session cannot decrypt past audio, because
/// the ephemeral DH secret is discarded when the session ends.
pub struct KeyExchange {
    secret: StaticSecret,
    public: PublicKey,
}

impl Default for KeyExchange {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyExchange {
    pub fn new() -> Self {
        let secret = StaticSecret::random_from_rng(rand::thread_rng());
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    /// Our public key bytes (32 bytes) to send to peers.
    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.public.to_bytes()
    }

    /// Derive a per-peer 256-bit session key from the peer's public key.
    ///
    /// Uses X25519 DH + HKDF-SHA256. The info string includes both peer IDs
    /// (sorted) so both sides derive the same key regardless of who initiated.
    pub fn derive_peer_key(
        &self,
        peer_public_bytes: &[u8; 32],
        session_id: &[u8; 8],
        local_id: u8,
        peer_id: u8,
    ) -> Result<[u8; 32]> {
        let peer_public = PublicKey::from(*peer_public_bytes);
        let shared_secret = self.secret.diffie_hellman(&peer_public);

        if shared_secret.as_bytes().iter().all(|&b| b == 0) {
            return Err(anyhow!("X25519 shared secret is zero (bad public key)"));
        }

        let (id_low, id_high) = if local_id < peer_id {
            (local_id, peer_id)
        } else {
            (peer_id, local_id)
        };

        let mut info = Vec::with_capacity(17);
        info.extend_from_slice(b"voicechat-audio");
        info.push(id_low);
        info.push(id_high);

        let hk = Hkdf::<Sha256>::new(Some(session_id), shared_secret.as_bytes());
        let mut key = [0u8; 32];
        hk.expand(&info, &mut key)
            .map_err(|e| anyhow!("HKDF expand failed: {e}"))?;

        Ok(key)
    }
}

/// KEY_EXCHANGE packet payload: 32-byte X25519 public key.
#[derive(Clone, Debug)]
pub struct KeyExchangePayload {
    pub public_key: [u8; 32],
}

impl KeyExchangePayload {
    pub fn to_bytes(&self) -> Vec<u8> {
        self.public_key.to_vec()
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() < 32 {
            return Err(anyhow!(
                "KEY_EXCHANGE payload too short: {} bytes",
                data.len()
            ));
        }
        let mut public_key = [0u8; 32];
        public_key.copy_from_slice(&data[..32]);
        Ok(Self { public_key })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_both_sides_derive_same_key() {
        let alice = KeyExchange::new();
        let bob = KeyExchange::new();
        let session_id = [0xAA; 8];

        let alice_key = alice
            .derive_peer_key(&bob.public_key_bytes(), &session_id, 0, 1)
            .unwrap();
        let bob_key = bob
            .derive_peer_key(&alice.public_key_bytes(), &session_id, 1, 0)
            .unwrap();

        assert_eq!(alice_key, bob_key);
    }

    #[test]
    fn test_different_sessions_yield_different_keys() {
        let alice = KeyExchange::new();
        let bob = KeyExchange::new();

        let key1 = alice
            .derive_peer_key(&bob.public_key_bytes(), &[0x01; 8], 0, 1)
            .unwrap();
        let key2 = alice
            .derive_peer_key(&bob.public_key_bytes(), &[0x02; 8], 0, 1)
            .unwrap();

        assert_ne!(key1, key2);
    }

    #[test]
    fn test_payload_roundtrip() {
        let kx = KeyExchange::new();
        let payload = KeyExchangePayload {
            public_key: kx.public_key_bytes(),
        };
        let bytes = payload.to_bytes();
        let parsed = KeyExchangePayload::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.public_key, kx.public_key_bytes());
    }
}
