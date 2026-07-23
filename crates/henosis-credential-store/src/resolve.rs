//! The use-without-holding resolve modes: sign, verify, derive, exec.
//!
//! Each loads the secret in-process, performs a cryptographic operation, and returns only the
//! result -- the secret never crosses the boundary. Every mode is fail-closed and deny-by-default:
//! it re-checks the capability policy itself ([`PhylaxStore::match_policy`]) before touching the
//! secret, so the methods are safe regardless of caller (defense in depth behind the gate).
//!
//! Ported from the Kleos `kleos-phylax` resolve_modes handlers (the freshly built + property-tested
//! 2026-06 implementation), reworked off HTTP onto the principal model.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ed25519_dalek::{Signer, Verifier};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use syntheos_contracts::{PrincipalId, TenantId};
use zeroize::Zeroizing;

use crate::error::PhylaxError;
use crate::model::{ResolveMode, SecretData, SignAlgo};
use crate::store::PhylaxStore;

/// Maximum derivable key length in bytes.
const MAX_DERIVE_LEN: usize = 64;

/// Hard wall-clock limit for an exec child, in seconds.
const EXEC_TIMEOUT_SECS: u64 = 20;

/// Cap on returned child output per stream, post-scrub.
const EXEC_OUTPUT_CAP: usize = 256 * 1024;

/// HMAC-SHA256 implementation used by credential signing and verification.
type HmacSha256 = Hmac<Sha256>;

/// Authorizes and executes non-command credential resolution modes.
impl PhylaxStore {
    /// Confirm a policy permits `mode` for (tenant, principal, category, name) without performing
    /// the operation. This is the [`crate::gate::PhylaxGate`]'s decision point: `Ok(())` means
    /// allowed, [`PhylaxError::PermissionDenied`] means denied, and a [`PhylaxError::Backend`]
    /// means the authority could not decide (the gate turns that into a fail-closed `GateError`).
    pub fn authorize_mode(
        &self,
        tenant: &TenantId,
        principal: &PrincipalId,
        category: &str,
        name: &str,
        mode: ResolveMode,
    ) -> Result<(), PhylaxError> {
        self.authorize(tenant, principal, category, name, mode)
            .map(|_| ())
    }

    /// Confirm a policy permits `mode` for (tenant, principal, category, name), returning the
    /// matched policy. Fail-closed: no matching policy, or a policy that does not name the mode,
    /// is a [`PhylaxError::PermissionDenied`].
    fn authorize(
        &self,
        tenant: &TenantId,
        principal: &PrincipalId,
        category: &str,
        name: &str,
        mode: ResolveMode,
    ) -> Result<crate::model::Policy, PhylaxError> {
        let policy = self
            .match_policy(tenant, principal, category, name)?
            .ok_or_else(|| {
                PhylaxError::PermissionDenied(format!(
                    "no policy permits '{}' on {category}/{name}",
                    mode.as_token()
                ))
            })?;
        if !policy.allows(mode) {
            return Err(PhylaxError::PermissionDenied(format!(
                "policy does not allow '{}' on {category}/{name}",
                mode.as_token()
            )));
        }
        Ok(policy)
    }

    /// The canonical secret key bytes, or a [`PhylaxError::InvalidInput`] for a secret type with
    /// no single key value (Environment). The bytes are zeroized on drop.
    fn key_bytes(
        &self,
        tenant: &TenantId,
        category: &str,
        name: &str,
    ) -> Result<Zeroizing<Vec<u8>>, PhylaxError> {
        let data = self.load_secret(tenant, category, name)?;
        match data.key_value() {
            Some(v) => Ok(Zeroizing::new(v.as_bytes().to_vec())),
            None => Err(PhylaxError::InvalidInput(
                "secret type has no single key value".into(),
            )),
        }
    }

