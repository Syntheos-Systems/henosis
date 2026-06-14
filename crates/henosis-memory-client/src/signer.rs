use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::error::{MemoryClientError, Result};

// --- Signature algorithm enum ---

/// Identifies the signing algorithm used in a request signature header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureAlgo {
    /// ECDSA over P-256 (retained for header parsing; not used for signing in this crate).
    EcdsaP256,
    /// Ed25519 Schnorr signature.
    Ed25519,
}

/// Methods for `SignatureAlgo`.
impl SignatureAlgo {
    /// Return the canonical string representation for use in headers.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::EcdsaP256 => "ecdsa-p256",
            Self::Ed25519 => "ed25519",
        }
    }
}

// --- Canonical envelope ---

/// Constructs the canonical signed envelope matching the KLEOSv1 scheme.
pub struct CanonicalEnvelope {
    /// HTTP method, uppercased.
    method: String,
    /// Request path component.
    path: String,
    /// Query string, with parameters sorted for determinism.
    query: String,
    /// SHA-256 hex digest of the request body.
    body_hash: String,
    /// Request timestamp in milliseconds since the Unix epoch.
    timestamp_ms: u64,
    /// Hex-encoded random nonce.
    nonce: String,
    /// Hex-encoded HKDF-derived identity hash.
    identity_hash: String,
}

/// Methods for `CanonicalEnvelope`.
impl CanonicalEnvelope {
    /// Build a canonical envelope from request components.
    pub fn new(
        method: &str,
        path: &str,
        query: &str,
        body: &[u8],
        timestamp_ms: u64,
        nonce_hex: &str,
        identity_hash_hex: &str,
    ) -> Self {
        let body_hash = hex::encode(Sha256::digest(body));
        let mut sorted_query = query
            .split('&')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        sorted_query.sort();
        Self {
            method: method.to_ascii_uppercase(),
            path: path.to_string(),
            query: sorted_query.join("&"),
            body_hash,
            timestamp_ms,
            nonce: nonce_hex.to_string(),
            identity_hash: identity_hash_hex.to_string(),
        }
    }

    /// Serialize to the byte string that is signed/verified.
    pub fn build(&self) -> Vec<u8> {
        format!(
            "KLEOSv1\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            self.method,
            self.path,
            self.query,
            self.body_hash,
            self.timestamp_ms,
            self.nonce,
            self.identity_hash,
        )
        .into_bytes()
    }
}

// --- HKDF identity derivation ---

/// Derive a 16-byte identity hash via HKDF-SHA256, keyed by the public key DER bytes.
pub fn derive_identity_hash(pubkey_der: &[u8], host: &str, agent: &str, model: &str) -> [u8; 16] {
    use hkdf::Hkdf;
    let hk = Hkdf::<Sha256>::new(Some(b"kleos-identity-v1"), pubkey_der);
    let info = format!("{host}|{agent}|{model}");
    let mut out = [0u8; 16];
    hk.expand(info.as_bytes(), &mut out)
        .expect("16 bytes is a valid HKDF-SHA256 output length");
    out
}

/// Return the HKDF identity hash as a lowercase hex string.
pub fn identity_hash_hex(pubkey_der: &[u8], host: &str, agent: &str, model: &str) -> String {
    hex::encode(derive_identity_hash(pubkey_der, host, agent, model))
}

// --- Nonce generation ---

/// Generate a cryptographically random 12-byte nonce, hex-encoded.
pub fn generate_nonce() -> String {
    let mut buf = [0u8; 12];
    use rand::rngs::OsRng;
    use rand::TryRngCore;
    OsRng
        .try_fill_bytes(&mut buf)
        .expect("OS CSPRNG must be available");
    hex::encode(buf)
}

// --- Signing backend (software Ed25519 only) ---

/// Internal signing backend -- software Ed25519 only.
enum SigningBackend {
    /// Software Ed25519 signing key.
    Ed25519(ed25519_dalek::SigningKey),
}

