//! File-backed opaque credential bindings for managed Rift agent executors.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use henosis_hermes::phylaxd_client::PhylaxdClient;
use henosis_rift_bridge::materialize::{
    materialize_error, CredentialBindingResolver, MaterializeError, MediatedCommandOutput,
    PhylaxCommandRunner, ResolvedCredentialBinding,
};
use serde::Deserialize;
use uuid::Uuid;

/// Environment variable pointing to the deployment-owned binding metadata file.
pub const CREDENTIAL_BINDINGS_FILE_ENV: &str = "HENOSIS_AGENT_CREDENTIAL_BINDINGS_FILE";

/// Maximum accepted binding metadata file size.
const MAX_BINDINGS_FILE_BYTES: u64 = 1024 * 1024;

/// Secret-free JSON document containing opaque agent credential bindings.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CredentialBindingFile {
    /// Deployment-owned binding records.
    bindings: Vec<CredentialBindingRecord>,
}

/// One secret-free mapping from an opaque Rift ID to a Phylax credential slot.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CredentialBindingRecord {
    /// Opaque identifier persisted in Rift desired state.
    id: Uuid,
    /// Rift human who owns this binding.
    owner_user_id: Uuid,
    /// Phylax credential category.
    phylax_category: String,
    /// Phylax credential slot name.
    phylax_slot: String,
    /// Environment variable injected only inside the broker child.
    env_var: String,
    /// Harness IDs this binding may authenticate.
    allowed_harness_ids: Vec<String>,
}

/// File-backed resolver sharing one authenticated broker command runner.
pub struct FileCredentialBindingResolver {
    /// Binding records indexed by their opaque public identifiers.
    bindings: HashMap<Uuid, CredentialBindingRecord>,
    /// Authenticated broker runner attached only after a record resolves.
    runner: Arc<dyn PhylaxCommandRunner>,
}

/// Loads and resolves deployment binding metadata.
impl FileCredentialBindingResolver {
    /// Load the configured binding file and construct a Phylax client from process environment.
    pub fn from_env() -> Result<Self, MaterializeError> {
        let path = std::env::var_os(CREDENTIAL_BINDINGS_FILE_ENV)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| {
                materialize_error(
                    "credential_not_ready",
                    format!("{CREDENTIAL_BINDINGS_FILE_ENV} is not configured"),
                )
            })?;
        let mut config = henosis_hermes::config::Config::from_env();
        let token = config
            .phylaxd_token
            .take()
            .map(|mut value| std::mem::take(&mut *value));
        let runner = Arc::new(PhylaxBrokerRunner {
            client: Arc::new(PhylaxdClient::new(config.phylaxd_url, token)),
        });
        Self::load(&path, runner)
    }

    /// Load a bounded JSON metadata file with an injected runner.
    pub fn load(
        path: &Path,
        runner: Arc<dyn PhylaxCommandRunner>,
    ) -> Result<Self, MaterializeError> {
        let bytes = read_binding_file(path)?;
        let file: CredentialBindingFile = serde_json::from_slice(&bytes).map_err(|_| {
            materialize_error(
                "credential_not_ready",
                "credential binding file is not valid JSON metadata",
            )
        })?;
        let mut bindings = HashMap::with_capacity(file.bindings.len());
        for record in file.bindings {
            validate_record(&record)?;
            if bindings.insert(record.id, record).is_some() {
                return Err(materialize_error(
                    "credential_not_ready",
                    "credential binding IDs must be unique",
                ));
            }
        }
        Ok(Self { bindings, runner })
    }
}

/// Read binding metadata from one verified descriptor without following filesystem links.
fn read_binding_file(path: &Path) -> Result<Vec<u8>, MaterializeError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
        };

        options
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .share_mode(FILE_SHARE_READ);
    }
    let file = options.open(path).map_err(|_| {
        materialize_error(
            "credential_not_ready",
            "credential binding file is unavailable",
        )
    })?;
    validate_binding_descriptor(&file)?;

    let mut bytes = Vec::new();
    let mut limited = file.take(MAX_BINDINGS_FILE_BYTES + 1);
    limited.read_to_end(&mut bytes).map_err(|_| {
        materialize_error(
            "credential_not_ready",
            "credential binding file could not be read",
        )
    })?;
    if bytes.len() as u64 > MAX_BINDINGS_FILE_BYTES {
        return Err(materialize_error(
            "credential_not_ready",
            "credential binding file exceeds the size limit",
        ));
    }
    Ok(bytes)
}

