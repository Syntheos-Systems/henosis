//! Safe operator command parsing and local bootstrap support for the Henosis binary.
//!
//! This module never builds a shell command. It performs only idempotent local initialization
//! and diagnostics itself; all live control-plane operations cross the typed [`ControlApi`] seam.

use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::{
    blocking::{Client as BlockingHttpClient, Response as BlockingHttpResponse},
    Method, Url,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use syntheos_contracts::{PrincipalId, TenantId};
use thiserror::Error;
use zeroize::Zeroize;

use crate::room_runtime::{parse_room_mode, RoomMode, DISABLED_MODE, REQUIRED_MODE};

/// Relative configuration file created by `henosis init --quick`.
const CONFIG_FILE: &str = "config.env";

/// Relative persistent data directory created by `henosis init --quick`.
const DATA_DIRECTORY: &str = "data";

/// Relative agent roster file created by `henosis init --quick`.
const AGENTS_FILE: &str = "agents.toml";

/// Environment variable selecting the agent harness during initialization.
///
/// Exists because `install.sh` runs under `curl | sh`, where stdin is the script
/// itself and an interactive prompt cannot read a reply.
const HARNESS_ENV: &str = "HENOSIS_HARNESS";

/// Local owner-token path key written by `henosis init --quick`.
const LOCAL_TOKEN_KEY: &str = "HENOSIS_LOCAL_TOKEN_FILE";

/// Local owner-token filename created on the first server boot.
const LOCAL_TOKEN_FILE: &str = "local-operator.token";

/// Maximum accepted live-control response body size in bytes.
const MAX_CONTROL_RESPONSE_BYTES: usize = 1024 * 1024;

/// Maximum accepted local bearer-token file size in bytes.
const MAX_LOCAL_TOKEN_BYTES: usize = 16 * 1024;

/// Maximum accepted local environment configuration size in bytes.
const MAX_LOCAL_CONFIG_BYTES: usize = 64 * 1024;

/// Fixed timeout for each live-control request.
const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Local database environment keys and filenames written by `henosis init --quick`.
const LOCAL_DATABASES: &[(&str, &str)] = &[
    ("SYNTHEOS_IDENTITY_DB", "identity.sqlite"),
    ("HENOSIS_APPROVAL_DB", "approval.sqlite"),
    ("HENOSIS_AUDIT_DB", "audit.sqlite"),
    ("SYNTHEOS_CHIASM_DB", "chiasm.sqlite"),
    ("SYNTHEOS_SOMA_DB", "soma.sqlite"),
    ("SYNTHEOS_BROCA_DB", "broca.sqlite"),
    ("SYNTHEOS_LOOM_DB", "loom.sqlite"),
    ("SYNTHEOS_THYMUS_DB", "thymus.sqlite"),
    ("SYNTHEOS_COGNITION_DB", "cognition.db"),
];

/// Environment-style keys that must be non-empty before production initialization can proceed.
const PRODUCTION_REQUIRED_KEYS: &[&str] = &[
    "SYNTHEOS_PLUTUS_DB",
    "SYNTHEOS_OPERATOR_JWT_SECRET",
    "HENOSIS_RIFT_JWT_SECRET",
    "HENOSIS_RIFT_BRIDGE_SECRET",
    "HENOSIS_RIFT_DATABASE_URL",
    "HENOSIS_RIFT_BRIDGE_CONFIG",
    "PHYLAXD_URL",
    "HERMES_PHYLAXD_TOKEN",
    "HENOSIS_WITNESS_URL",
    "HENOSIS_AUDIT_ORIGIN_KEY_FILE",
    "HENOSIS_AUDIT_ORIGIN_KEY_ID",
    "HENOSIS_WITNESS_PUBLIC_KEY_FILE",
    "HENOSIS_WITNESS_KEY_ID",
];

/// Stable human-readable usage text for `henosis --help` and `henosis help`.
pub const HELP_TEXT: &str = "Henosis operator commands:\n  henosis init --quick [--harness <name|path>]\n  henosis init --production\n  henosis doctor [--json]\n  henosis serve\n  henosis status\n  henosis update (unavailable in alpha)\n  henosis uninstall (unavailable in alpha)\n  henosis token create <label> [--token-only] | list | revoke <token-id>\n  henosis approvals list | approve <approval-id> | deny <approval-id>\n  henosis audit verify\n  henosis --help | --version";

/// Stable version text for `henosis --version` and `henosis version`.
pub const VERSION_TEXT: &str = concat!("henosis ", env!("CARGO_PKG_VERSION"));

/// A fully parsed top-level operator command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Initialize a local operator home directory.
    Init(InitMode),
    /// Initialize a local operator home with an explicitly selected external harness.
    InitWithHarness(Harness),
    /// Inspect local configuration and data directory readiness.
    Doctor(DoctorOutputFormat),
    /// Print the stable command reference without mutating local state.
    Help,
    /// Print the package version without mutating local state.
    Version,
    /// Fetch the live control plane's status.
    Status,
    /// Request an update through the live control plane.
    Update,
    /// Request a managed uninstall through the live control plane.
    Uninstall,
    /// Manage operator tokens through the live control plane.
    Token(TokenCommand),
    /// Resolve pending approvals through the live control plane.
    Approvals(ApprovalCommand),
    /// Verify the live audit trail.
    AuditVerify,
    /// Start the server using the binary's integrated runtime.
    Serve,
}

/// The explicitly selected initialization workflow.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitMode {
    /// Create immediately usable private local configuration and data paths without prompting.
    Quick,
    /// Validate an existing production authority configuration without creating files.
    Production,
}

/// The representation requested for a local diagnostic report.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DoctorOutputFormat {
    /// Render an actionable report for an operator at a terminal.
    Human,
    /// Render one JSON object for installers and automation.
    Json,
}

/// The terminal representation requested for a newly issued machine token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TokenOutputFormat {
    /// Preserve the explanatory one-time credential message for interactive operators.
    Human,
    /// Print only the credential for safe shell command substitution.
    TokenOnly,
}

/// A typed token-management operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TokenCommand {
    /// Create one token with a human-readable label.
    Create {
        /// Non-empty label attached to the created token.
        label: String,
        /// Terminal representation selected for the one-time credential.
        output: TokenOutputFormat,
    },
    /// List active token metadata without exposing token secrets.
    List,
    /// Revoke one token by its opaque identifier.
    Revoke {
        /// Non-empty opaque token identifier.
        token_id: String,
    },
}

/// A typed human-approval operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApprovalCommand {
    /// List approvals awaiting an operator decision.
    List,
    /// Approve one opaque approval identifier.
    Approve {
        /// Non-empty opaque approval identifier.
        approval_id: String,
    },
    /// Deny one opaque approval identifier.
    Deny {
        /// Non-empty opaque approval identifier.
        approval_id: String,
    },
}

/// A typed request emitted only after parsing and local validation complete.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlRequest {
    /// Retrieve the live control-plane status.
    Status,
    /// Ask the control plane to perform its supported update procedure.
    Update,
    /// Ask the control plane to perform its supported uninstall procedure.
    Uninstall,
    /// Forward a token-management request.
    Token(TokenCommand),
    /// Forward an approval-management request.
    Approvals(ApprovalCommand),
    /// Ask the live audit authority to verify its chain.
    AuditVerify,
}

/// A typed control-plane response safe for the CLI renderer to display.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlOutput {
    /// Stable operation name returned by the live control plane.
    pub operation: String,
    /// Terminal result, with a one-time token shown only for token creation.
    pub message: String,
}

/// The integration seam for authenticated live control-plane commands.
pub trait ControlApi {
    /// Executes one already-validated typed request against the live control plane.
    fn execute(&self, request: ControlRequest) -> Result<ControlOutput, CliError>;
}

/// An authenticated synchronous HTTP implementation of the live control-plane seam.
pub struct HttpControlApi {
    /// Validated hierarchical base URL with a trailing path separator.
    base_url: Url,
    /// Redirect-disabled client shared by all CLI control requests.
    client: BlockingHttpClient,
    /// Bearer credential kept only for the lifetime of this client.
    bearer_token: String,
}

/// Releases the in-memory bearer credential when the client is dropped.
impl Drop for HttpControlApi {
    /// Zeroes the bearer token before Rust releases its allocation.
    fn drop(&mut self) {
        self.bearer_token.zeroize();
    }
}

/// Constructs and executes secure synchronous control-plane requests.
impl HttpControlApi {
    /// Loads a validated API endpoint and bearer credential after local configuration is available.
    pub fn from_environment() -> Result<Self, CliError> {
        let base_url = control_base_url(
            env::var("HENOSIS_API_URL")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            env::var("SYNTHEOS_ADDR")
                .ok()
                .filter(|value| !value.trim().is_empty()),
        )?;
        let client = BlockingHttpClient::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(CONTROL_REQUEST_TIMEOUT)
            .build()
            .map_err(CliError::ControlClient)?;
        let bearer_token = match env::var("HENOSIS_API_TOKEN") {
            Ok(token) if !token.trim().is_empty() => validate_bearer_token(token)?,
            _ => {
                let path = env::var_os(LOCAL_TOKEN_KEY).ok_or(CliError::ControlConfiguration {
                    message: "set HENOSIS_API_TOKEN or HENOSIS_LOCAL_TOKEN_FILE",
                })?;
                load_local_bearer_token(Path::new(&path))?
            }
        };
        Ok(Self {
            base_url,
            client,
            bearer_token,
        })
    }

    /// Builds one URL from the validated base plus escaped path segments.
    fn url(&self, segments: &[&str]) -> Result<Url, CliError> {
        control_url(&self.base_url, segments)
    }

    /// Sends one authenticated request and rejects non-success statuses without reading their body.
    fn request(
        &self,
        method: Method,
        segments: &[&str],
        body: Option<Value>,
        operation: &'static str,
    ) -> Result<BlockingHttpResponse, CliError> {
        let request = self
            .client
            .request(method, self.url(segments)?)
            .bearer_auth(&self.bearer_token);
        let request = match body {
            Some(body) => request.json(&body),
            None => request,
        };
        let response = request
            .send()
            .map_err(|source| CliError::ControlTransport { operation, source })?;
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(CliError::ControlHttpStatus {
                operation,
                status: response.status().as_u16(),
            })
        }
    }

    /// Reads a successful response while enforcing the fixed control-plane body limit.
    fn response_bytes(
        &self,
        response: BlockingHttpResponse,
        operation: &'static str,
    ) -> Result<Vec<u8>, CliError> {
        if response
            .content_length()
            .is_some_and(|length| length > MAX_CONTROL_RESPONSE_BYTES as u64)
        {
            return Err(CliError::ControlResponseTooLarge { operation });
        }
        let mut body = Vec::new();
        response
            .take(MAX_CONTROL_RESPONSE_BYTES as u64 + 1)
            .read_to_end(&mut body)
            .map_err(|source| CliError::ControlResponseRead { operation, source })?;
        if body.len() > MAX_CONTROL_RESPONSE_BYTES {
            return Err(CliError::ControlResponseTooLarge { operation });
        }
        Ok(body)
    }

    /// Decodes a successful JSON response without including malformed response content in errors.
    fn response_json(
        &self,
        response: BlockingHttpResponse,
        operation: &'static str,
    ) -> Result<Value, CliError> {
        serde_json::from_slice(&self.response_bytes(response, operation)?)
            .map_err(|_| CliError::InvalidControlResponse { operation })
    }

    /// Decodes a successful UTF-8 response without including malformed response content in errors.
    fn response_text(
        &self,
        response: BlockingHttpResponse,
        operation: &'static str,
    ) -> Result<String, CliError> {
        String::from_utf8(self.response_bytes(response, operation)?)
            .map_err(|_| CliError::InvalidControlResponse { operation })
    }

    /// Formats a bounded JSON success response for safe terminal display.
    fn json_output(
        &self,
        response: BlockingHttpResponse,
        operation: &'static str,
    ) -> Result<ControlOutput, CliError> {
        let message = serde_json::to_string_pretty(&self.response_json(response, operation)?)
            .map_err(CliError::Serialization)?;
        Ok(ControlOutput {
            operation: operation.to_string(),
            message,
        })
    }

    /// Creates a one-time machine token and renders it in the explicitly selected format.
    fn create_token(
        &self,
        label: &str,
        output: TokenOutputFormat,
    ) -> Result<ControlOutput, CliError> {
        let operation = "henosis token create";
        let response = self.request(
            Method::POST,
            &["api", "v1", "tokens"],
            Some(json!({"label": label, "scopes": ["dispatch"], "expires_in_seconds": null})),
            operation,
        )?;
        let issued: IssuedTokenResponse =
            serde_json::from_value(self.response_json(response, operation)?)
                .map_err(|_| CliError::InvalidControlResponse { operation })?;
        let token = validate_bearer_token(issued.token)
            .map_err(|_| CliError::InvalidControlResponse { operation })?;
        Ok(ControlOutput {
            operation: operation.to_string(),
            message: render_created_token(token, output),
        })
    }
}