    /// Sign `payload` with the secret under `algo`. Returns the raw signature bytes; the key
    /// never leaves the process.
    pub fn resolve_sign(
        &self,
        tenant: &TenantId,
        principal: &PrincipalId,
        category: &str,
        name: &str,
        payload: &[u8],
        algo: SignAlgo,
    ) -> Result<Vec<u8>, PhylaxError> {
        self.authorize(tenant, principal, category, name, ResolveMode::Sign)?;
        match algo {
            SignAlgo::HmacSha256 => {
                let key = self.key_bytes(tenant, category, name)?;
                let mut mac =
                    HmacSha256::new_from_slice(&key).expect("HMAC accepts any key length");
                mac.update(payload);
                Ok(mac.finalize().into_bytes().to_vec())
            }
            SignAlgo::Ed25519 => {
                let signing_key = self.ed25519_key(tenant, category, name)?;
                Ok(signing_key.sign(payload).to_bytes().to_vec())
            }
        }
    }

    /// Verify `signature` over `payload` against the secret. Returns only the boolean verdict.
    // The verify operation is genuinely (tenant, principal, category, name, payload, signature,
    // algo); a struct would add ceremony without removing the coupling to those inputs.
    #[allow(clippy::too_many_arguments)]
    pub fn resolve_verify(
        &self,
        tenant: &TenantId,
        principal: &PrincipalId,
        category: &str,
        name: &str,
        payload: &[u8],
        signature: &[u8],
        algo: SignAlgo,
    ) -> Result<bool, PhylaxError> {
        self.authorize(tenant, principal, category, name, ResolveMode::Verify)?;
        match algo {
            SignAlgo::HmacSha256 => {
                let key = self.key_bytes(tenant, category, name)?;
                let mut mac =
                    HmacSha256::new_from_slice(&key).expect("HMAC accepts any key length");
                mac.update(payload);
                let expected = mac.finalize().into_bytes();
                // Constant-time; a wrong-length signature is simply invalid.
                Ok(expected.len() == signature.len()
                    && expected.as_slice().ct_eq(signature).unwrap_u8() == 1)
            }
            SignAlgo::Ed25519 => {
                let verifying_key = self.ed25519_key(tenant, category, name)?.verifying_key();
                let Ok(sig) = ed25519_dalek::Signature::from_slice(signature) else {
                    return Ok(false);
                };
                Ok(verifying_key.verify(payload, &sig).is_ok())
            }
        }
    }

    /// Derive `length` bytes of subordinate key material from the secret via HKDF-SHA256, domain-
    /// separated by `purpose`. The root secret is unrecoverable from any derived output.
    pub fn resolve_derive(
        &self,
        tenant: &TenantId,
        principal: &PrincipalId,
        category: &str,
        name: &str,
        purpose: &str,
        length: usize,
    ) -> Result<Vec<u8>, PhylaxError> {
        if purpose.is_empty() {
            return Err(PhylaxError::InvalidInput(
                "purpose must be non-empty".into(),
            ));
        }
        if length == 0 || length > MAX_DERIVE_LEN {
            return Err(PhylaxError::InvalidInput(format!(
                "length must be 1..={MAX_DERIVE_LEN}"
            )));
        }
        self.authorize(tenant, principal, category, name, ResolveMode::Derive)?;
        let key = self.key_bytes(tenant, category, name)?;
        let hk = Hkdf::<Sha256>::new(None, &key);
        let mut okm = vec![0u8; length];
        hk.expand(format!("phylax-derive:{purpose}").as_bytes(), &mut okm)
            .map_err(|_| PhylaxError::InvalidInput("derive length invalid".into()))?;
        Ok(okm)
    }

    /// Parse the stored secret into an ed25519 signing key. Requires a [`SecretData::SshKey`].
    fn ed25519_key(
        &self,
        tenant: &TenantId,
        category: &str,
        name: &str,
    ) -> Result<ed25519_dalek::SigningKey, PhylaxError> {
        let data = self.load_secret(tenant, category, name)?;
        let SecretData::SshKey { private_key, .. } = data else {
            return Err(PhylaxError::InvalidInput(
                "ed25519 requires an ssh_key-type secret".into(),
            ));
        };
        let key = ssh_key::PrivateKey::from_openssh(private_key.as_bytes())
            .map_err(|_| PhylaxError::InvalidInput("stored key is not OpenSSH-format".into()))?;
        let pair = key
            .key_data()
            .ed25519()
            .ok_or_else(|| PhylaxError::InvalidInput("stored key is not ed25519".into()))?;
        Ok(ed25519_dalek::SigningKey::from_bytes(
            &pair.private.to_bytes(),
        ))
    }
}

