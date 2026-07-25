//! Standalone Henosis audit witness process.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    path::Path,
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use ed25519_dalek::{SigningKey, VerifyingKey};
use henosis_witness::{router, TrustedOrigin, WitnessStore};
use serde::{
    de::{Error as _, MapAccess, Visitor},
    Deserialize, Deserializer,
};
use tracing_subscriber::EnvFilter;
use zeroize::Zeroizing;

/// Maximum accepted size of the encoded witness signing key file.
const MAX_SIGNING_KEY_FILE_BYTES: u64 = 4 * 1024;
/// Maximum accepted size of the origin trust configuration file.
const MAX_ORIGIN_CONFIG_FILE_BYTES: u64 = 1024 * 1024;

/// Strict JSON representation of one tenant-bound origin trust entry.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OriginTrustFileEntry {
    /// Base64-encoded Ed25519 verification key.
    public_key: String,
    /// Exact tenant identifiers this key may authenticate.
    tenant_ids: Vec<String>,
}

/// Duplicate-rejecting top-level origin trust map.
struct OriginTrustFile(BTreeMap<String, OriginTrustFileEntry>);

/// Deserializes origin entries while rejecting repeated key identifiers.
impl<'de> Deserialize<'de> for OriginTrustFile {
    /// Deserializes one strict origin trust object.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        /// Visitor that preserves unique key identifiers only.
        struct OriginTrustFileVisitor;

        /// Rejects duplicate origin key identifiers during map traversal.
        impl<'de> Visitor<'de> for OriginTrustFileVisitor {
            /// Strict top-level trust-map value produced by this visitor.
            type Value = OriginTrustFile;

            /// Describes the required top-level JSON shape.
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an object mapping unique origin key IDs to trust entries")
            }

            /// Reads entries one at a time so duplicate keys cannot be overwritten.
            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut entries = BTreeMap::new();
                while let Some((key_id, entry)) =
                    map.next_entry::<String, OriginTrustFileEntry>()?
                {
                    if entries.insert(key_id, entry).is_some() {
                        return Err(M::Error::custom("duplicate origin key identifier"));
                    }
                }
                Ok(OriginTrustFile(entries))
            }
        }

        deserializer.deserialize_map(OriginTrustFileVisitor)
    }
}

/// Process entry point for the separately deployed witness.
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let database_path = required_env("HENOSIS_WITNESS_DATABASE")?;
    let signing_key_path = required_env("HENOSIS_WITNESS_SIGNING_KEY_FILE")?;
    let witness_key_id = required_env("HENOSIS_WITNESS_KEY_ID")?;
    let origins_path = required_env("HENOSIS_WITNESS_ORIGIN_KEYS_FILE")?;
    let bind: SocketAddr = std::env::var("HENOSIS_WITNESS_BIND")
        .unwrap_or_else(|_| "127.0.0.1:9877".into())
        .parse()?;

    let signing_key = load_signing_key(Path::new(&signing_key_path))?;
    let trusted_origins = load_origin_keys(Path::new(&origins_path))?;
    let store = WitnessStore::open(database_path, trusted_origins, witness_key_id, signing_key)?;

    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "henosis witness listening");
    axum::serve(listener, router(store)).await?;
    Ok(())
}

/// Reads a required environment variable without printing its value.
fn required_env(name: &str) -> Result<String, Box<dyn std::error::Error>> {
    std::env::var(name).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::NotFound, format!("{name} is required")).into()
    })
}

/// Loads a base64-encoded 32-byte Ed25519 signing key from a protected file.
fn load_signing_key(path: &Path) -> Result<SigningKey, Box<dyn std::error::Error>> {
    let encoded = Zeroizing::new(read_security_file(
        path,
        MAX_SIGNING_KEY_FILE_BYTES,
        "witness signing key",
    )?);
    let encoded = std::str::from_utf8(&encoded)?;
    let bytes = Zeroizing::new(BASE64.decode(encoded.trim())?);
    let key = Zeroizing::new(
        <[u8; 32]>::try_from(bytes.as_slice())
            .map_err(|_| invalid_data("witness signing key must contain 32 bytes"))?,
    );
    Ok(SigningKey::from_bytes(&key))
}