/// Validate the opened descriptor as a controlled local regular file.
fn validate_binding_descriptor(file: &File) -> Result<(), MaterializeError> {
    let metadata = file.metadata().map_err(|_| {
        materialize_error(
            "credential_not_ready",
            "credential binding file metadata is unavailable",
        )
    })?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_BINDINGS_FILE_BYTES {
        return Err(materialize_error(
            "credential_not_ready",
            "credential binding file is not a bounded regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if metadata.mode() & 0o022 != 0 {
            return Err(materialize_error(
                "credential_not_ready",
                "credential binding file must not be group- or world-writable",
            ));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileType, FILE_ATTRIBUTE_REPARSE_POINT, FILE_TYPE_DISK,
        };

        // SAFETY: the descriptor remains open and owned by `file` for this call.
        let file_type = unsafe { GetFileType(file.as_raw_handle()) };
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || file_type != FILE_TYPE_DISK
        {
            return Err(materialize_error(
                "credential_not_ready",
                "credential binding file must be a non-reparse local disk file",
            ));
        }
    }
    Ok(())
}

/// Resolve opaque records without returning a credential value.
#[async_trait]
impl CredentialBindingResolver for FileCredentialBindingResolver {
    /// Attach the shared authenticated runner to safe metadata after lookup.
    async fn resolve_binding(
        &self,
        binding_id: Uuid,
    ) -> Result<Option<ResolvedCredentialBinding>, MaterializeError> {
        Ok(self
            .bindings
            .get(&binding_id)
            .map(|record| ResolvedCredentialBinding {
                binding_id: record.id,
                owner_user_id: record.owner_user_id,
                category: record.phylax_category.clone(),
                slot: record.phylax_slot.clone(),
                env_var: record.env_var.clone(),
                allowed_harness_ids: record.allowed_harness_ids.clone(),
                runner: self.runner.clone(),
            }))
    }
}

/// Validate broker routing identifiers before they reach the authenticated client.
fn validate_record(record: &CredentialBindingRecord) -> Result<(), MaterializeError> {
    if record.id.is_nil() || record.owner_user_id.is_nil() {
        return Err(materialize_error(
            "credential_not_ready",
            "credential binding and owner IDs must be non-nil",
        ));
    }
    for (label, value) in [
        ("Phylax category", record.phylax_category.as_str()),
        ("Phylax slot", record.phylax_slot.as_str()),
    ] {
        if value.is_empty()
            || value.len() > 128
            || !value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/')
            })
        {
            return Err(materialize_error(
                "credential_not_ready",
                format!("{label} contains an invalid identifier"),
            ));
        }
    }
    if record.env_var.is_empty()
        || record.env_var.len() > 128
        || !record.env_var.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit())
        })
    {
        return Err(materialize_error(
            "credential_not_ready",
            "credential environment variable is invalid",
        ));
    }
    if record.allowed_harness_ids.is_empty()
        || record
            .allowed_harness_ids
            .iter()
            .any(|harness| !matches!(harness.as_str(), "claude-code" | "codex"))
    {
        return Err(materialize_error(
            "credential_not_ready",
            "credential binding contains an unsupported harness allowlist",
        ));
    }
    Ok(())
}

/// Authenticated Phylax runner that decodes only broker-scrubbed process output.
struct PhylaxBrokerRunner {
    /// Shared credential broker client configured by the Henosis process.
    client: Arc<PhylaxdClient>,
}

/// Delegates commands to the existing authenticated Phylax execution endpoint.
#[async_trait]
impl PhylaxCommandRunner for PhylaxBrokerRunner {
    /// Send only routing metadata and argv, then decode scrubbed output fields.
    async fn run(
        &self,
        category: &str,
        slot: &str,
        env_var: &str,
        argv: &[String],
    ) -> Result<MediatedCommandOutput, MaterializeError> {
        let result = self
            .client
            .exec(category, slot, argv, env_var)
            .await
            .map_err(|_| {
                materialize_error("credential_not_ready", "credential broker execution failed")
            })?;
        let decoder = base64::engine::general_purpose::STANDARD;
        let stdout = decoder.decode(result.stdout_b64).map_err(|_| {
            materialize_error(
                "credential_not_ready",
                "credential broker returned invalid scrubbed output",
            )
        })?;
        let stderr = decoder.decode(result.stderr_b64).map_err(|_| {
            materialize_error(
                "credential_not_ready",
                "credential broker returned invalid scrubbed output",
            )
        })?;
        Ok(MediatedCommandOutput {
            timed_out: result.timed_out,
            exit_code: result.exit_code,
            stdout,
            stderr,
        })
    }
}

#[cfg(test)]
/// Contract tests for secret-free binding metadata loading.
mod tests {
    use std::fs;
    use std::sync::Mutex;

    use super::*;

    /// No-op broker runner used only to prove loaded metadata attaches a runner.
    #[derive(Default)]
    struct FakeRunner {
        /// Number of mediated invocations observed by the fake.
        calls: Mutex<usize>,
    }