/// Render one issued credential without changing the established human output.
fn render_created_token(token: String, output: TokenOutputFormat) -> String {
    match output {
        TokenOutputFormat::Human => {
            format!("token created; save this one-time credential now:\n{token}")
        }
        TokenOutputFormat::TokenOnly => token,
    }
}

/// Maps typed requests to the authenticated public HTTP control-plane routes.
impl ControlApi for HttpControlApi {
    /// Executes a typed operation against its fixed API route and response contract.
    fn execute(&self, request: ControlRequest) -> Result<ControlOutput, CliError> {
        match request {
            ControlRequest::Status => {
                let operation = "henosis status";
                let response = self.request(Method::GET, &["health"], None, operation)?;
                Ok(ControlOutput {
                    operation: operation.to_string(),
                    message: self.response_text(response, operation)?,
                })
            }
            ControlRequest::Update => Err(CliError::UnsupportedControlOperation {
                operation: "henosis update",
            }),
            ControlRequest::Uninstall => Err(CliError::UnsupportedControlOperation {
                operation: "henosis uninstall",
            }),
            ControlRequest::Token(TokenCommand::Create { label, output }) => {
                self.create_token(&label, output)
            }
            ControlRequest::Token(TokenCommand::List) => {
                let operation = "henosis token list";
                self.json_output(
                    self.request(Method::GET, &["api", "v1", "tokens"], None, operation)?,
                    operation,
                )
            }
            ControlRequest::Token(TokenCommand::Revoke { token_id }) => {
                let operation = "henosis token revoke";
                self.request(
                    Method::POST,
                    &["api", "v1", "tokens", &token_id, "revoke"],
                    None,
                    operation,
                )?;
                Ok(ControlOutput {
                    operation: operation.to_string(),
                    message: "token revoked".to_string(),
                })
            }
            ControlRequest::Approvals(ApprovalCommand::List) => {
                let operation = "henosis approvals list";
                self.json_output(
                    self.request(Method::GET, &["api", "v1", "approvals"], None, operation)?,
                    operation,
                )
            }
            ControlRequest::Approvals(ApprovalCommand::Approve { approval_id }) => {
                let operation = "henosis approvals approve";
                self.json_output(
                    self.request(
                        Method::POST,
                        &["api", "v1", "approvals", &approval_id, "approve"],
                        Some(json!({})),
                        operation,
                    )?,
                    operation,
                )
            }
            ControlRequest::Approvals(ApprovalCommand::Deny { approval_id }) => {
                let operation = "henosis approvals deny";
                self.json_output(
                    self.request(
                        Method::POST,
                        &["api", "v1", "approvals", &approval_id, "deny"],
                        Some(json!({})),
                        operation,
                    )?,
                    operation,
                )
            }
            ControlRequest::AuditVerify => {
                let operation = "henosis audit verify";
                self.json_output(
                    self.request(
                        Method::GET,
                        &["api", "v1", "audit", "verify"],
                        None,
                        operation,
                    )?,
                    operation,
                )
            }
        }
    }
}

/// Parses the one intentional secret-bearing response field from token issuance.
#[derive(Deserialize)]
struct IssuedTokenResponse {
    /// Cleartext machine credential presented once to the invoking terminal.
    token: String,
}

/// Resolves the configured base URL from an explicit URL or the local listening address.
fn control_base_url(
    explicit_url: Option<String>,
    syntheos_address: Option<String>,
) -> Result<Url, CliError> {
    let value = match explicit_url {
        Some(url) => url,
        None => {
            let address = syntheos_address.ok_or(CliError::ControlConfiguration {
                message: "set HENOSIS_API_URL or SYNTHEOS_ADDR",
            })?;
            format!("http://{address}")
        }
    };
    validate_control_url(&value)
}

/// Validates an HTTP base URL before a bearer credential can be sent to it.
fn validate_control_url(value: &str) -> Result<Url, CliError> {
    let mut url = Url::parse(value).map_err(|_| CliError::ControlConfiguration {
        message: "HENOSIS_API_URL must be an absolute HTTP URL",
    })?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(CliError::ControlConfiguration {
            message: "HENOSIS_API_URL must not contain credentials",
        });
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(CliError::ControlConfiguration {
            message: "HENOSIS_API_URL must not contain a query or fragment",
        });
    }
    let loopback = url.host_str().is_some_and(is_loopback_host);
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        "http" => {
            return Err(CliError::ControlConfiguration {
                message: "remote HENOSIS_API_URL endpoints must use HTTPS",
            });
        }
        _ => {
            return Err(CliError::ControlConfiguration {
                message: "HENOSIS_API_URL must use HTTP or HTTPS",
            });
        }
    }
    if url.host_str().is_none() {
        return Err(CliError::ControlConfiguration {
            message: "HENOSIS_API_URL must contain a host",
        });
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

/// Returns whether an HTTP host is strictly local to the machine.
fn is_loopback_host(host: &str) -> bool {
    let ip_host = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || ip_host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// Appends escaped path segments to a previously validated control-plane base URL.
fn control_url(base_url: &Url, segments: &[&str]) -> Result<Url, CliError> {
    if segments
        .iter()
        .any(|segment| segment.is_empty() || matches!(*segment, "." | ".."))
    {
        return Err(CliError::ControlConfiguration {
            message: "control route contains an invalid empty or dot path segment",
        });
    }
    let mut url = base_url.clone();
    let mut path = url
        .path_segments_mut()
        .map_err(|_| CliError::ControlConfiguration {
            message: "HENOSIS_API_URL cannot be used as an HTTP base URL",
        })?;
    path.pop_if_empty();
    for segment in segments {
        path.push(segment);
    }
    drop(path);
    Ok(url)
}

/// Validates a bearer credential without reflecting any portion of it in an error.
fn validate_bearer_token(mut token: String) -> Result<String, CliError> {
    let valid = !token.is_empty() && !token.chars().any(char::is_whitespace);
    if valid {
        Ok(token)
    } else {
        token.zeroize();
        Err(CliError::ControlConfiguration {
            message: "the live-control bearer token must not be empty or contain whitespace",
        })
    }
}

/// Loads a bearer token from a stable owner-private regular file.
fn load_local_bearer_token(path: &Path) -> Result<String, CliError> {
    let file = open_private_config(path)?;
    require_owner_private_token(path, &file)?;
    let mut token = read_bounded_string(path, file, MAX_LOCAL_TOKEN_BYTES)?;
    let trimmed = token.trim().to_string();
    token.zeroize();
    validate_bearer_token(trimmed)
}

/// Verifies that a local bearer-token file belongs to the current Unix user.
#[cfg(unix)]
fn require_owner_private_token(path: &Path, file: &File) -> Result<(), CliError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata().map_err(|source| CliError::Filesystem {
        path: path.to_path_buf(),
        source,
    })?;
    // SAFETY: `geteuid` has no preconditions and reads no Rust-managed memory.
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() == effective_uid {
        Ok(())
    } else {
        Err(CliError::InsecureLocalTokenOwnership {
            path: path.to_path_buf(),
        })
    }
}

/// Accepts platform-native ownership controls where Unix user identifiers are unavailable.
#[cfg(not(unix))]
fn require_owner_private_token(_path: &Path, _file: &File) -> Result<(), CliError> {
    Ok(())
}

/// Local paths owned by an operator installation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CliPaths {
    /// Root directory containing all local CLI-owned state.
    pub home: PathBuf,
    /// Environment-style configuration file created by quick initialization.
    pub config: PathBuf,
    /// Directory reserved for persistent local database files.
    pub data: PathBuf,
}

/// Resolves and validates CLI-owned local paths.
impl CliPaths {
    /// Builds paths rooted at an explicit directory, primarily for embedding and tests.
    pub fn from_home(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        Self {
            config: home.join(CONFIG_FILE),
            data: home.join(DATA_DIRECTORY),
            home,
        }
    }

    /// Uses `HENOSIS_HOME`, then the current user's home directory, and finally an actionable error.
    pub fn from_environment() -> Result<Self, CliError> {
        if let Some(home) = env::var_os("HENOSIS_HOME") {
            if !home.is_empty() {
                return Ok(Self::from_home(home));
            }
        }
        let home = env::var_os("HOME")
            .or_else(|| env::var_os("USERPROFILE"))
            .ok_or(CliError::HomeDirectoryUnavailable)?;
        Ok(Self::from_home(PathBuf::from(home).join(".henosis")))
    }
}

/// A local readiness report returned by `henosis doctor`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DoctorReport {
    /// Observed type of the local operator home path without following symlinks.
    pub home: LocalPathState,
    /// Observed type of the bootstrap configuration path without following symlinks.
    pub config: LocalPathState,
    /// Observed type of the persistent data path without following symlinks.
    pub data: LocalPathState,
    /// Required production configuration keys that are missing or blank.
    pub missing_production_keys: Vec<String>,
    /// Whether local filesystem paths and local operator identities are configured.
    pub local_ready: bool,
    /// Whether production authority configuration is complete without exposing its values.
    pub production_ready: bool,
    /// The exact next action to take when the local installation is incomplete.
    pub next_step: String,
}

/// A safe, non-following observation of one CLI-owned local path.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalPathState {
    /// No filesystem entry exists at the expected path.
    Missing,
    /// A non-symlink directory exists at the expected path.
    Directory,
    /// A non-symlink regular file exists at the expected path.
    File,
    /// A symlink or unsupported filesystem entry exists at the expected path.
    UnsafeOrUnexpected,
}

/// The successful result of idempotent local initialization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitResult {
    /// Initialization workflow that completed without prompting or generating a user password.
    pub mode: InitMode,
    /// Canonical path to the created or preserved configuration file.
    pub config: PathBuf,
    /// Canonical path to the created or preserved persistent data directory.
    pub data: PathBuf,
    /// The exact next action required before a server can safely start.
    pub next_step: String,
}

/// The result of a parsed command execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunResult {
    /// A local quick initialization completed or found an existing installation.
    Initialized(InitResult),
    /// A local diagnostic completed without contacting a server.
    Doctor {
        /// Report generated without contacting a live control plane.
        report: DoctorReport,
        /// Representation requested by the parsed command.
        format: DoctorOutputFormat,
    },
    /// The caller requested stable human-readable command help.
    Help,
    /// The caller requested the package version.
    Version,
    /// A typed live control-plane request completed.
    Control(ControlOutput),
    /// The binary entry point must continue into its integrated server runtime.
    Serve,
}

/// Renders local command results without executing a shell or disclosing authority secrets.
impl RunResult {
    /// Converts a completed local result into the representation selected by its command.
    pub fn render(&self) -> Result<String, CliError> {
        match self {
            Self::Initialized(result) => Ok(format!(
                "{}\nconfiguration: {}\ndata: {}",
                result.next_step,
                result.config.display(),
                result.data.display()
            )),
            Self::Doctor {
                report,
                format: DoctorOutputFormat::Human,
            } => Ok(format!(
                "home: {:?}\nconfig: {:?}\ndata: {:?}\nlocal ready: {}\nproduction ready: {}\nmissing production keys: {}\nnext: {}",
                report.home,
                report.config,
                report.data,
                report.local_ready,
                report.production_ready,
                report.missing_production_keys.join(", "),
                report.next_step
            )),
            Self::Doctor {
                report,
                format: DoctorOutputFormat::Json,
            } => serde_json::to_string(report).map_err(CliError::Serialization),
            Self::Help => Ok(HELP_TEXT.to_string()),
            Self::Version => Ok(VERSION_TEXT.to_string()),
            Self::Control(output) => Ok(output.message.clone()),
            Self::Serve => Ok(String::new()),
        }
    }
}