/// Replace every occurrence of `needle` in `haystack` with `replacement`.
pub(crate) fn replace_all_bytes(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return haystack.to_vec();
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if i + needle.len() <= haystack.len() && &haystack[i..i + needle.len()] == needle {
            out.extend_from_slice(replacement);
            i += needle.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

/// Scrub a secret from child output: the raw bytes plus their base64 and hex (both cases)
/// encodings. A hostile command can always re-encode the secret in a form this cannot catch,
/// which is why exec is allowlist-gated; the scrub closes the accidental-leak paths (an echoed
/// environment, a verbose log line).
pub(crate) fn scrub_secret(output: &[u8], secret: &[u8]) -> Vec<u8> {
    let encodings = [
        secret.to_vec(),
        B64.encode(secret).into_bytes(),
        hex::encode(secret).into_bytes(),
        hex::encode_upper(secret).into_bytes(),
    ];
    let mut out = output.to_vec();
    for needle in &encodings {
        out = replace_all_bytes(&out, needle, b"[redacted]");
    }
    out
}

/// POSIX-shaped environment variable names only.
fn valid_env_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Executes allowlisted commands without returning the underlying credential.
impl PhylaxStore {
    /// Run an allowlisted command with the secret injected as `env_var`. The agent receives the
    /// command's scrubbed output and exit code, never the secret.
    ///
    /// `argv[0]` must be an absolute path on the matched policy's exec allowlist; a policy with no
    /// allowlist forbids exec entirely. No shell, cleared environment, stdin null, hard timeout.
    pub async fn resolve_exec(
        &self,
        tenant: &TenantId,
        principal: &PrincipalId,
        category: &str,
        name: &str,
        argv: &[String],
        env_var: &str,
    ) -> Result<crate::model::ExecOutcome, PhylaxError> {
        let Some(argv0) = argv.first() else {
            return Err(PhylaxError::InvalidInput("argv must be non-empty".into()));
        };
        if !argv0.starts_with('/') {
            return Err(PhylaxError::InvalidInput(
                "argv[0] must be an absolute path".into(),
            ));
        }
        if !valid_env_var_name(env_var) {
            return Err(PhylaxError::InvalidInput("invalid env_var name".into()));
        }

        let policy = self.authorize(tenant, principal, category, name, ResolveMode::Exec)?;
        let allowlisted = policy
            .exec_allowlist
            .as_ref()
            .is_some_and(|list| list.iter().any(|p| p == argv0));
        if !allowlisted {
            return Err(PhylaxError::PermissionDenied(
                "argv[0] is not on the policy's exec allowlist".into(),
            ));
        }

        let secret = self.key_bytes(tenant, category, name)?;
        let secret_os = {
            use std::os::unix::ffi::OsStrExt;
            std::ffi::OsStr::from_bytes(&secret).to_owned()
        };

        let child = tokio::process::Command::new(argv0)
            .args(&argv[1..])
            .env_clear()
            .env(env_var, &secret_os)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                tracing::error!(error = %e, "exec spawn failed");
                PhylaxError::InvalidInput("command could not be started".into())
            })?;

        let output = match tokio::time::timeout(
            std::time::Duration::from_secs(EXEC_TIMEOUT_SECS),
            child.wait_with_output(),
        )
        .await
        {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                tracing::error!(error = %e, "exec wait failed");
                return Err(PhylaxError::Backend("command execution failed".into()));
            }
            Err(_) => {
                // Deadline exceeded: the dropped future kills the child (kill_on_drop).
                return Ok(crate::model::ExecOutcome {
                    timed_out: true,
                    exit_code: None,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                });
            }
        };

        let mut stdout = scrub_secret(&output.stdout, &secret);
        let mut stderr = scrub_secret(&output.stderr, &secret);
        stdout.truncate(EXEC_OUTPUT_CAP);
        stderr.truncate(EXEC_OUTPUT_CAP);

        Ok(crate::model::ExecOutcome {
            timed_out: false,
            exit_code: output.status.code(),
            stdout,
            stderr,
        })
    }
}

