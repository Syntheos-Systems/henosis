//! Ed25519 request signing for the KLEOSv1 envelope protocol.
//!
//! Every outbound request to Kleos is signed using a canonical envelope that
//! covers the method, path, query, body hash, timestamp, nonce, and a
//! deterministic identity hash derived from the public key and labels.  The
//! resulting signature and metadata travel as `X-Kleos-*` headers.
//!
//! Session tokens issued by Kleos are cached in memory so that subsequent
//! requests skip the full signing round-trip until the server rejects the
//! token with a 401.

use crate::error::GatewayError;
use ed25519_dalek::{SigningKey, VerifyingKey};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use zeroize::Zeroize;

// ------------------------------------------------------------------
// SPKI prefix for an Ed25519 public key (RFC 8410 / DER encoding).
// Total SPKI length = 12 prefix bytes + 32 key bytes = 44 bytes.
// ------------------------------------------------------------------

/// DER prefix that wraps an Ed25519 raw public key into SPKI format.
const SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, // SEQUENCE, 42 bytes
    0x30, 0x05, // SEQUENCE, 5 bytes
    0x06, 0x03, 0x2b, 0x65, 0x70, // OID 1.3.101.112 (id-EdDSA / Ed25519)
    0x03, 0x21, 0x00, // BIT STRING, 33 bytes, 0 unused bits
];

/// Salt constant used by HKDF when deriving the identity hash.
const HKDF_SALT: &[u8] = b"kleos-identity-v1";

// ------------------------------------------------------------------
// Structs
// ------------------------------------------------------------------

/// Secret key material held by the signer.  Zeroized on drop so that the
/// 32-byte scalar is not left in heap memory after the struct is freed.
#[derive(Zeroize)]
#[zeroize(drop)]
struct KeyMaterial {
    /// Raw 32-byte Ed25519 secret scalar.
    bytes: [u8; 32],
}

/// All headers that must be added to an authenticated Kleos request.
pub struct SignedHeaders {
    /// Hex-encoded Ed25519 signature over the canonical envelope.
    pub sig: String,
    /// Algorithm identifier, always "ed25519".
    pub algo: String,
    /// 32-hex-char identity hash for this key + label combination.
    pub identity: String,
    /// Unix timestamp in milliseconds.
    pub ts: String,
    /// 24-hex-char random nonce.
    pub nonce: String,
    /// 64-hex-char SHA-256 fingerprint of the SPKI DER bytes.
    pub key_fp: String,
    /// Host label (e.g. "my-workstation").
    pub host: String,
    /// Agent label (e.g. "syntheos-gateway").
    pub agent: String,
    /// Model label (e.g. "none").
    pub model: String,
}

/// Header application to outbound reqwest builders.
impl SignedHeaders {
    /// Apply all X-Kleos-* headers to a `reqwest::RequestBuilder`, consuming
    /// `self` and returning the augmented builder.
    pub fn apply(self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        rb.header("X-Kleos-Sig", &self.sig)
            .header("X-Kleos-Algo", &self.algo)
            .header("X-Kleos-Identity", &self.identity)
            .header("X-Kleos-Ts", &self.ts)
            .header("X-Kleos-Nonce", &self.nonce)
            .header("X-Kleos-Key-Fp", &self.key_fp)
            .header("X-Kleos-Host", &self.host)
            .header("X-Kleos-Agent", &self.agent)
            .header("X-Kleos-Model", &self.model)
    }
}

/// Ed25519 request signer that implements the KLEOSv1 envelope protocol.
///
/// Holds the signing key (zeroized on drop), precomputed public-key metadata
/// (identity hash, fingerprint, labels), and an in-memory session token cache.
pub struct RequestSigner {
    /// Wrapped signing key; the raw bytes live inside `KeyMaterial` which is
    /// zeroized on drop.
    signing_key: SigningKey,
    /// Storage for the secret scalar so it can be zeroized.
    _key_material: KeyMaterial,
    /// 32-char hex identity hash (HKDF output).
    identity_hash: String,
    /// 64-char hex SHA-256 fingerprint of the SPKI DER.
    fingerprint: String,
    /// Host label supplied at construction time.
    host: String,
    /// Agent label supplied at construction time.
    agent: String,
    /// Model label supplied at construction time.
    model: String,
    /// Optional cached Kleos session token.  Wrapped in `Arc<Mutex<_>>` so
    /// `RequestSigner` can be shared across async tasks via `Arc<RequestSigner>`.
    session: Arc<Mutex<Option<String>>>,
}

// ------------------------------------------------------------------
// Constructor helpers
// ------------------------------------------------------------------

/// Build the 44-byte SPKI DER blob for an Ed25519 verifying key.
fn spki_der(vk: &VerifyingKey) -> [u8; 44] {
    let mut der = [0u8; 44];
    der[..12].copy_from_slice(&SPKI_PREFIX);
    der[12..].copy_from_slice(vk.as_bytes());
    der
}

/// Compute hex(SHA-256(spki_der)) -- the 64-char key fingerprint.
fn fingerprint(der: &[u8]) -> String {
    hex::encode(Sha256::digest(der))
}

