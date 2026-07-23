//! Field-level secret encryption with AES-256-GCM.
//!
//! Each secret value is encrypted independently. The caller supplies an already-derived 32-byte
//! master key when constructing [`crate::CredentialStore`]. This module only owns raw-byte
//! encryption, decryption, and test-key generation.

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use rand::rngs::OsRng;
use rand::TryRngCore;
use zeroize::Zeroizing;

use crate::error::CredentialStoreError;

/// AES-256-GCM nonce size (96 bits).
pub const NONCE_SIZE: usize = 12;

/// AES-256 key size (256 bits).
pub const KEY_SIZE: usize = 32;

/// GCM authentication tag size (128 bits). A ciphertext shorter than nonce+tag cannot be valid.
const TAG_SIZE: usize = 16;

/// Encrypt `plaintext`, returning `nonce || ciphertext+tag` as one blob.
///
/// A fresh random nonce is drawn per call, so encrypting identical plaintext twice yields
/// distinct blobs.
pub fn encrypt(key: &[u8; KEY_SIZE], plaintext: &[u8]) -> Result<Vec<u8>, CredentialStoreError> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| CredentialStoreError::Encryption(format!("invalid key: {e}")))?;

    let mut nonce_bytes = [0u8; NONCE_SIZE];
    OsRng
        .try_fill_bytes(&mut nonce_bytes)
        .expect("OS CSPRNG must be available");
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| CredentialStoreError::Encryption(format!("encryption failed: {e}")))?;

    let mut out = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypt a `nonce || ciphertext+tag` blob produced by [`encrypt`].
///
/// The returned plaintext is wrapped in [`Zeroizing`] so it is scrubbed from the heap on drop.
/// The error is intentionally opaque: it distinguishes nothing about why authentication failed
/// and never echoes key or plaintext material.
pub fn decrypt(
    key: &[u8; KEY_SIZE],
    blob: &[u8],
) -> Result<Zeroizing<Vec<u8>>, CredentialStoreError> {
    if blob.len() < NONCE_SIZE + TAG_SIZE {
        return Err(CredentialStoreError::Decryption(
            "ciphertext too short".into(),
        ));
    }
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| CredentialStoreError::Decryption(format!("invalid key: {e}")))?;

    let nonce = Nonce::from_slice(&blob[..NONCE_SIZE]);
    let ciphertext = &blob[NONCE_SIZE..];

    cipher
        .decrypt(nonce, ciphertext)
        .map(Zeroizing::new)
        .map_err(|_| CredentialStoreError::Decryption("authentication failed".into()))
}

/// Generate a fresh random 256-bit master key. Used by tests and by the (future) provisioning
/// path; production keys come from the environment or a YubiKey.
pub fn generate_key() -> Zeroizing<[u8; KEY_SIZE]> {
    let mut key = [0u8; KEY_SIZE];
    OsRng
        .try_fill_bytes(&mut key)
        .expect("OS CSPRNG must be available");
    Zeroizing::new(key)
}

#[cfg(test)]
/// Verifies encryption round trips and authenticated-decryption failure behavior.
mod tests {
    use super::*;

    /// Round-trip: decrypt(encrypt(x)) == x, and a fresh nonce makes ciphertexts differ.
    #[test]
    fn encrypt_decrypt_round_trip() {
        let key = generate_key();
        let plaintext = b"super-secret value";
        let blob1 = encrypt(&key, plaintext).expect("encrypt");
        let blob2 = encrypt(&key, plaintext).expect("encrypt");
        assert_ne!(blob1, blob2, "fresh nonce per call");
        assert_eq!(&*decrypt(&key, &blob1).expect("decrypt"), plaintext);
        assert_eq!(&*decrypt(&key, &blob2).expect("decrypt"), plaintext);
    }

    /// The wrong key fails authentication rather than returning garbage plaintext.
    #[test]
    fn wrong_key_fails_authentication() {
        let key = generate_key();
        let other = generate_key();
        let blob = encrypt(&key, b"x").expect("encrypt");
        assert!(decrypt(&other, &blob).is_err());
    }

    /// A flipped ciphertext byte fails the GCM tag check.
    #[test]
    fn tampered_ciphertext_fails() {
        let key = generate_key();
        let mut blob = encrypt(&key, b"tamper me").expect("encrypt");
        let last = blob.len() - 1;
        blob[last] ^= 0x01;
        assert!(decrypt(&key, &blob).is_err());
    }

    /// A truncated blob (shorter than nonce+tag) is rejected, not panicked on.
    #[test]
    fn short_blob_rejected() {
        let key = generate_key();
        assert!(decrypt(&key, &[0u8; 4]).is_err());
    }
}