/// A parser and runner that separates local filesystem work from live control-plane authority.
pub struct CliRunner<'a> {
    /// Paths owned by this local CLI installation.
    paths: CliPaths,
    /// Optional authenticated client supplied by the binary integration layer.
    control_api: Option<&'a dyn ControlApi>,
}

/// Parses command lines and executes only explicitly authorized local work.
impl<'a> CliRunner<'a> {
    /// Creates a runner without a live control-plane client.
    pub fn local(paths: CliPaths) -> Self {
        Self {
            paths,
            control_api: None,
        }
    }

    /// Attaches an authenticated typed control-plane client to a local runner.
    pub fn with_control_api(mut self, control_api: &'a dyn ControlApi) -> Self {
        self.control_api = Some(control_api);
        self
    }

    /// Parses and runs the supplied arguments without invoking a shell.
    pub fn parse_and_run(&self, arguments: &[String]) -> Result<RunResult, CliError> {
        self.run(Command::parse(arguments)?)
    }

    /// Runs one parsed command through local handling or the typed control-plane seam.
    pub fn run(&self, command: Command) -> Result<RunResult, CliError> {
        match command {
            Command::Init(InitMode::Quick) => {
                initialize_quick(&self.paths, &Harness::Synapse).map(RunResult::Initialized)
            }
            Command::InitWithHarness(harness) => {
                initialize_quick(&self.paths, &harness).map(RunResult::Initialized)
            }
            Command::Init(InitMode::Production) => {
                initialize_production(&self.paths).map(RunResult::Initialized)
            }
            Command::Doctor(format) => Ok(RunResult::Doctor {
                report: doctor(&self.paths)?,
                format,
            }),
            Command::Help => Ok(RunResult::Help),
            Command::Version => Ok(RunResult::Version),
            Command::Serve => Ok(RunResult::Serve),
            Command::Status => self.run_control(ControlRequest::Status),
            Command::Update => self.run_control(ControlRequest::Update),
            Command::Uninstall => self.run_control(ControlRequest::Uninstall),
            Command::Token(command) => self.run_control(ControlRequest::Token(command)),
            Command::Approvals(command) => self.run_control(ControlRequest::Approvals(command)),
            Command::AuditVerify => self.run_control(ControlRequest::AuditVerify),
        }
    }

    /// Delegates a request only when binary integration supplied an authenticated typed client.
    fn run_control(&self, request: ControlRequest) -> Result<RunResult, CliError> {
        let operation = control_operation_name(&request);
        let client = self
            .control_api
            .ok_or_else(|| CliError::ControlApiUnavailable {
                operation: operation.to_string(),
            })?;
        client.execute(request).map(RunResult::Control)
    }
}

/// Parses the top-level CLI grammar without a shell or string-command construction.
impl Command {
    /// Parses a command after the binary name has been removed from the argument vector.
    pub fn parse(arguments: &[String]) -> Result<Self, CliError> {
        let Some((head, tail)) = arguments.split_first() else {
            return Ok(Self::Help);
        };
        match head.as_str() {
            "init" => parse_init(tail),
            "doctor" => parse_doctor(tail),
            "help" | "--help" | "-h" => parse_empty(tail, Self::Help),
            "version" | "--version" | "-V" => parse_empty(tail, Self::Version),
            "status" => parse_empty(tail, Self::Status),
            "update" => parse_empty(tail, Self::Update),
            "uninstall" => parse_empty(tail, Self::Uninstall),
            "token" => parse_token(tail).map(Self::Token),
            "approvals" => parse_approvals(tail).map(Self::Approvals),
            "audit" => parse_audit(tail),
            "serve" => parse_empty(tail, Self::Serve),
            value => Err(CliError::Usage {
                message: format!("unknown command `{value}`"),
            }),
        }
    }
}

/// Describes every command parsing, filesystem, and unavailable-integration failure.
#[derive(Debug, Error)]
pub enum CliError {
    /// The caller supplied an unsupported argument shape.
    #[error("invalid command: {message}")]
    Usage {
        /// Actionable explanation of the accepted command shape.
        message: String,
    },
    /// The caller omitted an explicit initialization mode.
    #[error(
        "`henosis init` requires `--quick` or `--production`; interactive initialization is not available"
    )]
    InitModeRequired,
    /// The execution environment supplied no usable user home directory.
    #[error("cannot resolve a local Henosis home; set HENOSIS_HOME to an explicit directory")]
    HomeDirectoryUnavailable,
    /// A filesystem operation for one explicit path failed.
    #[error("filesystem operation for {path} failed: {source}")]
    Filesystem {
        /// Exact path involved in the failed operation.
        path: PathBuf,
        /// Underlying operating-system error.
        #[source]
        source: io::Error,
    },
    /// An existing path would cause initialization to use an unsafe or unexpected filesystem type.
    #[error("{path} must be a non-symlink {expected}, but found {found}")]
    UnexpectedPathType {
        /// Exact path rejected before any write occurred.
        path: PathBuf,
        /// Safe filesystem type required by the operation.
        expected: &'static str,
        /// Observed filesystem type reported without following symlinks.
        found: &'static str,
    },
    /// A configuration file granted access to group or other users on Unix.
    #[error("{path} must use owner-only permissions, but found mode {mode:#o}")]
    InsecureConfigPermissions {
        /// Exact configuration path rejected before parsing.
        path: PathBuf,
        /// Observed Unix permission bits.
        mode: u32,
    },
    /// A local bearer-token file was not owned by the user running the CLI.
    #[error("{path} must be owned by the current user")]
    InsecureLocalTokenOwnership {
        /// Exact local token path rejected before its credential was used.
        path: PathBuf,
    },
    /// A sensitive local input exceeded its fixed allocation limit.
    #[error("{path} exceeds the maximum accepted size of {max_bytes} bytes")]
    LocalFileTooLarge {
        /// Exact local input path rejected before parsing.
        path: PathBuf,
        /// Fixed byte limit enforced for this input class.
        max_bytes: usize,
    },
    /// A configuration line did not match the strict `KEY=value` grammar.
    #[error("invalid configuration at {path}:{line}: {reason}")]
    InvalidConfiguration {
        /// Exact configuration path containing the malformed line.
        path: PathBuf,
        /// One-based line number containing the malformed entry.
        line: usize,
        /// Non-secret reason the entry was rejected.
        reason: &'static str,
    },
    /// A configuration key occurred more than once.
    #[error("duplicate configuration key {key} at {path}:{line}")]
    DuplicateConfigurationKey {
        /// Exact configuration path containing the duplicate.
        path: PathBuf,
        /// One-based line number containing the duplicate occurrence.
        line: usize,
        /// Valid environment key that occurred more than once.
        key: String,
    },
    /// A local database path could not be represented safely in a line-oriented environment file.
    #[error("cannot encode {key} path {path} in the local configuration")]
    InvalidConfigurationPath {
        /// Environment key whose path could not be represented.
        key: &'static str,
        /// Path that was non-Unicode or contained a line break.
        path: PathBuf,
    },
    /// Production initialization refused to create state or proceed with missing authority values.
    #[error(
        "production initialization refused because required configuration is incomplete: {missing:?}"
    )]
    ProductionConfigurationIncomplete {
        /// Missing local paths or non-secret required configuration key names.
        missing: Vec<String>,
    },
    /// Quick initialization preserved an existing configuration that cannot boot local mode.
    #[error("quick initialization found incomplete local configuration: {next_step}")]
    QuickConfigurationIncomplete {
        /// Non-secret repair instruction produced by the same doctor contract.
        next_step: String,
    },
    /// JSON encoding of a local diagnostic report failed.
    #[error("cannot encode the doctor report as JSON: {0}")]
    Serialization(#[source] serde_json::Error),
    /// A live command was requested without binary integration supplying an authenticated client.
    #[error(
        "`{operation}` requires a live authenticated control API; start the server with operator control enabled and configure a typed client"
    )]
    ControlApiUnavailable {
        /// Stable operation name that needs live control-plane integration.
        operation: String,
    },
    /// Live HTTP control configuration was malformed or unsafe before a request was sent.
    #[error("invalid live-control configuration: {message}")]
    ControlConfiguration {
        /// Non-secret corrective action for the operator.
        message: &'static str,
    },
    /// The synchronous HTTP client could not be initialized.
    #[error("cannot initialize the live-control HTTP client: {0}")]
    ControlClient(#[source] reqwest::Error),
    /// An authenticated HTTP request could not be completed.
    #[error("{operation} could not reach the live control plane: {source}")]
    ControlTransport {
        /// Stable command name that failed to reach the control plane.
        operation: &'static str,
        /// Underlying transport failure without a server response body.
        #[source]
        source: reqwest::Error,
    },
    /// A live control endpoint rejected a request without exposing its response body.
    #[error("{operation} failed with HTTP status {status}")]
    ControlHttpStatus {
        /// Stable command name that the server rejected.
        operation: &'static str,
        /// Numeric HTTP status received without its potentially sensitive body.
        status: u16,
    },
    /// A successful live-control response exceeded the fixed resource limit.
    #[error("{operation} returned a response larger than 1 MiB")]
    ControlResponseTooLarge {
        /// Stable command name that produced an oversized response.
        operation: &'static str,
    },
    /// Reading a successful response body failed before its content was displayed.
    #[error("{operation} could not read the live-control response: {source}")]
    ControlResponseRead {
        /// Stable command name that received the unreadable response.
        operation: &'static str,
        /// Underlying read failure without any response body contents.
        #[source]
        source: io::Error,
    },
    /// A successful response did not match the command's expected public response format.
    #[error("{operation} returned an invalid live-control response")]
    InvalidControlResponse {
        /// Stable command name that received the malformed success response.
        operation: &'static str,
    },
    /// The selected command is intentionally not implemented by the alpha control surface.
    #[error("{operation} is unavailable in the alpha control plane")]
    UnsupportedControlOperation {
        /// Stable command name that is deliberately unavailable.
        operation: &'static str,
    },
}

/// Agent harness the generated roster will run.
///
/// Henosis is not tied to one agent implementation. The built-in Synapse loop is
/// the default because it needs no external binary, but any CLI that accepts a
/// prompt and answers on stdout can take its place.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Harness {
    /// The in-process Synapse agent loop with provider and tool access.
    #[default]
    Synapse,
    /// An external agent CLI, named by preset or by path.
    External(String),
}

/// Resolves harness names into generated roster configuration.
impl Harness {
    /// Parse a `--harness` value or `HENOSIS_HARNESS` setting.
    ///
    /// An unrecognized value is treated as a binary name or path rather than an
    /// error, so a harness Henosis has never heard of still works on day one.
    fn parse(value: &str) -> Self {
        match value.trim() {
            "" | "synapse" => Self::Synapse,
            other => Self::External(other.to_string()),
        }
    }

    /// The value recorded in the generated configuration file.
    fn config_value(&self) -> &str {
        match self {
            Self::Synapse => "synapse",
            Self::External(name) => name,
        }
    }

