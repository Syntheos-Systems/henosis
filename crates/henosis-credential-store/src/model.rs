//! Secret payloads, capability policies, and credential resolution modes.
//!
//! [`SecretData`] is encrypted with AES-256-GCM before it reaches storage. Plaintext never appears
//! in a stored row and never crosses the gate-reachable agent boundary.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A use-without-holding resolve mode. The agent never receives the secret; only the operation's
/// result (a signature, a boolean, derived key material, or a command's scrubbed output) crosses
/// the boundary. Serializes to the lowercase mode token stored in policy `allowed_modes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveMode {
    /// Produce a signature over a caller payload (HMAC-SHA256 or ed25519).
    Sign,
    /// Check a signature against a caller payload.
    Verify,
    /// Derive subordinate key material via HKDF-SHA256.
    Derive,
    /// Run an allowlisted command with the secret injected into its environment.
    Exec,
}

/// Provides stable wire representations for credential resolution modes.
impl ResolveMode {
    /// The lowercase token used in policy `allowed_modes` JSON and on the wire.
    pub fn as_token(self) -> &'static str {
        match self {
            ResolveMode::Sign => "sign",
            ResolveMode::Verify => "verify",
            ResolveMode::Derive => "derive",
            ResolveMode::Exec => "exec",
        }
    }
}

/// A capability policy: which resolve modes a principal may use against which secrets, and
/// (for exec) which commands. Matched by specificity; see [`crate::store`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// Surrogate row id (0 before insert).
    pub id: i64,
    /// Principal this policy is scoped to, or `None` for any principal in the tenant.
    pub principal_id: Option<String>,
    /// Category filter, or `None` to match every category.
    pub category: Option<String>,
    /// Secret-name filter, or `None` to match every name in the category.
    pub secret_name: Option<String>,
    /// The resolve modes this policy permits.
    pub allowed_modes: Vec<ResolveMode>,
    /// Absolute argv[0] paths exec may spawn, or `None` = exec never allowed by this policy.
    pub exec_allowlist: Option<Vec<String>>,
}

/// Evaluates the operations allowed by a capability policy.
impl Policy {
    /// True if this policy permits `mode`.
    pub fn allows(&self, mode: ResolveMode) -> bool {
        self.allowed_modes.contains(&mode)
    }
}

/// A signature algorithm for the sign/verify modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignAlgo {
    /// HMAC-SHA256 over the secret's canonical key value.
    HmacSha256,
    /// Ed25519 over a stored SSH private key (requires a [`SecretData::SshKey`] secret).
    Ed25519,
}

/// Parses signing algorithm identifiers from the credential resolution protocol.
impl SignAlgo {
    /// Parse the wire token (`"hmac-sha256"` or `"ed25519"`).
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "hmac-sha256" => Some(SignAlgo::HmacSha256),
            "ed25519" => Some(SignAlgo::Ed25519),
            _ => None,
        }
    }
}

/// The result of an exec-mode invocation. The agent receives the command's outcome and scrubbed
/// output; never the secret. `timed_out` distinguishes a killed-on-deadline child from a real
/// (possibly non-zero) exit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecOutcome {
    /// True if the child was killed because it exceeded the wall-clock deadline.
    pub timed_out: bool,
    /// The child's exit code, or `None` if it timed out or was killed by a signal.
    pub exit_code: Option<i32>,
    /// Scrubbed standard output (the secret and its base64/hex encodings replaced).
    pub stdout: Vec<u8>,
    /// Scrubbed standard error.
    pub stderr: Vec<u8>,
}

/// A stored credential's plaintext payload. Tagged by `type` on the wire so the admin API can
/// round-trip any shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SecretData {
    /// A username/password login, optionally with a URL, TOTP seed, and notes.
    Login {
        /// The account username.
        username: String,
        /// The account password (the canonical key value for sign/verify/derive).
        password: String,
        /// The associated URL, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        /// A TOTP seed, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        totp_seed: Option<String>,
        /// Free-form notes.
        #[serde(skip_serializing_if = "Option::is_none")]
        notes: Option<String>,
    },
    /// An API key, optionally with an endpoint and notes.
    ApiKey {
        /// The API key value (the canonical key value for sign/verify/derive).
        key: String,
        /// The endpoint the key authenticates against, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        endpoint: Option<String>,
        /// Free-form notes.
        #[serde(skip_serializing_if = "Option::is_none")]
        notes: Option<String>,
    },
    /// An OAuth application's client credentials.
    OAuthApp {
        /// The OAuth client id.
        client_id: String,
        /// The OAuth client secret (the canonical key value for sign/verify/derive).
        client_secret: String,
        /// The redirect URI, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        redirect_uri: Option<String>,
        /// The granted scopes, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        scopes: Option<Vec<String>>,
    },
    /// An SSH private key (the only shape that supports ed25519 sign/verify).
    SshKey {
        /// The OpenSSH-format private key PEM.
        private_key: String,
        /// The matching public key, if stored.
        #[serde(skip_serializing_if = "Option::is_none")]
        public_key: Option<String>,
        /// The key passphrase, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        passphrase: Option<String>,
    },
    /// A free-form secret note (its content is the canonical key value).
    Note {
        /// The note body.
        content: String,
    },
    /// A bag of environment variables. Has no single canonical key value, so it is
    /// rejected by the keyed resolve modes (sign/verify/derive/exec).
    Environment {
        /// The variable name/value pairs.
        variables: HashMap<String, String>,
    },
}

/// Exposes the canonical secret value for operations that require a single key.
impl SecretData {
    /// The canonical secret bytes used to key HMAC/HKDF and to inject into exec.
    ///
    /// Returns `None` for [`SecretData::Environment`], which holds many values and therefore has
    /// no single "the secret" to key with.
    pub fn key_value(&self) -> Option<&str> {
        match self {
            SecretData::Login { password, .. } => Some(password),
            SecretData::ApiKey { key, .. } => Some(key),
            SecretData::OAuthApp { client_secret, .. } => Some(client_secret),
            SecretData::SshKey { private_key, .. } => Some(private_key),
            SecretData::Note { content } => Some(content),
            SecretData::Environment { .. } => None,
        }
    }
}
