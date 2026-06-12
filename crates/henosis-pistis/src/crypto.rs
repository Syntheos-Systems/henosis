//! Ed25519 identity primitive: the keypair every layer of the Pistis trust
//! chain (master, persona, session, grant) is built on.
//!
//! An independent snapshot of `pistis-core::identity`, reworked for Henosis:
//! the secret bytes live inside a [`zeroize::Zeroizing`] wrapper (Henosis
//! already depends on `zeroize`) rather than `secrecy::Secret`, and keypair
//! generation draws 32 random bytes via `rand` then constructs the signing key
//! directly -- sidestepping the rand_core 0.6/0.9 trait-version mismatch that
//! `SigningKey::generate(&mut OsRng)` would otherwise force on the workspace.

use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::error::{PistisError, Result};

/// Public verification key (Ed25519). Safe to share on the wire.
///
/// `PartialOrd`/`Ord` are derived over the raw `bytes`, giving a stable
/// lexicographic ordering required for `BTreeSet<PublicKey>` (the trusted-roots
/// and revoked-key sets).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PublicKey {
    /// Raw 32-byte Ed25519 verifying key.
    pub bytes: [u8; 32],
}

impl PublicKey {
    /// Verify `sig` over `msg`. `Err(SignatureInvalid)` on a corrupt key or a
    /// signature that does not validate against the key+message pair.
    pub fn verify(&self, msg: &[u8], sig: &Signature) -> Result<()> {
        let vk = VerifyingKey::from_bytes(&self.bytes)
            .map_err(|e| PistisError::SignatureInvalid(format!("bad pubkey: {e}")))?;
        let s = ed25519_dalek::Signature::from_bytes(&sig.bytes);
        vk.verify(msg, &s)
            .map_err(|_| PistisError::SignatureInvalid("verification failed".into()))
    }
}

/// Ed25519 signature (64 bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    /// Raw 64-byte signature.
    pub bytes: [u8; 64],
}

/// Private signing key. The 32 secret bytes live in a `Zeroizing` wrapper that
/// wipes them on drop; they are exposed only briefly inside `sign()` and
/// `public_key()` to reconstruct the in-memory signing key.
pub struct SecretKey {
    /// Zeroizing wrapper around the raw Ed25519 secret-key bytes.
    inner: Zeroizing<[u8; 32]>,
}

impl SecretKey {
    /// Generate a fresh Ed25519 keypair.
    ///
    /// Draws 32 random bytes from the `rand` CSPRNG and constructs the signing
    /// key from them, rather than `SigningKey::generate(&mut OsRng)`, to avoid
    /// pinning a specific `rand_core` trait version into the workspace.
    pub fn generate() -> (PublicKey, SecretKey) {
        let secret_bytes: [u8; 32] = rand::random();
        let signing = SigningKey::from_bytes(&secret_bytes);
        let public = PublicKey {
            bytes: signing.verifying_key().to_bytes(),
        };
        (
            public,
            SecretKey {
                inner: Zeroizing::new(secret_bytes),
            },
        )
    }

    /// Reconstruct a `SecretKey` from raw bytes (e.g. loaded from a vault). The
    /// input array is zeroized before return; callers must not reuse it.
    pub fn from_bytes(mut bytes: [u8; 32]) -> SecretKey {
        let secret = SecretKey {
            inner: Zeroizing::new(bytes),
        };
        // `Zeroizing::new` copied the bytes; wipe the caller's array too.
        use zeroize::Zeroize;
        bytes.zeroize();
        secret
    }

    /// Sign `msg`. The secret bytes are exposed only inside this call.
    pub fn sign(&self, msg: &[u8]) -> Signature {
        let signing = SigningKey::from_bytes(&self.inner);
        let sig = signing.sign(msg);
        Signature {
            bytes: sig.to_bytes(),
        }
    }

    /// Derive the public key from this secret.
    pub fn public_key(&self) -> PublicKey {
        let signing = SigningKey::from_bytes(&self.inner);
        PublicKey {
            bytes: signing.verifying_key().to_bytes(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A generated keypair signs and verifies its own message.
    #[test]
    fn generate_and_verify_roundtrip() {
        let (pk, sk) = SecretKey::generate();
        let msg = b"pistis test message";
        let sig = sk.sign(msg);
        pk.verify(msg, &sig).unwrap();
    }

    /// Verification rejects a tampered message.
    #[test]
    fn verify_rejects_tampered_message() {
        let (pk, sk) = SecretKey::generate();
        let sig = sk.sign(b"original");
        assert!(matches!(
            pk.verify(b"tampered", &sig),
            Err(PistisError::SignatureInvalid(_))
        ));
    }

    /// Verification rejects a signature under the wrong public key.
    #[test]
    fn verify_rejects_wrong_pubkey() {
        let (_pk1, sk1) = SecretKey::generate();
        let (pk2, _sk2) = SecretKey::generate();
        let sig = sk1.sign(b"msg");
        assert!(matches!(
            pk2.verify(b"msg", &sig),
            Err(PistisError::SignatureInvalid(_))
        ));
    }

    /// The derived public key matches the one returned at generation.
    #[test]
    fn derived_public_key_matches_generated() {
        let (pk_gen, sk) = SecretKey::generate();
        assert_eq!(pk_gen, sk.public_key());
    }

    /// A key reconstructed from raw bytes derives the expected public key and
    /// signs verifiably.
    #[test]
    fn from_bytes_reconstructs_signing_key() {
        let raw: [u8; 32] = rand::random();
        let expected = SigningKey::from_bytes(&raw).verifying_key().to_bytes();
        let sk = SecretKey::from_bytes(raw);
        assert_eq!(sk.public_key().bytes, expected);
        let sig = sk.sign(b"hello");
        sk.public_key().verify(b"hello", &sig).unwrap();
    }
}