    /// Render the `[agents.executor]` block for this harness.
    ///
    /// Presets cover modes whose discussion path is known not to edit. Any
    /// other binary uses the explicit Henosis adapter contract, keeping its
    /// discussion and approved execution entry points distinct.
    fn executor_toml(&self) -> String {
        let executor_type_key = "type";
        let (binary, discuss, execute, inherit_env) = match self {
            Self::Synapse => {
                return format!(
                    "[agents.executor]\n\
                     {executor_type_key} = \"Synapse\"\n\
                     provider = \"anthropic\"\n\
                     # api_key = \"...\"   # or set ANTHROPIC_API_KEY in the environment\n\
                     model = \"claude-sonnet-4-6\"\n"
                );
            }
            Self::External(name) => match name.as_str() {
                "claude-code" | "claude" => {
                    return format!(
                        "[agents.executor]\n\
                         {executor_type_key} = \"ClaudeCode\"\n\
                         binary = \"claude\"\n\
                         model = \"sonnet\"\n"
                    );
                }
                "aider" => (
                    "aider",
                    vec![
                        "--chat-mode",
                        "ask",
                        "--message",
                        "{prompt}",
                        "--no-auto-commits",
                    ],
                    vec!["--message", "{prompt}", "--yes"],
                    vec!["HOME", "USERPROFILE", "XDG_CONFIG_HOME", "XDG_CACHE_HOME"],
                ),
                other => (
                    other,
                    vec!["--henosis-discuss", "{prompt}"],
                    vec!["--henosis-execute", "{prompt}"],
                    vec!["HOME", "USERPROFILE", "XDG_CONFIG_HOME"],
                ),
            },
        };
        let render = |args: &[&str]| {
            args.iter()
                .map(|arg| format!("\"{arg}\""))
                .collect::<Vec<_>>()
                .join(", ")
        };
        // JSON and TOML basic strings share the escaping emitted here, and
        // serde_json is already part of the CLI's public response surface.
        let binary = serde_json::to_string(binary).expect("string serialization cannot fail");
        let inherit_env = render(&inherit_env);
        format!(
            "[agents.executor]\n\
             {executor_type_key} = \"Command\"\n\
             binary = {binary}\n\
             discuss_args = [{}]\n\
             execute_args = [{}]\n\
             max_runtime_secs = 600\n\
             env_clear = true\n\
             inherit_env = [{inherit_env}]\n\
             # Add provider credential variable names to inherit_env only when\n\
             # the harness cannot load credentials from its own protected config.\n\
             # progress_format = \"jsonl\"   # if this harness streams JSON lines\n",
            render(&discuss),
            render(&execute),
        )
    }
}

/// Resolve the harness selection from the environment.
///
/// Used by both `henosis init --quick` and the automatic boot-time
/// initialization path, so a `HENOSIS_HARNESS` set for a `curl | sh` install
/// applies however initialization is reached.
pub fn harness_from_env() -> Harness {
    std::env::var(HARNESS_ENV)
        .ok()
        .as_deref()
        .map(Harness::parse)
        .unwrap_or_default()
}

/// Build the source-compatible quick-init command selected by the environment.
pub fn quick_init_command_from_env() -> Command {
    quick_init_command(harness_from_env())
}

/// Keep the established unit-style quick command for the built-in harness.
fn quick_init_command(harness: Harness) -> Command {
    match harness {
        Harness::Synapse => Command::Init(InitMode::Quick),
        external => Command::InitWithHarness(external),
    }
}

/// Parses the `init` subcommand and requires exactly one safe non-interactive flag.
///
/// `--harness` may accompany `--quick`. When it is absent the `HENOSIS_HARNESS`
/// environment variable is consulted so `curl | sh` installs can select a
/// harness without an interactive prompt.
fn parse_init(arguments: &[String]) -> Result<Command, CliError> {
    let default_harness = harness_from_env();
    match arguments {
        [] => Err(CliError::InitModeRequired),
        [flag] if flag == "--quick" => Ok(quick_init_command(default_harness)),
        [flag] if flag == "--production" => Ok(Command::Init(InitMode::Production)),
        [flag, harness_flag, value] if flag == "--quick" && harness_flag == "--harness" => {
            Ok(quick_init_command(Harness::parse(value)))
        }
        _ => Err(CliError::Usage {
            message:
                "usage: henosis init --quick [--harness <name|path>] | henosis init --production"
                    .to_string(),
        }),
    }
}

/// Parses diagnostic output flags while rejecting all ignored extra arguments.
fn parse_doctor(arguments: &[String]) -> Result<Command, CliError> {
    match arguments {
        [] => Ok(Command::Doctor(DoctorOutputFormat::Human)),
        [flag] if flag == "--json" => Ok(Command::Doctor(DoctorOutputFormat::Json)),
        _ => Err(CliError::Usage {
            message: "usage: henosis doctor [--json]".to_string(),
        }),
    }
}

/// Parses a command that accepts no additional arguments.
fn parse_empty(arguments: &[String], command: Command) -> Result<Command, CliError> {
    if arguments.is_empty() {
        Ok(command)
    } else {
        Err(CliError::Usage {
            message: "this command accepts no additional arguments".to_string(),
        })
    }
}

/// Parses `token create`, `token list`, and `token revoke` with strict arity checks.
fn parse_token(arguments: &[String]) -> Result<TokenCommand, CliError> {
    match arguments {
        [subcommand, label] if subcommand == "create" && !label.starts_with('-') => {
            Ok(TokenCommand::Create {
                label: nonempty_argument("token label", label)?,
                output: TokenOutputFormat::Human,
            })
        }
        [subcommand, label, flag]
            if subcommand == "create" && !label.starts_with('-') && flag == "--token-only" =>
        {
            Ok(TokenCommand::Create {
                label: nonempty_argument("token label", label)?,
                output: TokenOutputFormat::TokenOnly,
            })
        }
        [subcommand] if subcommand == "list" => Ok(TokenCommand::List),
        [subcommand, token_id] if subcommand == "revoke" => Ok(TokenCommand::Revoke {
            token_id: nonempty_argument("token identifier", token_id)?,
        }),
        _ => Err(CliError::Usage {
            message:
                "usage: henosis token create <label> [--token-only] | list | revoke <token-id>"
                    .to_string(),
        }),
    }
}

/// Parses `approvals list`, `approvals approve`, and `approvals deny` with strict arity checks.
fn parse_approvals(arguments: &[String]) -> Result<ApprovalCommand, CliError> {
    match arguments {
        [subcommand] if subcommand == "list" => Ok(ApprovalCommand::List),
        [subcommand, approval_id] if subcommand == "approve" => Ok(ApprovalCommand::Approve {
            approval_id: nonempty_argument("approval identifier", approval_id)?,
        }),
        [subcommand, approval_id] if subcommand == "deny" => Ok(ApprovalCommand::Deny {
            approval_id: nonempty_argument("approval identifier", approval_id)?,
        }),
        _ => Err(CliError::Usage {
            message: "usage: henosis approvals list | approve <approval-id> | deny <approval-id>"
                .to_string(),
        }),
    }
}

/// Parses the narrow audit grammar supported by this public operator surface.
fn parse_audit(arguments: &[String]) -> Result<Command, CliError> {
    match arguments {
        [subcommand] if subcommand == "verify" => Ok(Command::AuditVerify),
        _ => Err(CliError::Usage {
            message: "usage: henosis audit verify".to_string(),
        }),
    }
}

/// Rejects empty IDs and labels while preserving an opaque non-empty argument unchanged.
fn nonempty_argument(kind: &str, value: &str) -> Result<String, CliError> {
    if value.trim().is_empty() {
        return Err(CliError::Usage {
            message: format!("{kind} must not be empty"),
        });
    }
    Ok(value.to_string())
}

/// Returns an operation name suitable for precise unavailable-control-client errors.
fn control_operation_name(request: &ControlRequest) -> &'static str {
    match request {
        ControlRequest::Status => "henosis status",
        ControlRequest::Update => "henosis update",
        ControlRequest::Uninstall => "henosis uninstall",
        ControlRequest::Token(_) => "henosis token",
        ControlRequest::Approvals(_) => "henosis approvals",
        ControlRequest::AuditVerify => "henosis audit verify",
    }
}

/// Creates the local operator home, protected data directory, and bootable private configuration.
fn initialize_quick(paths: &CliPaths, harness: &Harness) -> Result<InitResult, CliError> {
    validate_harness_config_value(harness)?;
    ensure_private_directory(&paths.home)?;
    ensure_private_database_directory(&paths.data)?;
    create_private_config(paths, harness)?;
    create_agent_roster(paths, harness)?;
    let report = doctor(paths)?;
    if !report.local_ready {
        return Err(CliError::QuickConfigurationIncomplete {
            next_step: report.next_step,
        });
    }
    Ok(InitResult {
        mode: InitMode::Quick,
        config: paths.config.clone(),
        data: paths.data.clone(),
        next_step: "local configuration is ready; run `henosis serve`; room agents remain disabled until managed-room authorities are configured".to_string(),
    })
}

/// Rejects harness names that cannot be represented as one environment-file value.
fn validate_harness_config_value(harness: &Harness) -> Result<(), CliError> {
    let value = harness.config_value();
    if value.trim().is_empty()
        || value
            .chars()
            .any(|character| matches!(character, '\n' | '\r' | '\0'))
    {
        return Err(CliError::Usage {
            message: "harness name or path must be a non-empty single-line value".to_string(),
        });
    }
    Ok(())
}

/// Refuses production initialization unless existing local state names both required authorities.
fn initialize_production(paths: &CliPaths) -> Result<InitResult, CliError> {
    let report = doctor(paths)?;
    let mut missing = Vec::new();
    if report.home != LocalPathState::Directory {
        missing.push(format!("{} (directory)", paths.home.display()));
    }
    if report.config != LocalPathState::File {
        missing.push(format!("{} (regular file)", paths.config.display()));
    }
    if report.data != LocalPathState::Directory {
        missing.push(format!("{} (directory)", paths.data.display()));
    }
    missing.extend(report.missing_production_keys);
    if !missing.is_empty() {
        return Err(CliError::ProductionConfigurationIncomplete { missing });
    }
    Ok(InitResult {
        mode: InitMode::Production,
        config: paths.config.clone(),
        data: paths.data.clone(),
        next_step: "production authority configuration is complete; start Henosis through the integrated runtime".to_string(),
    })
}

/// Inspects local bootstrap state without mutating files or contacting a control plane.
fn doctor(paths: &CliPaths) -> Result<DoctorReport, CliError> {
    let home = inspect_path(&paths.home)?;
    let config = inspect_path(&paths.config)?;
    let data = inspect_path(&paths.data)?;
    let values = if config == LocalPathState::File {
        read_config_values(&paths.config)?
    } else {
        BTreeMap::new()
    };
    let mut missing_production_keys = missing_config_keys(&values, PRODUCTION_REQUIRED_KEYS);
    if config_value_present(&values, "SYNTHEOS_LOCAL_POLICY") {
        missing_production_keys.push(
            "SYNTHEOS_LOCAL_POLICY (remove this local-only setting for production)".to_string(),
        );
    }
    let room_mode = effective_room_mode(&values);
    if room_mode != Some(RoomMode::Required) {
        missing_production_keys
            .push("HENOSIS_ROOM_MODE (must be required for production)".to_string());
    }
    let local_ready = home == LocalPathState::Directory
        && config == LocalPathState::File
        && data == LocalPathState::Directory
        && local_configuration_ready(&values, room_mode);
    let production_ready = home == LocalPathState::Directory
        && config == LocalPathState::File
        && data == LocalPathState::Directory
        && missing_production_keys.is_empty();
    let next_step = if home != LocalPathState::Directory
        || config != LocalPathState::File
        || data != LocalPathState::Directory
    {
        "run `henosis init --quick` to create only missing local paths".to_string()
    } else if !local_ready {
        format!(
            "repair the required local identity, JWT, room mode, and database values in {}",
            paths.config.display()
        )
    } else if !production_ready {
        format!(
            "set the required PostgreSQL, managed-room, phylaxd, and witness values in {}, remove SYNTHEOS_LOCAL_POLICY, select HENOSIS_ROOM_MODE=required, then run `henosis init --production`",
            paths.config.display()
        )
    } else {
        "local and production configuration are complete; start Henosis through the integrated runtime".to_string()
    };
    Ok(DoctorReport {
        home,
        config,
        data,
        missing_production_keys,
        local_ready,
        production_ready,
        next_step,
    })
}

/// Creates or validates the database directory through the shared hardened path boundary.
fn ensure_private_database_directory(path: &Path) -> Result<(), CliError> {
    henosis_sqlite::ensure_private_directory(path).map_err(|error| CliError::Filesystem {
        path: path.to_path_buf(),
        source: io::Error::other(error),
    })
}