/// Loads trusted origin public keys from a JSON object keyed by origin identifier.
fn load_origin_keys(
    path: &Path,
) -> Result<BTreeMap<String, TrustedOrigin>, Box<dyn std::error::Error>> {
    let encoded = read_security_file(
        path,
        MAX_ORIGIN_CONFIG_FILE_BYTES,
        "witness origin trust file",
    )?;
    parse_origin_keys(&encoded)
}

/// Parses only the tenant-bound origin trust schema and rejects ambiguous entries.
fn parse_origin_keys(
    encoded: &[u8],
) -> Result<BTreeMap<String, TrustedOrigin>, Box<dyn std::error::Error>> {
    let OriginTrustFile(entries) = serde_json::from_slice(encoded)?;
    if entries.is_empty() {
        return Err(invalid_data("origin trust configuration must not be empty").into());
    }
    entries
        .into_iter()
        .map(|(key_id, entry)| {
            if key_id.is_empty() || key_id.trim() != key_id {
                return Err(invalid_data("origin key identifier must not be empty").into());
            }
            let configured_tenant_count = entry.tenant_ids.len();
            let tenant_ids = entry.tenant_ids.into_iter().collect::<BTreeSet<_>>();
            if tenant_ids.is_empty()
                || tenant_ids.len() != configured_tenant_count
                || tenant_ids.iter().any(|tenant_id| {
                    tenant_id.is_empty() || tenant_id == "*" || tenant_id.trim() != tenant_id
                })
            {
                return Err(invalid_data(
                    "origin tenant allowlist must contain unique non-empty identifiers",
                )
                .into());
            }
            let bytes = BASE64.decode(entry.public_key)?;
            let key: [u8; 32] = bytes
                .try_into()
                .map_err(|_| invalid_data("origin public key must contain 32 bytes"))?;
            let verifying_key = VerifyingKey::from_bytes(&key)?;
            Ok((key_id, TrustedOrigin::new(verifying_key, tenant_ids)?))
        })
        .collect()
}

/// Constructs a stable invalid-data error for fail-closed configuration parsing.
fn invalid_data(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

/// Opens, validates, and reads one security file through the same no-follow descriptor.
#[cfg(unix)]
fn read_security_file(
    path: &Path,
    max_bytes: u64,
    label: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    use std::fs::OpenOptions;
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{label} must be a regular file"),
        )
        .into());
    }
    // SAFETY: `geteuid` has no preconditions and does not dereference memory.
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{label} must be owned by the witness service user"),
        )
        .into());
    }
    let mode = metadata.permissions().mode();
    if mode & 0o7177 != 0 || mode & 0o400 == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{label} has unsafe permissions"),
        )
        .into());
    }
    if metadata.len() > max_bytes {
        return Err(invalid_data("security configuration file is too large").into());
    }
    let mut contents = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(max_bytes + 1)
        .read_to_end(&mut contents)?;
    if contents.len() as u64 > max_bytes {
        return Err(invalid_data("security configuration file is too large").into());
    }
    Ok(contents)
}

/// Refuses security-file loading where descriptor protections are not implemented.
#[cfg(not(unix))]
fn read_security_file(
    _path: &Path,
    _max_bytes: u64,
    _label: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "henosis-witness requires Unix file ownership and mode enforcement",
    )
    .into())
}

#[cfg(test)]
/// Exercises strict trust parsing and descriptor-based security-file loading.
mod tests {
    use super::*;

    /// Encodes a deterministic valid origin public key for configuration tests.
    fn encoded_origin_key() -> String {
        BASE64.encode(
            SigningKey::from_bytes(&[3_u8; 32])
                .verifying_key()
                .as_bytes(),
        )
    }