#[cfg(test)]
/// Exercises complete credential resolution flows against an in-memory store.
mod functional_tests {
    use super::*;
    use crate::model::SecretData;
    use std::sync::Arc;
    use syntheos_axon::AxonBus;

    /// In-memory store with a random key.
    fn store() -> PhylaxStore {
        PhylaxStore::open_in_memory(Arc::new(AxonBus::new()), *crate::crypto::generate_key())
            .expect("store")
    }

    /// Store a Note secret and a policy permitting `modes` (and `exec_allowlist`). Returns
    /// (tenant, principal).
    fn fixture(
        s: &PhylaxStore,
        modes: &[ResolveMode],
        exec_allowlist: Option<&[String]>,
    ) -> (TenantId, PrincipalId) {
        let tenant = TenantId::new();
        let principal = PrincipalId::new();
        s.store_secret(
            &tenant,
            &principal,
            "prod",
            "db",
            &SecretData::Note {
                content: "super-secret".into(),
            },
        )
        .expect("store");
        s.create_policy(
            &tenant,
            Some(&principal),
            Some("prod"),
            None,
            modes,
            exec_allowlist,
        )
        .expect("policy");
        (tenant, principal)
    }

    /// HMAC sign then verify round-trips; a tampered payload does not verify.
    #[test]
    fn hmac_sign_verify_round_trip() {
        let s = store();
        let (t, p) = fixture(&s, &[ResolveMode::Sign, ResolveMode::Verify], None);
        let sig = s
            .resolve_sign(&t, &p, "prod", "db", b"attest", SignAlgo::HmacSha256)
            .expect("sign");
        assert!(s
            .resolve_verify(&t, &p, "prod", "db", b"attest", &sig, SignAlgo::HmacSha256)
            .expect("verify"));
        assert!(!s
            .resolve_verify(&t, &p, "prod", "db", b"TAMPER", &sig, SignAlgo::HmacSha256)
            .expect("verify"));
    }