/// Creates a missing directory with owner-only permissions without changing an existing path.
fn ensure_private_directory(path: &Path) -> Result<(), CliError> {
    match inspect_path(path)? {
        LocalPathState::Directory => Ok(()),
        LocalPathState::Missing => {
            fs::create_dir_all(path).map_err(|source| CliError::Filesystem {
                path: path.to_path_buf(),
                source,
            })?;
            require_path_state(path, LocalPathState::Directory, "directory")?;
            set_unix_mode(path, 0o700)
        }
        state => Err(unexpected_path_type(path, "directory", state)),
    }
}

/// Creates configuration exactly once while preserving every byte of an existing regular file.
fn create_private_config(paths: &CliPaths, harness: &Harness) -> Result<(), CliError> {
    if inspect_path(&paths.config)? == LocalPathState::File {
        let file = open_regular_config(&paths.config)?;
        return set_unix_file_mode(&file, &paths.config, 0o600);
    }
    let configuration = render_quick_config(paths, harness)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&paths.config) {
        Ok(mut file) => {
            file.write_all(configuration.as_bytes())
                .map_err(|source| CliError::Filesystem {
                    path: paths.config.clone(),
                    source,
                })?;
            file.sync_all().map_err(|source| CliError::Filesystem {
                path: paths.config.clone(),
                source,
            })?;
            set_unix_file_mode(&file, &paths.config, 0o600)?;
        }
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
            let file = open_regular_config(&paths.config)?;
            set_unix_file_mode(&file, &paths.config, 0o600)?;
        }
        Err(source) => {
            return Err(CliError::Filesystem {
                path: paths.config.clone(),
                source,
            });
        }
    }
    Ok(())
}

/// Writes the starter agent roster naming the selected harness.
///
/// Never overwrites an existing roster: the operator's edits to their own agent
/// definitions outrank a regenerated template, and `init --quick` is documented
/// as idempotent.
fn create_agent_roster(paths: &CliPaths, harness: &Harness) -> Result<(), CliError> {
    let roster_path = paths.home.join(AGENTS_FILE);
    if inspect_path(&roster_path)? != LocalPathState::Missing {
        return Ok(());
    }
    let roster = render_agent_roster(harness);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&roster_path) {
        Ok(mut file) => {
            file.write_all(roster.as_bytes())
                .map_err(|source| CliError::Filesystem {
                    path: roster_path.clone(),
                    source,
                })?;
            file.sync_all().map_err(|source| CliError::Filesystem {
                path: roster_path.clone(),
                source,
            })?;
            set_unix_file_mode(&file, &roster_path, 0o600)?;
            Ok(())
        }
        // A roster that appeared between the check and the create is still the
        // operator's, so leave it alone rather than racing it.
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(source) => Err(CliError::Filesystem {
            path: roster_path,
            source,
        }),
    }
}

/// Renders the starter roster body for one harness.
fn render_agent_roster(harness: &Harness) -> String {
    format!(
        "# Henosis agent roster.\n\
         #\n\
         # Henosis is not tied to one agent implementation. `type` selects the\n\
         # harness: \"Synapse\" (built in), \"ClaudeCode\", or \"Command\" for any\n\
         # external agent CLI. A Command harness receives the prompt as a single\n\
         # argument wherever {{prompt}} appears, and answers on stdout.\n\
         #\n\
         # Re-run `henosis init --quick --harness <name|path>` in a fresh home to\n\
         # regenerate this file for a different harness. Known presets: synapse,\n\
         # claude-code, aider. Other binaries implement --henosis-discuss and\n\
         # --henosis-execute as separate adapter modes.\n\
         \n\
         [[agents]]\n\
         name = \"Assistant\"\n\
         username = \"assistant\"\n\
         base_chance = 1.0\n\
         system_prompt = \"You are a helpful engineering agent.\"\n\
         \n\
         {}",
        harness.executor_toml()
    )
}

/// Renders generated identities, local JWT material, and absolute persistent database paths.
fn render_quick_config(paths: &CliPaths, harness: &Harness) -> Result<String, CliError> {
    validate_harness_config_value(harness)?;
    let home = fs::canonicalize(&paths.home).map_err(|source| CliError::Filesystem {
        path: paths.home.clone(),
        source,
    })?;
    let data = fs::canonicalize(&paths.data).map_err(|source| CliError::Filesystem {
        path: paths.data.clone(),
        source,
    })?;
    let tenant = TenantId::new();
    let principal = PrincipalId::new();
    let jwt_secret = hex::encode(rand::random::<[u8; 32]>());
    let roster_path = config_path_value("HENOSIS_RIFT_BRIDGE_CONFIG", &home.join(AGENTS_FILE))?;
    let mut configuration = format!(
        "# Henosis private local operator configuration.\n\
         # This file contains local authority material. Do not share it.\n\
         SYNTHEOS_ADDR=127.0.0.1:8088\n\
         SYNTHEOS_LOCAL_POLICY=1\n\
         HENOSIS_ROOM_MODE={DISABLED_MODE}\n\
         SYNTHEOS_PLUTUS_OPERATOR_TENANT={tenant}\n\
         SYNTHEOS_PLUTUS_OPERATOR_PRINCIPAL={principal}\n\
         SYNTHEOS_OPERATOR_JWT_SECRET={jwt_secret}\n\
         HENOSIS_HARNESS={harness}\n\
         HENOSIS_RIFT_BRIDGE_CONFIG={roster_path}\n",
        harness = harness.config_value()
    );
    for &(key, filename) in LOCAL_DATABASES {
        let path = config_path_value(key, &data.join(filename))?;
        configuration.push_str(key);
        configuration.push('=');
        configuration.push_str(&path);
        configuration.push('\n');
    }
    let local_token_path = config_path_value(LOCAL_TOKEN_KEY, &data.join(LOCAL_TOKEN_FILE))?;
    configuration.push_str(LOCAL_TOKEN_KEY);
    configuration.push('=');
    configuration.push_str(&local_token_path);
    configuration.push('\n');
    configuration.push_str(
        "# Production authority values are deliberately absent.\n\
         # SYNTHEOS_PLUTUS_DB=\n\
         # PHYLAXD_URL=\n\
         # HERMES_PHYLAXD_TOKEN=\n\
         # HENOSIS_WITNESS_URL=\n\
         # HENOSIS_AUDIT_ORIGIN_KEY_FILE=\n\
         # HENOSIS_AUDIT_ORIGIN_KEY_ID=\n\
         # HENOSIS_WITNESS_PUBLIC_KEY_FILE=\n\
         # HENOSIS_WITNESS_KEY_ID=\n",
    );
    Ok(configuration)
}

/// Converts one database path into a safe single-line Unicode environment value.
fn config_path_value(key: &'static str, path: &Path) -> Result<String, CliError> {
    let Some(value) = path.to_str() else {
        return Err(CliError::InvalidConfigurationPath {
            key,
            path: path.to_path_buf(),
        });
    };
    if value
        .chars()
        .any(|character| matches!(character, '\n' | '\r' | '\0'))
    {
        return Err(CliError::InvalidConfigurationPath {
            key,
            path: path.to_path_buf(),
        });
    }
    Ok(value.to_string())
}

/// Loads strict local `KEY=value` entries without shell evaluation or overriding process values.
pub fn load_local_environment(paths: &CliPaths) -> Result<(), CliError> {
    let values = read_config_values(&paths.config)?;
    for (key, value) in values {
        if env::var_os(&key).is_none() {
            env::set_var(key, value);
        }
    }
    Ok(())
}

/// Loads local configuration when present and permits environment-only production startup.
pub fn load_local_environment_if_present(paths: &CliPaths) -> Result<bool, CliError> {
    match inspect_path(&paths.config)? {
        LocalPathState::Missing => Ok(false),
        LocalPathState::File => {
            load_local_environment(paths)?;
            Ok(true)
        }
        state => Err(unexpected_path_type(&paths.config, "regular file", state)),
    }
}

/// Reads strict environment-style values without shell expansion or credential disclosure.
fn read_config_values(path: &Path) -> Result<BTreeMap<String, String>, CliError> {
    let file = open_private_config(path)?;
    let mut contents = read_bounded_string(path, file, MAX_LOCAL_CONFIG_BYTES)?;
    let values = parse_config_values(path, &contents);
    contents.zeroize();
    values
}

/// Parses a strict line-oriented environment file and rejects malformed or duplicate entries.
fn parse_config_values(path: &Path, contents: &str) -> Result<BTreeMap<String, String>, CliError> {
    let mut values = BTreeMap::new();
    for (index, line) in contents.lines().enumerate() {
        let line_number = index + 1;
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(CliError::InvalidConfiguration {
                path: path.to_path_buf(),
                line: line_number,
                reason: "expected KEY=value",
            });
        };
        if !valid_environment_key(key) {
            return Err(CliError::InvalidConfiguration {
                path: path.to_path_buf(),
                line: line_number,
                reason: "environment key must match [A-Za-z_][A-Za-z0-9_]*",
            });
        }
        if value.contains('\0') {
            return Err(CliError::InvalidConfiguration {
                path: path.to_path_buf(),
                line: line_number,
                reason: "environment value contains a NUL byte",
            });
        }
        if values.insert(key.to_string(), value.to_string()).is_some() {
            return Err(CliError::DuplicateConfigurationKey {
                path: path.to_path_buf(),
                line: line_number,
                key: key.to_string(),
            });
        }
    }
    Ok(values)
}

/// Validates a portable environment variable key without accepting shell syntax.
fn valid_environment_key(key: &str) -> bool {
    let mut characters = key.bytes();
    let Some(first) = characters.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    characters.all(|character| character.is_ascii_alphanumeric() || character == b'_')
}