    /// Accepts the new schema only when each key names an explicit tenant allowlist.
    #[test]
    fn parses_tenant_bound_origin_config() {
        let encoded = serde_json::json!({
            "origin-a": {
                "public_key": encoded_origin_key(),
                "tenant_ids": ["tenant-a", "tenant-b"]
            }
        });

        assert_eq!(
            parse_origin_keys(&serde_json::to_vec(&encoded).unwrap())
                .unwrap()
                .len(),
            1
        );
    }

    /// Rejects the legacy scalar-key schema instead of granting implicit tenant authority.
    #[test]
    fn rejects_legacy_origin_config() {
        let encoded = serde_json::json!({"origin-a": encoded_origin_key()});

        assert!(parse_origin_keys(&serde_json::to_vec(&encoded).unwrap()).is_err());
    }

    /// Rejects trust entries without an explicit tenant.
    #[test]
    fn rejects_empty_tenant_allowlist() {
        let encoded = serde_json::json!({
            "origin-a": {
                "public_key": encoded_origin_key(),
                "tenant_ids": []
            }
        });

        assert!(parse_origin_keys(&serde_json::to_vec(&encoded).unwrap()).is_err());
    }

    /// Rejects wildcard tenant syntax so every authorization remains an exact match.
    #[test]
    fn rejects_wildcard_tenant_allowlist() {
        let encoded = serde_json::json!({
            "origin-a": {
                "public_key": encoded_origin_key(),
                "tenant_ids": ["*"]
            }
        });

        assert!(parse_origin_keys(&serde_json::to_vec(&encoded).unwrap()).is_err());
    }

    /// Rejects duplicate exact tenants so authorization input stays canonical.
    #[test]
    fn rejects_duplicate_tenant_identifiers() {
        let encoded = serde_json::json!({
            "origin-a": {
                "public_key": encoded_origin_key(),
                "tenant_ids": ["tenant-a", "tenant-a"]
            }
        });

        assert!(parse_origin_keys(&serde_json::to_vec(&encoded).unwrap()).is_err());
    }

    /// Rejects unknown fields so misspelled authorization settings cannot be ignored.
    #[test]
    fn rejects_unknown_origin_config_fields() {
        let encoded = serde_json::json!({
            "origin-a": {
                "public_key": encoded_origin_key(),
                "tenant_ids": ["tenant-a"],
                "tenants": ["tenant-b"]
            }
        });

        assert!(parse_origin_keys(&serde_json::to_vec(&encoded).unwrap()).is_err());
    }

    /// Rejects duplicate origin key identifiers instead of silently replacing trust policy.
    #[test]
    fn rejects_duplicate_origin_key_identifiers() {
        let key = encoded_origin_key();
        let encoded = format!(
            r#"{{
                "origin-a": {{"public_key":"{key}","tenant_ids":["tenant-a"]}},
                "origin-a": {{"public_key":"{key}","tenant_ids":["tenant-b"]}}
            }}"#
        );

        assert!(parse_origin_keys(encoded.as_bytes()).is_err());
    }

    /// Rejects a final-component symlink before any key bytes are consumed.
    #[cfg(unix)]
    #[test]
    fn nofollow_rejects_security_file_symlink() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        use std::time::{SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let target = std::env::temp_dir().join(format!("henosis-witness-target-{unique}"));
        let link = std::env::temp_dir().join(format!("henosis-witness-link-{unique}"));
        std::fs::write(&target, encoded_origin_key()).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&target, &link).unwrap();

        let result = read_security_file(&link, 4096, "test security file");

        std::fs::remove_file(&link).unwrap();
        std::fs::remove_file(&target).unwrap();
        assert!(result.is_err());
    }

    /// Rejects any group, world, execute, or special permission on security files.
    #[cfg(unix)]
    #[test]
    fn restrictive_mode_rejects_group_readable_security_file() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("henosis-witness-mode-{unique}"));
        std::fs::write(&path, encoded_origin_key()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();

        let result = read_security_file(&path, 4096, "test security file");

        std::fs::remove_file(&path).unwrap();
        assert!(result.is_err());
    }
}
