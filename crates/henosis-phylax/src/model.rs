//! Phylax domain types: the secret payload shapes and the resolve-mode enum.
//!
//! [`SecretData`] is copy-and-owned from `kleos-cred`'s secret types: the same six shapes a
//! credential can take. It is the plaintext that gets AES-256-GCM encrypted before it touches
//! disk (see [`crate::crypto`]); it never appears in a stored row in the clear and never crosses
//! the agent boundary through the gate-reachable surface.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