/// Opens an operator-selected local configuration through one validated descriptor.
#[cfg(any(unix, windows))]
fn open_regular_config(path: &Path) -> Result<File, CliError> {
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
    let file = options
        .open(path)
        .map_err(|source| classify_config_open_error(path, source))?;
    let opened = file.metadata().map_err(|source| CliError::Filesystem {
        path: path.to_path_buf(),
        source,
    })?;
    if !opened.file_type().is_file() {
        return Err(unexpected_path_type(
            path,
            "regular file",
            LocalPathState::UnsafeOrUnexpected,
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileType, FILE_ATTRIBUTE_REPARSE_POINT, FILE_TYPE_DISK,
        };

        // SAFETY: the raw handle remains owned and valid for the duration of this call.
        let file_type = unsafe { GetFileType(file.as_raw_handle()) };
        if opened.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || file_type != FILE_TYPE_DISK
        {
            return Err(unexpected_path_type(
                path,
                "non-reparse local disk file",
                LocalPathState::UnsafeOrUnexpected,
            ));
        }
    }
    Ok(file)
}

/// Fails closed where the runtime has no descriptor-level no-follow implementation.
#[cfg(not(any(unix, windows)))]
fn open_regular_config(path: &Path) -> Result<File, CliError> {
    Err(CliError::InvalidConfiguration {
        path: path.to_path_buf(),
        line: 0,
        reason: "secure configuration opens are unsupported on this platform",
    })
}

/// Classify no-follow rejections without performing a pathname precheck.
#[cfg(any(unix, windows))]
fn classify_config_open_error(path: &Path, source: io::Error) -> CliError {
    #[cfg(unix)]
    if source.raw_os_error() == Some(libc::ELOOP) {
        return unexpected_path_type(path, "regular file", LocalPathState::UnsafeOrUnexpected);
    }
    CliError::Filesystem {
        path: path.to_path_buf(),
        source,
    }
}

/// Opens a stable regular configuration and rejects non-owner Unix permission bits.
fn open_private_config(path: &Path) -> Result<File, CliError> {
    let file = open_regular_config(path)?;
    let metadata = file.metadata().map_err(|source| CliError::Filesystem {
        path: path.to_path_buf(),
        source,
    })?;
    require_owner_only_permissions(path, &metadata)?;
    Ok(file)
}

/// Reads one UTF-8 file through an opened descriptor without exceeding its fixed byte budget.
fn read_bounded_string(path: &Path, file: File, max_bytes: usize) -> Result<String, CliError> {
    let mut contents = String::new();
    if let Err(source) = file
        .take(max_bytes as u64 + 1)
        .read_to_string(&mut contents)
    {
        contents.zeroize();
        return Err(CliError::Filesystem {
            path: path.to_path_buf(),
            source,
        });
    }
    if contents.len() > max_bytes {
        contents.zeroize();
        return Err(CliError::LocalFileTooLarge {
            path: path.to_path_buf(),
            max_bytes,
        });
    }
    Ok(contents)
}

/// Returns required key names whose values are absent or blank without returning their contents.
fn missing_config_keys(values: &BTreeMap<String, String>, keys: &[&str]) -> Vec<String> {
    keys.iter()
        .filter(|key| !config_value_present(values, key))
        .map(|key| (*key).to_string())
        .collect()
}

/// Determines whether one configuration key is present with a non-blank value.
fn config_value_present(values: &BTreeMap<String, String>, key: &str) -> bool {
    values
        .get(key)
        .is_some_and(|value| !value.trim().is_empty())
}

/// Resolves room mode with the same process-environment precedence used by server startup.
fn effective_room_mode(values: &BTreeMap<String, String>) -> Option<RoomMode> {
    let mode = match env::var("HENOSIS_ROOM_MODE") {
        Ok(value) => value,
        Err(env::VarError::NotPresent) => values
            .get("HENOSIS_ROOM_MODE")
            .cloned()
            .unwrap_or_else(|| REQUIRED_MODE.to_string()),
        Err(env::VarError::NotUnicode(_)) => return None,
    };
    parse_room_mode(&mode).ok()
}

/// Validates every generated local value needed for the private runtime to boot.
fn local_configuration_ready(
    values: &BTreeMap<String, String>,
    room_mode: Option<RoomMode>,
) -> bool {
    if values
        .get("SYNTHEOS_LOCAL_POLICY")
        .is_none_or(|value| value != "1")
    {
        return false;
    }
    if values
        .get("SYNTHEOS_PLUTUS_OPERATOR_TENANT")
        .is_none_or(|value| value.parse::<TenantId>().is_err())
        || values
            .get("SYNTHEOS_PLUTUS_OPERATOR_PRINCIPAL")
            .is_none_or(|value| value.parse::<PrincipalId>().is_err())
    {
        return false;
    }
    let jwt_ready = values
        .get("SYNTHEOS_OPERATOR_JWT_SECRET")
        .and_then(|value| hex::decode(value).ok())
        .is_some_and(|secret| secret.len() >= 32);
    let local_token_ready = values
        .get(LOCAL_TOKEN_KEY)
        .is_some_and(|value| Path::new(value).is_absolute());
    room_mode == Some(RoomMode::Disabled)
        && jwt_ready
        && local_token_ready
        && LOCAL_DATABASES.iter().all(|(key, _)| {
            values
                .get(*key)
                .is_some_and(|value| Path::new(value).is_absolute())
        })
}

/// Inspects a filesystem entry without following a potentially unsafe symbolic link.
fn inspect_path(path: &Path) -> Result<LocalPathState, CliError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(LocalPathState::Directory),
        Ok(metadata) if metadata.file_type().is_file() => Ok(LocalPathState::File),
        Ok(_) => Ok(LocalPathState::UnsafeOrUnexpected),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(LocalPathState::Missing),
        Err(source) => Err(CliError::Filesystem {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Rejects a path whose current type does not satisfy one explicit operation's contract.
fn require_path_state(
    path: &Path,
    expected_state: LocalPathState,
    expected: &'static str,
) -> Result<(), CliError> {
    let state = inspect_path(path)?;
    if state == expected_state {
        Ok(())
    } else {
        Err(unexpected_path_type(path, expected, state))
    }
}

/// Builds a typed error that identifies a path problem without following symlinks.
fn unexpected_path_type(path: &Path, expected: &'static str, state: LocalPathState) -> CliError {
    let found = match state {
        LocalPathState::Missing => "missing path",
        LocalPathState::Directory => "directory",
        LocalPathState::File => "regular file",
        LocalPathState::UnsafeOrUnexpected => "symlink or unsupported filesystem entry",
    };
    CliError::UnexpectedPathType {
        path: path.to_path_buf(),
        expected,
        found,
    }
}

/// Rejects Unix configuration files accessible by group or other users.
#[cfg(unix)]
fn require_owner_only_permissions(path: &Path, metadata: &fs::Metadata) -> Result<(), CliError> {
    use std::os::unix::fs::PermissionsExt;

    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 == 0 {
        Ok(())
    } else {
        Err(CliError::InsecureConfigPermissions {
            path: path.to_path_buf(),
            mode,
        })
    }
}

/// Accepts platform-native file access controls where Unix permission bits do not exist.
#[cfg(not(unix))]
fn require_owner_only_permissions(_path: &Path, _metadata: &fs::Metadata) -> Result<(), CliError> {
    Ok(())
}

/// Applies Unix owner-only permissions and becomes a no-op on platforms without Unix modes.
#[cfg(unix)]
fn set_unix_mode(path: &Path, mode: u32) -> Result<(), CliError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|source| {
        CliError::Filesystem {
            path: path.to_path_buf(),
            source,
        }
    })
}

/// Keeps local initialization portable where the operating system has no Unix permission model.
#[cfg(not(unix))]
fn set_unix_mode(_path: &Path, _mode: u32) -> Result<(), CliError> {
    Ok(())
}

/// Applies owner-only permissions through an already-open Unix file handle.
#[cfg(unix)]
fn set_unix_file_mode(file: &File, path: &Path, mode: u32) -> Result<(), CliError> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(mode))
        .map_err(|source| CliError::Filesystem {
            path: path.to_path_buf(),
            source,
        })
}

/// Keeps configuration creation portable where the operating system has no Unix mode bits.
#[cfg(not(unix))]
fn set_unix_file_mode(_file: &File, _path: &Path, _mode: u32) -> Result<(), CliError> {
    Ok(())
}