/// Derive the 16-byte (32-char hex) identity hash via HKDF-SHA256.
///
/// IKM = SPKI DER bytes; info = "{host}|{agent}|{model}".
fn identity_hash(der: &[u8], host: &str, agent: &str, model: &str) -> String {
    let info = format!("{host}|{agent}|{model}");
    let hk = Hkdf::<Sha256>::new(Some(HKDF_SALT), der);
    let mut out = [0u8; 16];
    hk.expand(info.as_bytes(), &mut out)
        .expect("HKDF expand with 16-byte output never fails");
    hex::encode(out)
}

/// Key construction, loading, signing, and session management.
impl RequestSigner {
    /// Construct a signer from a raw 32-byte Ed25519 secret scalar, plus the
    /// three label strings that parameterise the identity hash.
    pub fn from_key_bytes(secret: [u8; 32], host: &str, agent: &str, model: &str) -> Self {
        let signing_key = SigningKey::from_bytes(&secret);
        let vk = signing_key.verifying_key();
        let der = spki_der(&vk);
        let fp = fingerprint(&der);
        let id_hash = identity_hash(&der, host, agent, model);
        let km = KeyMaterial { bytes: secret };
        Self {
            signing_key,
            _key_material: km,
            identity_hash: id_hash,
            fingerprint: fp,
            host: host.to_string(),
            agent: agent.to_string(),
            model: model.to_string(),
            session: Arc::new(Mutex::new(None)),
        }
    }

    /// Attempt to load a signing key from the environment or well-known file
    /// locations.  Returns `None` (with a warning log) when no key is found,
    /// so the gateway can still start in unauthenticated mode.
    ///
    /// Resolution order:
    /// 1. `KLEOS_IDENTITY_KEY` env var -- 64-char hex encoding of the 32-byte scalar.
    /// 2. `SYNTHEOS_SIGNING_KEY_FILE` env var -- path to a key file.
    /// 3. `~/.kleos/identity.key` -- default location.
    ///
    /// Accepted file formats: raw 32 bytes, 64-char hex (UTF-8), or PEM PKCS8.
    pub fn from_env_or_file(
        host: &str,
        agent: &str,
        model: &str,
    ) -> Result<Option<Self>, GatewayError> {
        // 1. Inline hex via env var.
        if let Ok(hex_val) = std::env::var("KLEOS_IDENTITY_KEY") {
            if !hex_val.is_empty() {
                tracing::warn!(
                    "KLEOS_IDENTITY_KEY is set via environment variable -- \
                     the secret key is visible in /proc/PID/environ to any \
                     process on this host with sufficient privileges. \
                     Consider using SYNTHEOS_SIGNING_KEY_FILE instead."
                );
                let secret = parse_key_hex(&hex_val)?;
                return Ok(Some(Self::from_key_bytes(secret, host, agent, model)));
            }
        }

        // 2. File path from env var.
        if let Ok(path_str) = std::env::var("SYNTHEOS_SIGNING_KEY_FILE") {
            if !path_str.is_empty() {
                let path = PathBuf::from(&path_str);
                match load_key_file(&path)? {
                    Some(secret) => {
                        return Ok(Some(Self::from_key_bytes(secret, host, agent, model)));
                    }
                    None => {
                        tracing::warn!(
                            path = %path.display(),
                            "SYNTHEOS_SIGNING_KEY_FILE is set but the file does not exist"
                        );
                    }
                }
            }
        }

        // 3. Default file location.
        if let Some(home) = dirs_next(host) {
            let default_path = home.join(".kleos").join("identity.key");
            if default_path.exists() {
                if let Some(secret) = load_key_file(&default_path)? {
                    return Ok(Some(Self::from_key_bytes(secret, host, agent, model)));
                }
            }
        }

        tracing::warn!(
            "No Ed25519 signing key found. \
             Set KLEOS_IDENTITY_KEY, SYNTHEOS_SIGNING_KEY_FILE, \
             or place a key at ~/.kleos/identity.key. \
             Requests to Kleos will be unauthenticated."
        );
        Ok(None)
    }

    /// Return the precomputed 32-char hex identity hash.
    pub fn identity_hash(&self) -> &str {
        &self.identity_hash
    }

    /// Return the precomputed 64-char hex key fingerprint.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Build the KLEOSv1 canonical envelope, sign it, and return the set of
    /// `X-Kleos-*` headers that must be attached to the outbound request.
    ///
    /// - `method`: HTTP verb, will be uppercased.
    /// - `path`: URL path component (e.g. "/store").
    /// - `query`: raw query string, may be empty.
    /// - `body`: serialized request body bytes; use `&[]` for GET requests.
    pub fn sign_request(&self, method: &str, path: &str, query: &str, body: &[u8]) -> SignedHeaders {
        let ts_ms = unix_ms();
        let nonce = random_nonce();
        let body_hash = hex::encode(Sha256::digest(body));
        let sorted_query = sort_query(query);

        let envelope = format!(
            "KLEOSv1\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            method.to_uppercase(),
            path,
            sorted_query,
            body_hash,
            ts_ms,
            nonce,
            self.identity_hash,
        );

        use ed25519_dalek::Signer;
        let sig_bytes = self.signing_key.sign(envelope.as_bytes());

        SignedHeaders {
            sig: hex::encode(sig_bytes.to_bytes()),
            algo: "ed25519".to_string(),
            identity: self.identity_hash.clone(),
            ts: ts_ms.to_string(),
            nonce,
            key_fp: self.fingerprint.clone(),
            host: self.host.clone(),
            agent: self.agent.clone(),
            model: self.model.clone(),
        }
    }