    /// Derive is deterministic per (purpose, length) and purpose-separated.
    #[test]
    fn derive_deterministic_and_separated() {
        let s = store();
        let (t, p) = fixture(&s, &[ResolveMode::Derive], None);
        let a = s
            .resolve_derive(&t, &p, "prod", "db", "session", 32)
            .expect("derive");
        let b = s
            .resolve_derive(&t, &p, "prod", "db", "session", 32)
            .expect("derive");
        let c = s
            .resolve_derive(&t, &p, "prod", "db", "other", 32)
            .expect("derive");
        assert_eq!(a.len(), 32);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    /// Derive rejects an empty purpose and an oversize length.
    #[test]
    fn derive_input_validation() {
        let s = store();
        let (t, p) = fixture(&s, &[ResolveMode::Derive], None);
        assert!(matches!(
            s.resolve_derive(&t, &p, "prod", "db", "", 32),
            Err(PhylaxError::InvalidInput(_))
        ));
        assert!(matches!(
            s.resolve_derive(&t, &p, "prod", "db", "x", 65),
            Err(PhylaxError::InvalidInput(_))
        ));
    }

    /// A mode the policy does not name is denied (deny-by-default within an existing policy).
    #[test]
    fn mode_not_in_policy_denied() {
        let s = store();
        let (t, p) = fixture(&s, &[ResolveMode::Sign], None);
        assert!(matches!(
            s.resolve_derive(&t, &p, "prod", "db", "x", 16),
            Err(PhylaxError::PermissionDenied(_))
        ));
    }

    /// With no policy at all, every mode is denied.
    #[test]
    fn no_policy_denies() {
        let s = store();
        let tenant = TenantId::new();
        let principal = PrincipalId::new();
        s.store_secret(
            &tenant,
            &principal,
            "prod",
            "db",
            &SecretData::Note {
                content: "x".into(),
            },
        )
        .expect("store");
        assert!(matches!(
            s.resolve_sign(
                &tenant,
                &principal,
                "prod",
                "db",
                b"x",
                SignAlgo::HmacSha256
            ),
            Err(PhylaxError::PermissionDenied(_))
        ));
    }

    /// A different principal is not covered by a principal-scoped policy.
    #[test]
    fn principal_scoped_policy_isolates() {
        let s = store();
        let (t, _p) = fixture(&s, &[ResolveMode::Sign], None);
        let intruder = PrincipalId::new();
        assert!(matches!(
            s.resolve_sign(&t, &intruder, "prod", "db", b"x", SignAlgo::HmacSha256),
            Err(PhylaxError::PermissionDenied(_))
        ));
    }

    /// Unknown algo token does not parse.
    #[test]
    fn unknown_algo_rejected() {
        assert!(SignAlgo::parse("md5").is_none());
        assert_eq!(SignAlgo::parse("hmac-sha256"), Some(SignAlgo::HmacSha256));
        assert_eq!(SignAlgo::parse("ed25519"), Some(SignAlgo::Ed25519));
    }

    /// exec runs an allowlisted command, injects the secret, and scrubs it from output.
    #[tokio::test]
    async fn exec_runs_and_scrubs() {
        let s = store();
        let allow = vec!["/usr/bin/env".to_string()];
        let (t, p) = fixture(&s, &[ResolveMode::Exec], Some(&allow));
        let out = s
            .resolve_exec(
                &t,
                &p,
                "prod",
                "db",
                &["/usr/bin/env".to_string()],
                "INJECTED",
            )
            .await
            .expect("exec");
        assert!(!out.timed_out);
        assert_eq!(out.exit_code, Some(0));
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("INJECTED=[redacted]"), "got: {stdout}");
        assert!(!stdout.contains("super-secret"));
    }

    /// A command off the allowlist is denied, even with exec mode allowed.
    #[tokio::test]
    async fn exec_non_allowlisted_denied() {
        let s = store();
        let allow = vec!["/usr/bin/env".to_string()];
        let (t, p) = fixture(&s, &[ResolveMode::Exec], Some(&allow));
        let r = s
            .resolve_exec(&t, &p, "prod", "db", &["/bin/cat".to_string()], "X")
            .await;
        assert!(matches!(r, Err(PhylaxError::PermissionDenied(_))));
    }

    /// An exec policy with no allowlist forbids exec entirely.
    #[tokio::test]
    async fn exec_no_allowlist_denied() {
        let s = store();
        let (t, p) = fixture(&s, &[ResolveMode::Exec], None);
        let r = s
            .resolve_exec(&t, &p, "prod", "db", &["/usr/bin/env".to_string()], "X")
            .await;
        assert!(matches!(r, Err(PhylaxError::PermissionDenied(_))));
    }

    /// Relative argv[0] and bad env var names are input errors.
    #[tokio::test]
    async fn exec_input_validation() {
        let s = store();
        let allow = vec!["/usr/bin/env".to_string()];
        let (t, p) = fixture(&s, &[ResolveMode::Exec], Some(&allow));
        assert!(matches!(
            s.resolve_exec(&t, &p, "prod", "db", &["env".to_string()], "X")
                .await,
            Err(PhylaxError::InvalidInput(_))
        ));
        assert!(matches!(
            s.resolve_exec(
                &t,
                &p,
                "prod",
                "db",
                &["/usr/bin/env".to_string()],
                "BAD;VAR"
            )
            .await,
            Err(PhylaxError::InvalidInput(_))
        ));
    }

    /// create_policy rejects a relative exec allowlist entry.
    #[test]
    fn create_policy_rejects_relative_allowlist() {
        let s = store();
        let tenant = TenantId::new();
        let r = s.create_policy(
            &tenant,
            None,
            Some("prod"),
            None,
            &[ResolveMode::Exec],
            Some(&["env".to_string()]),
        );
        assert!(matches!(r, Err(PhylaxError::InvalidInput(_))));
    }