/// Ed25519 SubjectPublicKeyInfo 12-byte DER prefix (OID 1.3.101.112).
const ED25519_SPKI_PREFIX_CONST: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// HTTP request signer using a software Ed25519 identity key.
pub struct RequestSigner {
    /// The active signing backend.
    backend: SigningBackend,
    /// Signing algorithm in use.
    algo: SignatureAlgo,
    /// PEM-encoded public key.
    pubkey_pem: String,
    /// DER-encoded public key bytes.
    pubkey_der: Vec<u8>,
    /// SHA-256 fingerprint of the public key DER.
    fingerprint: String,
    /// Host label embedded in the identity hash.
    host_label: String,
    /// Agent label embedded in the identity hash.
    agent_label: String,
    /// Model label embedded in the identity hash.
    model_label: String,
    /// HKDF-derived identity hash (hex).
    identity_hash: String,
    /// In-process cached session token.
    session_token: Mutex<Option<String>>,
}

/// Methods for `RequestSigner`.
impl RequestSigner {
    /// Construct a signer from a raw 32-byte Ed25519 secret key.
    ///
    /// The local copy of `secret` is zeroized once the `SigningKey` (which owns
    /// its own zeroize-on-drop copy) is built. An external caller that passes a
    /// secret in is responsible for scrubbing its own copy.
    pub fn from_key_bytes(mut secret: [u8; 32], host: &str, agent: &str, model: &str) -> Self {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&secret);
        use zeroize::Zeroize;
        secret.zeroize();
        Self::from_signing_key(signing_key, host, agent, model)
    }

    /// Build a signer from an already-constructed signing key. Shared by the
    /// raw-bytes loader and the validated-PKCS8 loader so neither re-derives the
    /// public material differently.
    fn from_signing_key(
        signing_key: ed25519_dalek::SigningKey,
        host: &str,
        agent: &str,
        model: &str,
    ) -> Self {
        let vk = signing_key.verifying_key();

        let mut der = Vec::with_capacity(44);
        der.extend_from_slice(&ED25519_SPKI_PREFIX_CONST);
        der.extend_from_slice(vk.as_bytes());

        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&der);
        let pubkey_pem = format!("-----BEGIN PUBLIC KEY-----\n{b64}\n-----END PUBLIC KEY-----");

        let fingerprint = hex::encode(Sha256::digest(&der));
        let identity_hash = identity_hash_hex(&der, host, agent, model);

        Self {
            backend: SigningBackend::Ed25519(signing_key),
            algo: SignatureAlgo::Ed25519,
            pubkey_pem,
            pubkey_der: der,
            fingerprint,
            host_label: host.to_string(),
            agent_label: agent.to_string(),
            model_label: model.to_string(),
            identity_hash,
            session_token: Mutex::new(None),
        }
    }

    /// Load a signer from a key file. Accepts raw 32-byte binary, PKCS8 PEM, or hex.
    pub fn from_file(path: &std::path::Path, host: &str, agent: &str, model: &str) -> Result<Self> {
        // The file holds raw private-key material. Every heap buffer that
        // touches the secret (file bytes, decoded DER, hex-decoded bytes) is
        // wrapped in Zeroizing so it is scrubbed when this function returns.
        let raw = zeroize::Zeroizing::new(std::fs::read(path).map_err(|e| {
            MemoryClientError::Internal(format!("cannot read identity key {}: {e}", path.display()))
        })?);

        use zeroize::Zeroize;

        if raw.len() == 32 {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&raw);
            let signer = Self::from_key_bytes(arr, host, agent, model);
            arr.zeroize();
            return Ok(signer);
        }

        let text = std::str::from_utf8(&raw).map_err(|_| {
            MemoryClientError::InvalidInput(
                "identity key file is not valid UTF-8 or 32-byte raw".into(),
            )
        })?;

        if text.contains("PRIVATE KEY") {
            // Parse PKCS8 with a real ASN.1 decoder rather than slicing a fixed
            // offset: a hardcoded `der[16..48]` silently extracts wrong bytes
            // from a PKCS8v2 key (which carries a public-key appendix) and
            // yields a key that signs but never verifies. `from_pkcs8_pem`
            // validates the structure and rejects anything malformed.
            use ed25519_dalek::pkcs8::DecodePrivateKey;
            let signing_key = ed25519_dalek::SigningKey::from_pkcs8_pem(text).map_err(|e| {
                MemoryClientError::InvalidInput(format!("invalid Ed25519 PKCS8 private key: {e}"))
            })?;
            return Ok(Self::from_signing_key(signing_key, host, agent, model));
        }

        let decoded = zeroize::Zeroizing::new(hex::decode(text.trim()).map_err(|_| {
            MemoryClientError::InvalidInput(
                "identity key file is not 32-byte raw, PEM, or hex".into(),
            )
        })?);
        if decoded.len() != 32 {
            return Err(MemoryClientError::InvalidInput(format!(
                "hex-encoded key must be 32 bytes, got {}",
                decoded.len()
            )));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&decoded);
        let signer = Self::from_key_bytes(arr, host, agent, model);
        arr.zeroize();
        Ok(signer)
    }

    /// Try to load a signer from environment variables or well-known file paths.
    ///
    /// Resolution order:
    /// 1. `KLEOS_IDENTITY_KEY` env var (hex-encoded 32-byte secret).
    /// 2. `KLEOS_IDENTITY_KEY_FILE` env var (path to key file).
    /// 3. `~/.kleos/identity.key`.
    ///
    /// Returns `Ok(None)` when no key material is found.
    pub fn from_env_or_file(host: &str, agent: &str, model: &str) -> Result<Option<Self>> {
        // T2: Software Ed25519 key from env var or file
        if let Ok(hex_key) = std::env::var("KLEOS_IDENTITY_KEY") {
            use zeroize::Zeroize;
            let mut bytes = hex::decode(hex_key.trim()).map_err(|e| {
                MemoryClientError::InvalidInput(format!("KLEOS_IDENTITY_KEY bad hex: {e}"))
            })?;
            if bytes.len() != 32 {
                bytes.zeroize();
                return Err(MemoryClientError::InvalidInput(format!(
                    "KLEOS_IDENTITY_KEY must be 32 bytes, got {}",
                    bytes.len()
                )));
            }
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes);
            bytes.zeroize();
            let signer = Self::from_key_bytes(arr, host, agent, model);
            arr.zeroize();
            return Ok(Some(signer));
        }

        let key_path = if let Ok(p) = std::env::var("KLEOS_IDENTITY_KEY_FILE") {
            std::path::PathBuf::from(p)
        } else if let Some(home) = dirs_for_key_path() {
            home.join(".kleos").join("identity.key")
        } else {
            return Ok(None);
        };

        if key_path.exists() {
            Ok(Some(Self::from_file(&key_path, host, agent, model)?))
        } else {
            Ok(None)
        }
    }

    /// Generate a fresh keypair and write it to `~/.kleos/identity.key`.
    /// Fails if a key already exists at that path.
    pub fn generate_software_key(
        host: &str,
        agent: &str,
        model: &str,
    ) -> Result<(Self, std::path::PathBuf)> {
        let home = dirs_for_key_path()
            .ok_or_else(|| MemoryClientError::Internal("cannot determine home directory".into()))?;
        let kleos_dir = home.join(".kleos");
        std::fs::create_dir_all(&kleos_dir)
            .map_err(|e| MemoryClientError::Internal(format!("cannot create ~/.kleos: {e}")))?;
        // Tighten the key directory to owner-only so a sibling secret cannot be
        // read via a permissive parent dir. Best-effort, never fatal.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&kleos_dir, std::fs::Permissions::from_mode(0o700));
        }
        let key_path = kleos_dir.join("identity.key");
        if key_path.exists() {
            return Err(MemoryClientError::InvalidInput(format!(
                "software key already exists at {}; remove it first to regenerate",
                key_path.display()
            )));
        }

        let mut secret = [0u8; 32];
        use rand::rngs::OsRng;
        use rand::TryRngCore;
        use zeroize::Zeroize;
        OsRng
            .try_fill_bytes(&mut secret)
            .expect("OS CSPRNG must be available");

        // Atomic 0600 creation: no world-readable window between write and chmod.
        // The hex encoding holds the secret in plaintext, so scrub it once written.
        let hex_secret = zeroize::Zeroizing::new(hex::encode(secret));
        write_owner_only(&key_path, hex_secret.as_bytes())
            .map_err(|e| MemoryClientError::Internal(format!("cannot write key file: {e}")))?;

        let signer = Self::from_key_bytes(secret, host, agent, model);
        secret.zeroize();
        Ok((signer, key_path))
    }

    /// Signs the legacy nonce-less enrollment proof.
    pub fn sign_enrollment_proof(&self) -> Result<String> {
        let proof_msg = format!(
            "KLEOS-ENROLL:{}:{}:{}:{}",
            self.algo.as_str(),
            self.tier(),
            self.host_label,
            self.pubkey_pem,
        );
        self.sign_proof_message(&proof_msg)
    }

    /// Signs an enrollment proof bound to a server-issued single-use challenge nonce.
    pub fn sign_enrollment_proof_with_nonce(&self, nonce: &str) -> Result<String> {
        let proof_msg = format!(
            "KLEOS-ENROLL:{}:{}:{}:{}:{}",
            self.algo.as_str(),
            self.tier(),
            self.host_label,
            self.pubkey_pem,
            nonce,
        );
        self.sign_proof_message(&proof_msg)
    }

    /// Signs an enrollment proof message with the active backend.
    fn sign_proof_message(&self, proof_msg: &str) -> Result<String> {
        match &self.backend {
            SigningBackend::Ed25519(sk) => {
                use ed25519_dalek::Signer;
                Ok(hex::encode(sk.sign(proof_msg.as_bytes()).to_bytes()))
            }
        }
    }

    /// Return the signing algorithm for this signer.
    pub fn algo(&self) -> SignatureAlgo {
        self.algo
    }

    /// Return the auth tier label (always "soft" in this crate).
    pub fn tier(&self) -> &'static str {
        match &self.backend {
            SigningBackend::Ed25519(_) => "soft",
        }
    }

    /// Return the PEM-encoded public key.
    pub fn pubkey_pem(&self) -> &str {
        &self.pubkey_pem
    }

    /// Return the DER-encoded public key bytes.
    pub fn pubkey_der(&self) -> &[u8] {
        &self.pubkey_der
    }

    /// Return the SHA-256 fingerprint of the public key.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Return the raw Ed25519 secret key bytes (32 bytes).
    pub fn ed25519_secret_bytes(&self) -> Option<[u8; 32]> {
        match &self.backend {
            SigningBackend::Ed25519(sk) => Some(sk.to_bytes()),
        }
    }

    /// Return a reference to the soft Ed25519 signing key.
    pub fn soft_signing_key(&self) -> Option<&ed25519_dalek::SigningKey> {
        match &self.backend {
            SigningBackend::Ed25519(sk) => Some(sk),
        }
    }

    /// Return the derived identity hash for this signer.
    pub fn identity_hash(&self) -> &str {
        &self.identity_hash
    }

    /// Return the host label.
    pub fn host_label(&self) -> &str {
        &self.host_label
    }

    /// Return the agent label.
    pub fn agent_label(&self) -> &str {
        &self.agent_label
    }

    /// Return the model label.
    pub fn model_label(&self) -> &str {
        &self.model_label
    }

    /// Path to the on-disk session cache for this identity. Scoped by
    /// `identity_hash` so distinct agents/models never share a token, placed in
    /// `$XDG_RUNTIME_DIR` (tmpfs, cleared on logout) and falling back to a
    /// user-private temp dir.
    fn session_file_path(&self) -> Option<std::path::PathBuf> {
        let base = std::env::var("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            .ok()
            .or_else(dirs::cache_dir)
            .unwrap_or_else(std::env::temp_dir);
        Some(base.join(format!("kleos-session-{}", &self.identity_hash)))
    }

    /// Returns the cached session token: the in-process copy if present,
    /// otherwise the on-disk copy (which is then promoted into memory).
    pub fn cached_session(&self) -> Option<String> {
        if let Some(tok) = self.session_token.lock().unwrap().clone() {
            return Some(tok);
        }
        let path = self.session_file_path()?;
        let tok = std::fs::read_to_string(&path).ok()?;
        let tok = tok.trim();
        if tok.is_empty() {
            return None;
        }
        *self.session_token.lock().unwrap() = Some(tok.to_string());
        Some(tok.to_string())
    }

    /// Caches a session token both in process and on disk (0600).
    pub fn set_session(&self, token: String) {
        if let Some(path) = self.session_file_path() {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // Atomic 0600 creation: no world-readable window. Best-effort,
            // never fatal -- an unwritable cache just means the next request
            // re-signs instead of reusing the token.
            let _ = write_owner_only(&path, token.as_bytes());
        }
        *self.session_token.lock().unwrap() = Some(token);
    }

    /// Clears the session token from memory and disk. Called on a 401 so a
    /// stale on-disk token self-heals.
    pub fn clear_session(&self) {
        if let Some(path) = self.session_file_path() {
            let _ = std::fs::remove_file(&path);
        }
        *self.session_token.lock().unwrap() = None;
    }

    /// Sign an HTTP request, returning a `SignedRequest` with all required headers.
    pub fn sign_request(
        &self,
        method: &str,
        path: &str,
        query: &str,
        body: &[u8],
    ) -> Result<SignedRequest> {
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let nonce = generate_nonce();

        let envelope = CanonicalEnvelope::new(
            method,
            path,
            query,
            body,
            ts_ms,
            &nonce,
            &self.identity_hash,
        );
        let msg = envelope.build();

        let sig_hex = match &self.backend {
            SigningBackend::Ed25519(sk) => {
                use ed25519_dalek::Signer;
                hex::encode(sk.sign(&msg).to_bytes())
            }
        };

        Ok(SignedRequest {
            sig_hex,
            algo: self.algo,
            identity_hash: self.identity_hash.clone(),
            ts_ms,
            nonce,
            key_fp: self.fingerprint.clone(),
            host_label: self.host_label.clone(),
            agent_label: self.agent_label.clone(),
            model_label: self.model_label.clone(),
        })
    }

    /// Generate a fresh Ed25519 keypair and return the raw secret bytes and PEM-encoded public key.
    pub fn generate_keypair() -> ([u8; 32], String) {
        let mut secret = [0u8; 32];
        use rand::rngs::OsRng;
        use rand::TryRngCore;
        OsRng
            .try_fill_bytes(&mut secret)
            .expect("OS CSPRNG must be available");
        let sk = ed25519_dalek::SigningKey::from_bytes(&secret);
        let vk = sk.verifying_key();

        let mut der = Vec::with_capacity(44);
        der.extend_from_slice(&ED25519_SPKI_PREFIX_CONST);
        der.extend_from_slice(vk.as_bytes());
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&der);
        let pubkey_pem = format!("-----BEGIN PUBLIC KEY-----\n{b64}\n-----END PUBLIC KEY-----");

        (secret, pubkey_pem)
    }
}