    /// Return a clone of the cached session token, if any is stored.
    pub fn cached_session(&self) -> Option<String> {
        self.session
            .lock()
            .expect("session mutex poisoned")
            .clone()
    }

    /// Store a session token received from Kleos in `X-Kleos-Session-Issued`.
    pub fn set_session(&self, token: &str) {
        *self.session.lock().expect("session mutex poisoned") = Some(token.to_string());
    }

    /// Discard the cached session token, forcing the next request to use full
    /// envelope signing.
    pub fn clear_session(&self) {
        *self.session.lock().expect("session mutex poisoned") = None;
    }
}

// ------------------------------------------------------------------
// Internal helpers
// ------------------------------------------------------------------

/// Return the current Unix time as milliseconds since the epoch.
fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the Unix epoch")
        .as_millis() as u64
}

/// Generate 12 random bytes and return them hex-encoded (24 chars).
fn random_nonce() -> String {
    let mut buf = [0u8; 12];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// Normalise a query string for canonical comparison: split on `'&'`, drop
/// empty segments, sort lexicographically, rejoin with `'&'`.
fn sort_query(query: &str) -> String {
    if query.is_empty() {
        return String::new();
    }
    let mut parts: Vec<&str> = query.split('&').filter(|s| !s.is_empty()).collect();
    parts.sort_unstable();
    parts.join("&")
}

/// Decode a 64-char hex string into a 32-byte secret scalar, returning a
/// `GatewayError::Signing` on any parse failure.
fn parse_key_hex(hex_str: &str) -> Result<[u8; 32], GatewayError> {
    let bytes = hex::decode(hex_str.trim())
        .map_err(|e| GatewayError::Signing(format!("invalid hex in key: {e}")))?;
    bytes
        .try_into()
        .map_err(|_| GatewayError::Signing("Ed25519 key must be exactly 32 bytes".to_string()))
}

/// Attempt to read and decode a key file.  Returns `Ok(None)` when the file
/// does not exist, `Ok(Some(...))` on success, or `Err(...)` on a format error.
fn load_key_file(path: &PathBuf) -> Result<Option<[u8; 32]>, GatewayError> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read(path)
        .map_err(|e| GatewayError::Signing(format!("could not read key file {}: {e}", path.display())))?;

    // 32 raw bytes.
    if raw.len() == 32 {
        let arr: [u8; 32] = raw.try_into().expect("checked len == 32");
        return Ok(Some(arr));
    }

    // 64 hex chars (optionally with a trailing newline).
    let trimmed = std::str::from_utf8(&raw)
        .map_err(|e| GatewayError::Signing(format!("key file is not valid UTF-8: {e}")))?
        .trim();

    if trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(Some(parse_key_hex(trimmed)?));
    }

    // PEM PKCS8 (very minimal parser: strip header/footer and base64-decode).
    if trimmed.starts_with("-----BEGIN PRIVATE KEY-----") {
        return decode_pkcs8_pem(trimmed).map(Some);
    }

    Err(GatewayError::Signing(format!(
        "key file {} has an unrecognised format (expected 32 raw bytes, 64-char hex, or PEM PKCS8)",
        path.display()
    )))
}

/// Minimal PKCS8 PEM decoder for Ed25519 private keys.
///
/// Strips PEM armor, base64-decodes the DER body, then extracts the 32-byte
/// private scalar from the `OneAsymmetricKey` structure.  The PKCS8 DER for
/// Ed25519 has the 32-byte scalar at offset 16 (after a fixed 16-byte header).
fn decode_pkcs8_pem(pem: &str) -> Result<[u8; 32], GatewayError> {
    let b64: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect::<Vec<_>>()
        .join("");

    use base64::Engine;
    let der = base64::engine::general_purpose::STANDARD
        .decode(&b64)
        .map_err(|e| GatewayError::Signing(format!("PEM base64 decode error: {e}")))?;

    // Ed25519 PKCS8v1 DER is 48 bytes; the scalar occupies bytes 16..48.
    if der.len() < 48 {
        return Err(GatewayError::Signing(
            "PKCS8 DER too short for Ed25519".to_string(),
        ));
    }
    let scalar: [u8; 32] = der[16..48]
        .try_into()
        .expect("slice is exactly 32 bytes");
    Ok(scalar)
}

/// Resolve the current user's home directory.  The `host` parameter is unused
/// here but kept for future per-host configuration expansion.
fn dirs_next(_host: &str) -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}