    /// ed25519 sign/verify over a stored SSH key round-trips.
    #[test]
    fn ed25519_sign_verify_round_trip() {
        let s = store();
        let tenant = TenantId::new();
        let principal = PrincipalId::new();
        let key = ssh_key::PrivateKey::random(
            &mut ssh_key::rand_core::OsRng,
            ssh_key::Algorithm::Ed25519,
        )
        .expect("gen key");
        let pem = key.to_openssh(ssh_key::LineEnding::LF).expect("pem");
        s.store_secret(
            &tenant,
            &principal,
            "prod",
            "signer",
            &SecretData::SshKey {
                private_key: pem.to_string(),
                public_key: None,
                passphrase: None,
            },
        )
        .expect("store");
        s.create_policy(
            &tenant,
            Some(&principal),
            Some("prod"),
            None,
            &[ResolveMode::Sign, ResolveMode::Verify],
            None,
        )
        .expect("policy");

        let sig = s
            .resolve_sign(
                &tenant,
                &principal,
                "prod",
                "signer",
                b"manifest",
                SignAlgo::Ed25519,
            )
            .expect("sign");
        assert!(s
            .resolve_verify(
                &tenant,
                &principal,
                "prod",
                "signer",
                b"manifest",
                &sig,
                SignAlgo::Ed25519
            )
            .expect("verify"));
    }
}

#[cfg(test)]
/// Verifies secret scrubbing properties and input validation helpers.
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Secrets under test: 4..64 bytes, none a prefix of the redaction marker.
    fn secret_strategy() -> impl Strategy<Value = Vec<u8>> {
        proptest::collection::vec(any::<u8>(), 4..64).prop_filter(
            "secret must not be part of the marker",
            |s| {
                let n = s.len().min(10);
                !b"[redacted]".windows(n).any(|w| w == &s[..n])
            },
        )
    }

    /// True when `needle` occurs nowhere in `haystack`.
    fn absent(haystack: &[u8], needle: &[u8]) -> bool {
        needle.is_empty()
            || haystack.len() < needle.len()
            || !haystack.windows(needle.len()).any(|w| w == needle)
    }

    proptest! {
        /// replace_all_bytes leaves no occurrence of the needle behind.
        #[test]
        fn prop_replace_all_bytes_total(
            prefix in proptest::collection::vec(any::<u8>(), 0..128),
            middle in proptest::collection::vec(any::<u8>(), 0..128),
            suffix in proptest::collection::vec(any::<u8>(), 0..128),
            needle in secret_strategy(),
        ) {
            let mut haystack = prefix;
            haystack.extend_from_slice(&needle);
            haystack.extend_from_slice(&middle);
            haystack.extend_from_slice(&needle);
            haystack.extend_from_slice(&suffix);
            let out = replace_all_bytes(&haystack, &needle, b"[redacted]");
            prop_assert!(absent(&out, &needle));
        }

        /// Scrub totality: no encoding of the secret survives, wherever it was embedded.
        #[test]
        fn prop_scrub_secret_total(
            prefix in proptest::collection::vec(any::<u8>(), 0..96),
            middle in proptest::collection::vec(any::<u8>(), 0..96),
            secret in secret_strategy(),
            embed_choice in 0usize..4,
        ) {
            let encodings: [Vec<u8>; 4] = [
                secret.clone(),
                B64.encode(&secret).into_bytes(),
                hex::encode(&secret).into_bytes(),
                hex::encode_upper(&secret).into_bytes(),
            ];
            let mut output = prefix;
            output.extend_from_slice(&encodings[embed_choice]);
            output.extend_from_slice(&middle);
            output.extend_from_slice(&secret);
            output.extend_from_slice(&encodings[embed_choice]);
            let scrubbed = scrub_secret(&output, &secret);
            for enc in &encodings {
                prop_assert!(absent(&scrubbed, enc), "an encoding survived scrubbing");
            }
        }
    }
}