/// Exercises parser strictness, local idempotence, and unavailable-control behavior.
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Creates a unique temporary directory without relying on a shell command.
    fn temporary_home() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after Unix epoch")
            .as_nanos();
        env::temp_dir().join(format!("henosis-cli-test-{}-{nonce}", std::process::id()))
    }

    /// Removes a single test directory after its assertions complete.
    fn remove_temporary_home(path: &Path) {
        fs::remove_dir_all(path).expect("remove test home");
    }

    /// Builds a unique valid environment key for tests that mutate process environment state.
    fn unique_environment_key(suffix: &str) -> String {
        format!(
            "HENOSIS_CLI_TEST_{}_{}",
            TenantId::new().to_string().replace('-', "_"),
            suffix
        )
    }

    /// Writes a test configuration with the same private mode required by the loader.
    fn write_private_test_config(paths: &CliPaths, contents: &str) {
        fs::create_dir_all(&paths.home).expect("create test home");
        fs::write(&paths.config, contents).expect("write test configuration");
        set_unix_mode(&paths.config, 0o600).expect("set private test mode");
    }

    /// Builds an HTTP client from fixed test values without mutating process environment state.
    fn test_http_client(base_url: Url) -> HttpControlApi {
        HttpControlApi {
            base_url,
            client: BlockingHttpClient::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(CONTROL_REQUEST_TIMEOUT)
                .build()
                .expect("build test HTTP client"),
            bearer_token: "test-bearer-token".to_string(),
        }
    }

    /// Starts one minimal HTTP server and returns the request bytes it observes.
    fn one_response_server(response: &'static str) -> (Url, mpsc::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test listener");
        let address = listener.local_addr().expect("read local listener address");
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept client request");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("set request read timeout");
            let mut request = vec![0; 4096];
            let read = stream.read(&mut request).expect("read client request");
            request.truncate(read);
            sender.send(request).expect("send observed request");
            stream
                .write_all(response.as_bytes())
                .expect("write local test response");
        });
        (
            validate_control_url(&format!("http://{address}")).expect("validate loopback URL"),
            receiver,
        )
    }

    /// Accepts HTTPS endpoints and loopback HTTP endpoints while rejecting unsafe URL shapes.
    #[test]
    fn control_url_validation_rejects_unsafe_endpoints() {
        assert!(validate_control_url("https://control.example/api").is_ok());
        assert!(validate_control_url("http://127.0.0.1:8088").is_ok());
        assert!(validate_control_url("http://[::1]:8088").is_ok());
        assert!(validate_control_url("http://control.example").is_err());
        assert!(validate_control_url("https://user:secret@control.example").is_err());
        assert!(validate_control_url("https://control.example/?q=1").is_err());
        assert!(validate_control_url("https://control.example/#fragment").is_err());
    }

    /// Escapes opaque identifiers so a token or approval ID cannot alter the fixed API route.
    #[test]
    fn control_url_escapes_opaque_path_segments() {
        let base = validate_control_url("https://control.example/base").expect("validate URL");
        let url = control_url(
            &base,
            &["api", "v1", "tokens", "id/with?reserved", "revoke"],
        )
        .expect("build control URL");
        assert_eq!(
            url.path(),
            "/base/api/v1/tokens/id%2Fwith%3Freserved/revoke"
        );
        assert!(url.query().is_none());
        assert!(control_url(&base, &["api", ".", "health"]).is_err());
        assert!(control_url(&base, &["api", "..", "health"]).is_err());
        assert!(control_url(&base, &["api", "", "health"]).is_err());
    }

    /// Rejects bearer credentials that could form ambiguous authorization header values.
    #[test]
    fn bearer_token_validation_rejects_whitespace() {
        assert!(validate_bearer_token("valid-token".to_string()).is_ok());
        assert!(validate_bearer_token("token with spaces".to_string()).is_err());
        assert!(validate_bearer_token("\n".to_string()).is_err());
    }

    /// Rejects an oversized private bearer-token file before accepting any credential bytes.
    #[test]
    fn bearer_token_loader_enforces_fixed_size_limit() {
        let home = temporary_home();
        fs::create_dir_all(&home).expect("create test home");
        let path = home.join(LOCAL_TOKEN_FILE);
        fs::write(&path, "a".repeat(MAX_LOCAL_TOKEN_BYTES + 1)).expect("write oversized token");
        set_unix_mode(&path, 0o600).expect("set private token mode");
        assert!(matches!(
            load_local_bearer_token(&path),
            Err(CliError::LocalFileTooLarge {
                max_bytes: MAX_LOCAL_TOKEN_BYTES,
                ..
            })
        ));
        remove_temporary_home(&home);
    }

    /// Sends status to the health route with bearer authentication and bounded text output.
    #[test]
    fn status_maps_to_authenticated_health_request() {
        let (base_url, requests) = one_response_server(
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
        );
        let output = test_http_client(base_url)
            .execute(ControlRequest::Status)
            .expect("execute status request");
        let request =
            String::from_utf8(requests.recv().expect("receive request")).expect("request is ASCII");
        assert!(request.starts_with("GET /health HTTP/1.1\r\n"));
        assert!(request.contains("authorization: Bearer test-bearer-token\r\n"));
        assert_eq!(output.message, "ok");
    }

    /// Rejects alpha-only maintenance commands before issuing a network request.
    #[test]
    fn alpha_maintenance_commands_are_explicitly_unsupported() {
        let client = test_http_client(
            validate_control_url("http://127.0.0.1:8088").expect("validate loopback URL"),
        );
        assert!(matches!(
            client.execute(ControlRequest::Update),
            Err(CliError::UnsupportedControlOperation { .. })
        ));
        assert!(matches!(
            client.execute(ControlRequest::Uninstall),
            Err(CliError::UnsupportedControlOperation { .. })
        ));
    }

    /// Parses every required top-level command without accepting implicit shell syntax.
    #[test]
    fn parses_supported_commands() {
        assert_eq!(
            Command::parse(&["init".into(), "--quick".into()]).expect("parse init"),
            Command::Init(InitMode::Quick)
        );
        assert_eq!(
            Command::parse(&["init".into(), "--production".into()]).expect("parse production init"),
            Command::Init(InitMode::Production)
        );
        assert_eq!(
            Command::parse(&["doctor".into()]).expect("parse doctor"),
            Command::Doctor(DoctorOutputFormat::Human)
        );
        assert_eq!(
            Command::parse(&["doctor".into(), "--json".into()]).expect("parse json doctor"),
            Command::Doctor(DoctorOutputFormat::Json)
        );
        assert_eq!(
            Command::parse(&[]).expect("parse empty arguments"),
            Command::Help
        );
        assert_eq!(
            Command::parse(&["--version".into()]).expect("parse version"),
            Command::Version
        );
        assert_eq!(
            Command::parse(&["status".into()]).expect("parse status"),
            Command::Status
        );
        assert_eq!(
            Command::parse(&["update".into()]).expect("parse update"),
            Command::Update
        );
        assert_eq!(
            Command::parse(&["uninstall".into()]).expect("parse uninstall"),
            Command::Uninstall
        );
        assert_eq!(
            Command::parse(&["token".into(), "create".into(), "operator".into()])
                .expect("parse token create"),
            Command::Token(TokenCommand::Create {
                label: "operator".into(),
                output: TokenOutputFormat::Human,
            })
        );
        assert_eq!(
            Command::parse(&["token".into(), "list".into()]).expect("parse token list"),
            Command::Token(TokenCommand::List)
        );
        assert_eq!(
            Command::parse(&["token".into(), "revoke".into(), "opaque-token".into()])
                .expect("parse token revoke"),
            Command::Token(TokenCommand::Revoke {
                token_id: "opaque-token".into()
            })
        );
        assert_eq!(
            Command::parse(&["approvals".into(), "list".into()]).expect("parse approvals"),
            Command::Approvals(ApprovalCommand::List)
        );
        assert_eq!(
            Command::parse(&["approvals".into(), "approve".into(), "approval-1".into()])
                .expect("parse approval"),
            Command::Approvals(ApprovalCommand::Approve {
                approval_id: "approval-1".into()
            })
        );
        assert_eq!(
            Command::parse(&["approvals".into(), "deny".into(), "approval-1".into()])
                .expect("parse denial"),
            Command::Approvals(ApprovalCommand::Deny {
                approval_id: "approval-1".into()
            })
        );
        assert_eq!(
            Command::parse(&["audit".into(), "verify".into()]).expect("parse audit"),
            Command::AuditVerify
        );
        assert_eq!(
            Command::parse(&["serve".into()]).expect("parse serve"),
            Command::Serve
        );
    }

    /// Parses the exact trailing token-only flag without changing default token creation.
    #[test]
    fn token_create_parses_explicit_output_format() {
        assert_eq!(
            Command::parse(&[
                "token".into(),
                "create".into(),
                "first-agent".into(),
                "--token-only".into(),
            ])
            .expect("parse token-only creation"),
            Command::Token(TokenCommand::Create {
                label: "first-agent".into(),
                output: TokenOutputFormat::TokenOnly,
            })
        );
        assert!(Command::parse(&[
            "token".into(),
            "create".into(),
            "first-agent".into(),
            "--unknown".into(),
        ])
        .is_err());
        assert!(
            Command::parse(&["token".into(), "create".into(), "--token-only".into(),]).is_err()
        );
        assert!(Command::parse(&[
            "token".into(),
            "create".into(),
            "--token-only".into(),
            "first-agent".into(),
        ])
        .is_err());
    }

    /// Renders token-only output raw while preserving the established human message byte-for-byte.
    #[test]
    fn token_create_renders_selected_output_format() {
        let token = "henosis-machine-token";
        assert_eq!(
            render_created_token(token.to_string(), TokenOutputFormat::Human),
            "token created; save this one-time credential now:\nhenosis-machine-token"
        );
        assert_eq!(
            render_created_token(token.to_string(), TokenOutputFormat::TokenOnly),
            token
        );
    }

    /// Rejects an argument shape that could otherwise conceal ignored shell-like input.
    #[test]
    fn rejects_extra_arguments() {
        assert!(Command::parse(&["status".into(), ";".into(), "whoami".into()]).is_err());
    }

    /// An explicit harness flag selects an external harness.
    #[test]
    fn harness_flag_selects_external_harness() {
        assert_eq!(
            Command::parse(&[
                "init".into(),
                "--quick".into(),
                "--harness".into(),
                "aider".into()
            ])
            .expect("parse harness init"),
            Command::InitWithHarness(Harness::External("aider".to_string()))
        );
    }

    /// An unknown harness name is accepted as a binary rather than rejected.
    #[test]
    fn unknown_harness_is_treated_as_a_binary() {
        assert_eq!(
            Harness::parse("/opt/bin/my-agent"),
            Harness::External("/opt/bin/my-agent".to_string())
        );
    }

    /// The built-in harness stays the default.
    #[test]
    fn synapse_is_the_default_harness() {
        assert_eq!(Harness::parse("synapse"), Harness::Synapse);
        assert_eq!(Harness::parse(""), Harness::Synapse);
        assert_eq!(Harness::default(), Harness::Synapse);
    }

    /// Presets render a Command executor with a whole-element prompt placeholder.
    #[test]
    fn external_preset_renders_command_executor() {
        let toml = Harness::External("aider".to_string()).executor_toml();
        assert!(toml.contains("type = \"Command\""));
        assert!(toml.contains("binary = \"aider\""));
        assert!(toml.contains("\"{prompt}\""));
        assert!(toml.contains("inherit_env = [\"HOME\""));
        assert!(!toml.contains("ANTHROPIC_API_KEY"));
    }

    /// The Claude Code preset uses the dedicated executor, not the generic one.
    #[test]
    fn claude_code_preset_uses_its_own_executor() {
        let toml = Harness::External("claude-code".to_string()).executor_toml();
        assert!(toml.contains("type = \"ClaudeCode\""));
    }

    /// Quick initialization writes a roster naming the selected harness.
    #[test]
    fn quick_init_writes_roster_for_selected_harness() {
        let home = temporary_home();
        let paths = CliPaths::from_home(&home);
        initialize_quick(&paths, &Harness::External("codex".to_string()))
            .expect("initialize with harness");

        let roster = fs::read_to_string(home.join(AGENTS_FILE)).expect("read roster");
        assert!(roster.contains("type = \"Command\""));
        assert!(roster.contains("binary = \"codex\""));
        assert!(roster.contains("--henosis-discuss"));
        assert!(roster.contains("--henosis-execute"));

        let values = read_config_values(&paths.config).expect("read generated configuration");
        assert_eq!(values["HENOSIS_HARNESS"], "codex");
        let roster_path = fs::canonicalize(home.join(AGENTS_FILE)).expect("canonical roster");
        assert_eq!(
            PathBuf::from(&values["HENOSIS_RIFT_BRIDGE_CONFIG"]),
            roster_path
        );
        let managed = henosis_rift_bridge::config::BridgeConfig::load_for_managed_room(
            &roster_path,
            "http://127.0.0.1:3200".to_string(),
            "ws://127.0.0.1:3200/ws".to_string(),
            "j".repeat(32),
            "b".repeat(32),
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
        )
        .expect("generated roster is consumable by managed runtime");
        assert_eq!(managed.agents.len(), 1);
    }

    /// A harness value cannot inject another line into the private configuration.
    #[test]
    fn quick_init_rejects_multiline_harness_before_writing_state() {
        let home = temporary_home();
        let paths = CliPaths::from_home(&home);
        let error = initialize_quick(
            &paths,
            &Harness::External("adapter\nHENOSIS_ROOM_MODE=required".to_string()),
        )
        .expect_err("multiline harness must be rejected");

        assert!(matches!(error, CliError::Usage { .. }));
        assert_eq!(
            inspect_path(&paths.config).expect("inspect config"),
            LocalPathState::Missing
        );
        assert_eq!(
            inspect_path(&paths.home).expect("inspect home"),
            LocalPathState::Missing
        );
    }

    /// The established quick-init mode remains Copy and source-compatible.
    #[test]
    fn quick_init_mode_remains_copy() {
        let first = InitMode::Quick;
        let second = first;
        assert_eq!(first, second);
    }

    /// An existing roster is never overwritten by a repeat initialization.
    #[test]
    fn quick_init_preserves_an_existing_roster() {
        let home = temporary_home();
        let paths = CliPaths::from_home(&home);
        initialize_quick(&paths, &Harness::Synapse).expect("first initialization");

        let roster_path = home.join(AGENTS_FILE);
        fs::write(&roster_path, "# operator edited\n").expect("edit roster");
        initialize_quick(&paths, &Harness::External("aider".to_string()))
            .expect("second initialization");

        assert_eq!(
            fs::read_to_string(&roster_path).expect("read roster"),
            "# operator edited\n"
        );
    }

    /// Generates bootable identities, a strong JWT secret, and absolute local database paths.
    #[test]
    fn quick_init_generates_bootable_private_configuration() {
        let home = temporary_home();
        let paths = CliPaths::from_home(&home);
        initialize_quick(&paths, &Harness::Synapse).expect("quick initialization");
        let values = read_config_values(&paths.config).expect("read generated configuration");

        let tenant = values["SYNTHEOS_PLUTUS_OPERATOR_TENANT"]
            .parse::<TenantId>()
            .expect("generated tenant identifier");
        let principal = values["SYNTHEOS_PLUTUS_OPERATOR_PRINCIPAL"]
            .parse::<PrincipalId>()
            .expect("generated principal identifier");
        assert_eq!(tenant.as_uuid().get_version_num(), 8);
        assert_eq!(principal.as_uuid().get_version_num(), 8);

        let jwt = hex::decode(&values["SYNTHEOS_OPERATOR_JWT_SECRET"])
            .expect("generated JWT secret is hex");
        assert!(jwt.len() >= 32);
        assert!(!values.contains_key("SYNTHEOS_OPERATOR_PASSWORD"));
        assert_eq!(values["HENOSIS_ROOM_MODE"], DISABLED_MODE);

        let canonical_data = fs::canonicalize(&paths.data).expect("canonical data path");
        assert_eq!(
            PathBuf::from(&values[LOCAL_TOKEN_KEY]),
            canonical_data.join(LOCAL_TOKEN_FILE)
        );
        for &(key, filename) in LOCAL_DATABASES {
            let configured = PathBuf::from(&values[key]);
            assert!(configured.is_absolute());
            assert_eq!(configured, canonical_data.join(filename));
        }
        assert!(doctor(&paths).expect("doctor").local_ready);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = fs::metadata(&paths.config)
                .expect("config metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        remove_temporary_home(&home);
    }

    /// Preserves every byte of a complete existing local configuration across repeated initialization.
    #[test]
    fn quick_init_is_idempotent() {
        let home = temporary_home();
        let paths = CliPaths::from_home(&home);
        let runner = CliRunner::local(paths.clone());
        let first = runner
            .run(Command::Init(InitMode::Quick))
            .expect("first initialization");
        let mut preserved = fs::read(&paths.config).expect("read generated configuration");
        preserved.extend_from_slice(b"OPERATOR_NOTE=preserve-me\n");
        fs::write(&paths.config, &preserved).expect("extend configuration");
        #[cfg(unix)]
        set_unix_mode(&paths.config, 0o644).expect("make permissions repairable");
        let before = fs::read(&paths.config).expect("read first configuration");
        let second = runner
            .run(Command::Init(InitMode::Quick))
            .expect("second initialization");
        let after = fs::read(&paths.config).expect("read second configuration");
        assert!(matches!(first, RunResult::Initialized(_)));
        assert!(matches!(second, RunResult::Initialized(_)));
        assert_eq!(before, after);
        assert!(paths.data.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                fs::metadata(&paths.config)
                    .expect("config metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        remove_temporary_home(&home);
    }

    /// Preserves a legacy local file but refuses to call it ready when room mode is absent.
    #[test]
    fn quick_init_rejects_incomplete_existing_configuration_without_overwrite() {
        let home = temporary_home();
        let paths = CliPaths::from_home(&home);
        initialize_quick(&paths, &Harness::Synapse).expect("initial quick configuration");
        let legacy = fs::read_to_string(&paths.config)
            .expect("read generated configuration")
            .lines()
            .filter(|line| *line != "HENOSIS_ROOM_MODE=disabled")
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        fs::write(&paths.config, &legacy).expect("write legacy configuration");

        let error = initialize_quick(&paths, &Harness::Synapse)
            .expect_err("legacy configuration must not claim ready");

        assert!(matches!(
            error,
            CliError::QuickConfigurationIncomplete { .. }
        ));
        assert_eq!(
            fs::read_to_string(&paths.config).expect("read preserved legacy configuration"),
            legacy
        );
        remove_temporary_home(&home);
    }

    /// Loads literal values without expansion and leaves existing process values untouched.
    #[test]
    fn local_environment_loader_is_literal_and_non_overwriting() {
        let home = temporary_home();
        let paths = CliPaths::from_home(&home);
        let literal_key = unique_environment_key("LITERAL");
        let protected_key = unique_environment_key("PROTECTED");
        let literal_value = "$HOME;$(touch never-run)=${NOT_EXPANDED}";
        write_private_test_config(
            &paths,
            &format!("{literal_key}={literal_value}\n{protected_key}=from-file\n"),
        );
        env::set_var(&protected_key, "from-process");

        load_local_environment(&paths).expect("load strict local environment");

        assert_eq!(
            env::var(&literal_key).expect("literal value loaded"),
            literal_value
        );
        assert_eq!(
            env::var(&protected_key).expect("protected value retained"),
            "from-process"
        );
        env::remove_var(&literal_key);
        env::remove_var(&protected_key);
        remove_temporary_home(&home);
    }

    /// Treats a truly absent local configuration as an environment-only deployment.
    #[test]
    fn optional_environment_loader_accepts_absent_config() {
        let home = temporary_home();
        let paths = CliPaths::from_home(&home);
        assert!(
            !load_local_environment_if_present(&paths).expect("missing config must be optional")
        );
        assert!(!home.exists());
    }

    /// Rejects invalid and duplicate keys before setting any process environment variable.
    #[test]
    fn local_environment_loader_rejects_invalid_or_duplicate_keys_atomically() {
        let home = temporary_home();
        let paths = CliPaths::from_home(&home);
        let key = unique_environment_key("ATOMIC");
        write_private_test_config(
            &paths,
            &format!("{key}=first\n9INVALID=value\n{key}=second\n"),
        );
        let error = load_local_environment(&paths).expect_err("invalid key must be rejected");
        assert!(matches!(
            error,
            CliError::InvalidConfiguration { line: 2, .. }
        ));
        assert!(env::var_os(&key).is_none());

        write_private_test_config(&paths, &format!("{key}=first\n{key}=second\n"));
        let error = load_local_environment(&paths).expect_err("duplicate key must be rejected");
        assert!(matches!(
            error,
            CliError::DuplicateConfigurationKey { line: 2, .. }
        ));
        assert!(env::var_os(&key).is_none());
        remove_temporary_home(&home);
    }

    /// Rejects an oversized private configuration before parsing or mutating the environment.
    #[test]
    fn local_environment_loader_enforces_fixed_size_limit() {
        let home = temporary_home();
        let paths = CliPaths::from_home(&home);
        write_private_test_config(&paths, &"A".repeat(MAX_LOCAL_CONFIG_BYTES + 1));
        assert!(matches!(
            load_local_environment(&paths),
            Err(CliError::LocalFileTooLarge {
                max_bytes: MAX_LOCAL_CONFIG_BYTES,
                ..
            })
        ));
        remove_temporary_home(&home);
    }

    /// Refuses symbolic links and non-regular configuration paths.
    #[cfg(unix)]
    #[test]
    fn local_environment_loader_refuses_unsafe_path_types() {
        use std::os::unix::fs::symlink;

        let home = temporary_home();
        let paths = CliPaths::from_home(&home);
        fs::create_dir_all(&paths.home).expect("create test home");
        let target = paths.home.join("target.env");
        fs::write(&target, "SAFE=value\n").expect("write target");
        set_unix_mode(&target, 0o600).expect("set target mode");
        symlink(&target, &paths.config).expect("create config symlink");
        assert!(matches!(
            load_local_environment(&paths),
            Err(CliError::UnexpectedPathType { .. })
        ));
        fs::remove_file(&paths.config).expect("remove test symlink");
        fs::create_dir(&paths.config).expect("create config directory");
        assert!(matches!(
            load_local_environment(&paths),
            Err(CliError::UnexpectedPathType { .. })
        ));
        remove_temporary_home(&home);
    }

    /// Refuses a Windows file reparse point before reading its target as configuration.
    #[cfg(windows)]
    #[test]
    fn local_environment_loader_refuses_windows_symbolic_link() {
        use std::os::windows::fs::symlink_file;

        let home = temporary_home();
        let paths = CliPaths::from_home(&home);
        fs::create_dir_all(&paths.home).expect("create test home");
        let target = paths.home.join("target.env");
        fs::write(&target, "SAFE=value\n").expect("write target");
        symlink_file(&target, &paths.config).expect("create config symlink");

        load_local_environment(&paths).expect_err("Windows config symlink must be rejected");

        remove_temporary_home(&home);
    }

    /// Refuses a Unix FIFO through a nonblocking configuration open.
    #[cfg(unix)]
    #[test]
    fn local_environment_loader_refuses_fifo_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let home = temporary_home();
        let paths = CliPaths::from_home(&home);
        fs::create_dir_all(&paths.home).expect("create test home");
        let encoded =
            CString::new(paths.config.as_os_str().as_bytes()).expect("FIFO path has no NUL");
        // SAFETY: `encoded` is a live NUL-terminated path and the mode is valid.
        let result = unsafe { libc::mkfifo(encoded.as_ptr(), 0o600) };
        assert_eq!(result, 0, "create FIFO: {}", io::Error::last_os_error());
        assert!(matches!(
            load_local_environment(&paths),
            Err(CliError::UnexpectedPathType { .. })
        ));

        remove_temporary_home(&home);
    }

    /// Refuses a regular Unix configuration file with group or other access bits.
    #[cfg(unix)]
    #[test]
    fn local_environment_loader_requires_owner_only_mode() {
        let home = temporary_home();
        let paths = CliPaths::from_home(&home);
        write_private_test_config(&paths, "SAFE=value\n");
        set_unix_mode(&paths.config, 0o640).expect("set insecure test mode");
        assert!(matches!(
            load_local_environment(&paths),
            Err(CliError::InsecureConfigPermissions { mode: 0o640, .. })
        ));
        remove_temporary_home(&home);
    }

    /// Refuses production initialization until every external authority key is configured.
    #[test]
    fn production_init_refuses_missing_authority_configuration() {
        let home = temporary_home();
        let paths = CliPaths::from_home(&home);
        let runner = CliRunner::local(paths.clone());
        runner
            .run(Command::Init(InitMode::Quick))
            .expect("quick initialization");
        let error = runner
            .run(Command::Init(InitMode::Production))
            .expect_err("production init must reject incomplete authority configuration");
        let CliError::ProductionConfigurationIncomplete { missing } = error else {
            panic!("unexpected production initialization error");
        };
        assert!(missing.iter().any(|key| key == "SYNTHEOS_PLUTUS_DB"));
        assert!(missing.iter().any(|key| key == "PHYLAXD_URL"));
        assert!(missing.iter().any(|key| key == "HENOSIS_WITNESS_URL"));
        assert!(missing.iter().any(|key| key == "HENOSIS_RIFT_JWT_SECRET"));
        assert!(missing
            .iter()
            .any(|key| key.starts_with("HENOSIS_ROOM_MODE")));
        assert!(missing
            .iter()
            .any(|key| key.starts_with("SYNTHEOS_LOCAL_POLICY")));
        assert!(!missing
            .iter()
            .any(|key| key == "SYNTHEOS_OPERATOR_JWT_SECRET"));
        remove_temporary_home(&home);
    }

    /// Accepts production initialization when every required authority value is non-empty.
    #[test]
    fn production_init_accepts_complete_authority_configuration() {
        let home = temporary_home();
        let paths = CliPaths::from_home(&home);
        initialize_quick(&paths, &Harness::Synapse).expect("quick initialization");
        let generated = fs::read_to_string(&paths.config).expect("read generated configuration");
        let mut production = generated
            .lines()
            .filter(|line| {
                *line != "SYNTHEOS_LOCAL_POLICY=1" && *line != "HENOSIS_ROOM_MODE=disabled"
            })
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        production.push_str("HENOSIS_ROOM_MODE=required\n");
        for key in PRODUCTION_REQUIRED_KEYS {
            if !production
                .lines()
                .any(|line| line.starts_with(&format!("{key}=")))
            {
                production.push_str(&format!("{key}=configured\n"));
            }
        }
        fs::write(&paths.config, production).expect("write production configuration");

        let result = initialize_production(&paths).expect("production initialization");

        assert_eq!(result.mode, InitMode::Production);
        remove_temporary_home(&home);
    }

    /// Refuses to call local configuration ready when its room mode is absent or invalid.
    #[test]
    fn doctor_requires_explicit_disabled_room_mode_for_local_readiness() {
        let home = temporary_home();
        let paths = CliPaths::from_home(&home);
        initialize_quick(&paths, &Harness::Synapse).expect("quick initialization");
        let generated = fs::read_to_string(&paths.config).expect("read generated configuration");

        let without_room_mode = generated
            .lines()
            .filter(|line| *line != "HENOSIS_ROOM_MODE=disabled")
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        fs::write(&paths.config, without_room_mode).expect("remove room mode");
        assert!(
            !doctor(&paths)
                .expect("doctor without room mode")
                .local_ready
        );

        fs::write(
            &paths.config,
            generated.replace("HENOSIS_ROOM_MODE=disabled", "HENOSIS_ROOM_MODE=invalid"),
        )
        .expect("write invalid room mode");
        let report = doctor(&paths).expect("doctor with invalid room mode");
        assert!(!report.local_ready);
        assert!(report
            .missing_production_keys
            .iter()
            .any(|key| key.starts_with("HENOSIS_ROOM_MODE")));
        remove_temporary_home(&home);
    }

    /// Refuses a production request before creating a missing operator home directory.
    #[test]
    fn production_init_never_creates_missing_local_state() {
        let home = temporary_home();
        let paths = CliPaths::from_home(&home);
        let runner = CliRunner::local(paths);
        let error = runner
            .run(Command::Init(InitMode::Production))
            .expect_err("production init must require existing state");
        assert!(matches!(
            error,
            CliError::ProductionConfigurationIncomplete { .. }
        ));
        assert!(!home.exists());
    }

    /// Emits diagnostic JSON without including the values of production authority configuration.
    #[test]
    fn doctor_json_reports_missing_keys_without_secrets() {
        let home = temporary_home();
        let paths = CliPaths::from_home(&home);
        let runner = CliRunner::local(paths.clone());
        runner
            .run(Command::Init(InitMode::Quick))
            .expect("quick initialization");
        let generated = fs::read_to_string(&paths.config).expect("read generated configuration");
        fs::write(
            &paths.config,
            format!(
                "{generated}PHYLAXD_URL=https://authority.example\nHERMES_PHYLAXD_TOKEN=secret-value\n"
            ),
        )
        .expect("write test configuration");
        let output = runner
            .run(Command::Doctor(DoctorOutputFormat::Json))
            .expect("run doctor")
            .render()
            .expect("render doctor JSON");
        assert!(output.contains("HENOSIS_WITNESS_URL"));
        assert!(!output.contains("secret-value"));
        remove_temporary_home(&home);
    }

    /// Renders stable help and version output without touching the local filesystem.
    #[test]
    fn help_and_version_are_renderable() {
        assert!(RunResult::Help
            .render()
            .expect("render help")
            .contains("init --quick"));
        assert!(RunResult::Version
            .render()
            .expect("render version")
            .starts_with("henosis "));
    }

    /// Refuses a control-plane operation when no authenticated typed client was integrated.
    #[test]
    fn live_command_never_pretends_success_without_a_client() {
        let paths = CliPaths::from_home(temporary_home());
        let runner = CliRunner::local(paths);
        assert!(matches!(
            runner.run(Command::Status),
            Err(CliError::ControlApiUnavailable { .. })
        ));
    }
}