    /// Returns deterministic scrubbed output without handling a credential value.
    #[async_trait]
    impl PhylaxCommandRunner for FakeRunner {
        /// Count the safe call and return a successful empty child result.
        async fn run(
            &self,
            _category: &str,
            _slot: &str,
            _env_var: &str,
            _argv: &[String],
        ) -> Result<MediatedCommandOutput, MaterializeError> {
            *self.calls.lock().expect("fake runner lock") += 1;
            Ok(MediatedCommandOutput {
                timed_out: false,
                exit_code: Some(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    /// Isolated binding metadata file removed after each test.
    struct BindingFixture {
        /// UUID-scoped fixture directory.
        root: PathBuf,
        /// JSON metadata file path.
        path: PathBuf,
    }

    /// Writes bounded binding documents for resolver tests.
    impl BindingFixture {
        /// Create one fixture containing the supplied JSON document.
        fn new(document: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("henosis-binding-test-{}", Uuid::new_v4()));
            fs::create_dir(&root).expect("create binding fixture directory");
            let path = root.join("bindings.json");
            fs::write(&path, document).expect("write binding fixture");
            Self { root, path }
        }
    }

    /// Removes only the UUID-scoped binding fixture directory.
    impl Drop for BindingFixture {
        /// Clean up one isolated test fixture.
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// Valid records resolve only opaque ownership and broker routing metadata.
    #[tokio::test]
    async fn binding_file_resolves_secret_free_metadata() {
        let binding_id = Uuid::new_v4();
        let owner_user_id = Uuid::new_v4();
        let fixture = BindingFixture::new(&format!(
            r#"{{"bindings":[{{"id":"{binding_id}","ownerUserId":"{owner_user_id}","phylaxCategory":"openai","phylaxSlot":"codex-seat","envVar":"OPENAI_API_KEY","allowedHarnessIds":["codex"]}}]}}"#
        ));
        let runner = Arc::new(FakeRunner::default());
        let resolver = FileCredentialBindingResolver::load(&fixture.path, runner.clone())
            .expect("load binding metadata");

        let binding = resolver
            .resolve_binding(binding_id)
            .await
            .expect("resolve binding")
            .expect("binding exists");

        assert_eq!(binding.owner_user_id, owner_user_id);
        assert_eq!(binding.category, "openai");
        assert_eq!(binding.slot, "codex-seat");
        assert_eq!(binding.env_var, "OPENAI_API_KEY");
        assert_eq!(binding.allowed_harness_ids, ["codex"]);
        assert!(resolver
            .resolve_binding(Uuid::new_v4())
            .await
            .expect("missing lookup")
            .is_none());
        assert_eq!(*runner.calls.lock().expect("fake calls"), 0);
    }

    /// Unknown fields such as a resolved secret are rejected instead of retained.
    #[test]
    fn binding_file_rejects_secret_fields() {
        let fixture = BindingFixture::new(&format!(
            r#"{{"bindings":[{{"id":"{}","ownerUserId":"{}","phylaxCategory":"openai","phylaxSlot":"codex-seat","envVar":"OPENAI_API_KEY","allowedHarnessIds":["codex"],"secret":"must-not-load"}}]}}"#,
            Uuid::new_v4(),
            Uuid::new_v4(),
        ));

        let error = match FileCredentialBindingResolver::load(
            &fixture.path,
            Arc::new(FakeRunner::default()),
        ) {
            Ok(_) => panic!("secret-bearing metadata unexpectedly loaded"),
            Err(error) => error,
        };

        assert_eq!(error.code, "credential_not_ready");
    }

    /// Symbolic links cannot redirect the resolver after deployment validation.
    #[cfg(unix)]
    #[test]
    fn binding_file_rejects_symbolic_links() {
        use std::os::unix::fs::symlink;

        let fixture = BindingFixture::new(r#"{"bindings":[]}"#);
        let link = fixture.root.join("bindings-link.json");
        symlink(&fixture.path, &link).expect("create binding symlink");

        let error =
            match FileCredentialBindingResolver::load(&link, Arc::new(FakeRunner::default())) {
                Ok(_) => panic!("symbolic-link metadata unexpectedly loaded"),
                Err(error) => error,
            };

        assert_eq!(error.code, "credential_not_ready");
    }

    /// Group- or world-writable metadata cannot control broker routing.
    #[cfg(unix)]
    #[test]
    fn binding_file_rejects_writable_by_others() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = BindingFixture::new(r#"{"bindings":[]}"#);
        fs::set_permissions(&fixture.path, fs::Permissions::from_mode(0o666))
            .expect("make binding fixture unsafe");

        let error = match FileCredentialBindingResolver::load(
            &fixture.path,
            Arc::new(FakeRunner::default()),
        ) {
            Ok(_) => panic!("writable binding metadata unexpectedly loaded"),
            Err(error) => error,
        };

        assert_eq!(error.code, "credential_not_ready");
    }
}