/// A signed request ready to have its headers applied to a `reqwest::RequestBuilder`.
pub struct SignedRequest {
    /// Hex-encoded signature bytes.
    pub sig_hex: String,
    /// Signing algorithm used.
    pub algo: SignatureAlgo,
    /// Identity hash (hex) of the signer.
    pub identity_hash: String,
    /// Request timestamp (milliseconds since Unix epoch).
    pub ts_ms: u64,
    /// Hex-encoded random nonce.
    pub nonce: String,
    /// SHA-256 fingerprint of the signing public key.
    pub key_fp: String,
    /// Host label.
    pub host_label: String,
    /// Agent label.
    pub agent_label: String,
    /// Model label.
    pub model_label: String,
}

/// Methods for `SignedRequest`.
impl SignedRequest {
    /// Apply all Kleos signature headers to the given request builder.
    pub fn apply_headers(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.header("X-Kleos-Sig", &self.sig_hex)
            .header("X-Kleos-Algo", self.algo.as_str())
            .header("X-Kleos-Identity", &self.identity_hash)
            .header("X-Kleos-Ts", self.ts_ms.to_string())
            .header("X-Kleos-Nonce", &self.nonce)
            .header("X-Kleos-Key-Fp", &self.key_fp)
            .header("X-Kleos-Host", &self.host_label)
            .header("X-Kleos-Agent", &self.agent_label)
            .header("X-Kleos-Model", &self.model_label)
    }
}

/// Derive the home directory path for key-file lookup.
fn dirs_for_key_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
}

/// Create (or truncate) a file containing secret material with owner-only
/// (0600) permissions applied atomically at creation time.
#[cfg(unix)]
fn write_owner_only(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(contents)
}

/// Non-unix fallback: platform perms model differs; best-effort plain write.
#[cfg(not(unix))]
fn write_owner_only(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, contents)
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// Secret material written via write_owner_only is 0600 immediately.
    #[cfg(unix)]
    #[test]
    fn write_owner_only_creates_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        write_owner_only(&path, b"deadbeef").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "secret file must be owner-only at creation");
    }

    // -- Envelope tests --

    #[test]
    fn envelope_deterministic() {
        let e1 = CanonicalEnvelope::new("POST", "/store", "", b"hello", 1000, "aabb", "ccdd");
        let e2 = CanonicalEnvelope::new("POST", "/store", "", b"hello", 1000, "aabb", "ccdd");
        assert_eq!(e1.build(), e2.build());
    }

    #[test]
    /// Test: envelope empty body.
    fn envelope_empty_body() {
        let e = CanonicalEnvelope::new("GET", "/search", "q=test", b"", 1000, "aa", "bb");
        let built = e.build();
        let s = String::from_utf8(built).unwrap();
        assert!(s.contains(&hex::encode(Sha256::digest(b""))));
    }

    #[test]
    /// Test: envelope sorts query params.
    fn envelope_sorts_query_params() {
        let e1 = CanonicalEnvelope::new("GET", "/s", "z=1&a=2", b"", 1000, "aa", "bb");
        let e2 = CanonicalEnvelope::new("GET", "/s", "a=2&z=1", b"", 1000, "aa", "bb");
        assert_eq!(e1.build(), e2.build());
    }

    #[test]
    /// Test: envelope method uppercased.
    fn envelope_method_uppercased() {
        let e1 = CanonicalEnvelope::new("post", "/x", "", b"", 1, "a", "b");
        let e2 = CanonicalEnvelope::new("POST", "/x", "", b"", 1, "a", "b");
        assert_eq!(e1.build(), e2.build());
    }

    // -- HKDF tests --

    #[test]
    fn hkdf_deterministic() {
        let pk = b"fake-pubkey-der-bytes";
        let h1 = derive_identity_hash(pk, "wsl", "claude-code", "opus");
        let h2 = derive_identity_hash(pk, "wsl", "claude-code", "opus");
        assert_eq!(h1, h2);
    }

    #[test]
    /// Test: hkdf different labels different hash.
    fn hkdf_different_labels_different_hash() {
        let pk = b"same-key";
        let h1 = derive_identity_hash(pk, "host-a", "claude-code", "opus");
        let h2 = derive_identity_hash(pk, "host-b", "claude-code", "opus");
        let h3 = derive_identity_hash(pk, "host-a", "opencode", "opus");
        assert_ne!(h1, h2);
        assert_ne!(h1, h3);
        assert_ne!(h2, h3);
    }

    #[test]
    /// Test: hkdf empty labels valid.
    fn hkdf_empty_labels_valid() {
        let pk = b"key";
        let h = derive_identity_hash(pk, "", "", "");
        assert_eq!(h.len(), 16);
        let h2 = derive_identity_hash(pk, "a", "", "");
        assert_ne!(h, h2);
    }

    // -- RequestSigner tests --

    #[test]
    fn from_key_bytes_round_trip() {
        let secret = [42u8; 32];
        let signer = RequestSigner::from_key_bytes(secret, "test-host", "test-agent", "test-model");
        assert_eq!(signer.tier(), "soft");
        assert_eq!(signer.algo(), SignatureAlgo::Ed25519);
        assert_eq!(signer.host_label(), "test-host");
        assert_eq!(signer.agent_label(), "test-agent");
        assert_eq!(signer.model_label(), "test-model");
        assert_eq!(signer.ed25519_secret_bytes(), Some([42u8; 32]));
    }

    #[test]
    /// Test: sign request roundtrip.
    fn sign_request_roundtrip() {
        let secret = [7u8; 32];
        let signer = RequestSigner::from_key_bytes(secret, "h", "a", "m");
        let signed = signer
            .sign_request("POST", "/store", "", b"{\"x\":1}")
            .unwrap();
        assert_eq!(signed.algo, SignatureAlgo::Ed25519);
        assert!(!signed.sig_hex.is_empty());
        assert!(!signed.nonce.is_empty());
        assert!(!signed.identity_hash.is_empty());
    }

    #[test]
    /// Test: from file hex roundtrip.
    fn from_file_hex_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("identity.key");
        let secret = [0xABu8; 32];
        std::fs::write(&key_path, hex::encode(secret)).unwrap();
        let signer = RequestSigner::from_file(&key_path, "h", "a", "m").unwrap();
        assert_eq!(signer.ed25519_secret_bytes(), Some([0xABu8; 32]));
    }

    #[test]
    /// Test: from file raw bytes roundtrip.
    fn from_file_raw_bytes_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("identity.key");
        let secret = [0xCDu8; 32];
        std::fs::write(&key_path, secret).unwrap();
        let signer = RequestSigner::from_file(&key_path, "h", "a", "m").unwrap();
        assert_eq!(signer.ed25519_secret_bytes(), Some([0xCDu8; 32]));
    }

    #[test]
    /// Test: a valid PKCS8 PEM private key loads via the validated parser and
    /// recovers the secret seed it encodes. Static vector generated with
    /// `openssl genpkey -algorithm ed25519`, so the test carries no fragile
    /// dependency on the pkcs8 encoder's import paths.
    fn from_file_pkcs8_pem_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("identity.key");
        let pem = "-----BEGIN PRIVATE KEY-----\n\
                   MC4CAQAwBQYDK2VwBCIEIOPCl74iq4xOqq3sRD1BBudy7TXys619cwpAVmFec8ZL\n\
                   -----END PRIVATE KEY-----\n";
        std::fs::write(&key_path, pem).unwrap();
        let signer = RequestSigner::from_file(&key_path, "h", "a", "m").unwrap();
        let expected =
            hex::decode("e3c297be22ab8c4eaaadec443d4106e772ed35f2b3ad7d730a4056615e73c64b")
                .unwrap();
        assert_eq!(signer.ed25519_secret_bytes().unwrap().to_vec(), expected);
    }

    #[test]
    /// Test: a malformed PKCS8 PEM is rejected, not silently mis-parsed into a
    /// wrong key (the hardcoded-offset bug this replaced).
    fn from_file_malformed_pkcs8_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("identity.key");
        std::fs::write(
            &key_path,
            "-----BEGIN PRIVATE KEY-----\nbm90LXZhbGlkLXBrY3M4\n-----END PRIVATE KEY-----\n",
        )
        .unwrap();
        assert!(RequestSigner::from_file(&key_path, "h", "a", "m").is_err());
    }

    #[test]
    /// Test: identity hash is stable.
    fn identity_hash_is_stable() {
        let secret = [1u8; 32];
        let s1 = RequestSigner::from_key_bytes(secret, "host", "agent", "model");
        let s2 = RequestSigner::from_key_bytes(secret, "host", "agent", "model");
        assert_eq!(s1.identity_hash(), s2.identity_hash());
    }

    #[test]
    /// Test: identity hash differs by label.
    fn identity_hash_differs_by_label() {
        let secret = [1u8; 32];
        let s1 = RequestSigner::from_key_bytes(secret, "host-a", "agent", "model");
        let s2 = RequestSigner::from_key_bytes(secret, "host-b", "agent", "model");
        assert_ne!(s1.identity_hash(), s2.identity_hash());
    }
}
