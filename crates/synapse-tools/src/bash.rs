//! Bash/shell command execution tool.

use crate::tool::{AgentTool, ToolResult};
use anyhow::Result;
use serde_json::Value;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::time::timeout;

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Upper bound on the model-supplied timeout.
///
/// `timeout_ms` arrives from model-generated tool arguments. Without a ceiling a
/// single call can request `u64::MAX` milliseconds and the documented bound stops
/// meaning anything, leaving the session blocked on a command that never returns.
const MAX_TIMEOUT_MS: u64 = 600_000;
const MAX_OUTPUT_BYTES: usize = 50 * 1024; // 50 KB

/// One bounded child stream and whether bytes were discarded after the cap.
struct BoundedOutput {
    /// Retained prefix of the stream.
    bytes: Vec<u8>,
    /// Whether the stream contained more bytes than were retained.
    truncated: bool,
}

/// Captured child status and bounded pipe output.
struct ProcessOutput {
    /// Process exit status.
    status: ExitStatus,
    /// Bounded stdout prefix.
    stdout: BoundedOutput,
    /// Bounded stderr prefix.
    stderr: BoundedOutput,
}

/// Drain one pipe to EOF while retaining at most `cap` bytes.
async fn read_capped<R>(mut reader: R, cap: usize) -> std::io::Result<BoundedOutput>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::with_capacity(cap);
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Ok(BoundedOutput { bytes, truncated });
        }
        let remaining = cap.saturating_sub(bytes.len());
        let retained = read.min(remaining);
        bytes.extend_from_slice(&chunk[..retained]);
        truncated |= retained < read;
    }
}

/// Executes bounded shell commands and returns combined process output.
pub struct BashTool;

/// Implements the agent tool contract for shell command execution.
#[async_trait::async_trait]
impl AgentTool for BashTool {
    /// Returns the shell tool's stable registry name.
    fn name(&self) -> &str {
        "bash"
    }

    /// Describes the shell command execution capability.
    fn description(&self) -> &str {
        "Execute a shell command and return combined stdout+stderr. \
         Use for running builds, tests, CLI tools, or any system command."
    }

    /// Returns the accepted command and timeout parameters.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute."
                },
                "timeout_ms": {
                    "type": "number",
                    "description": "Timeout in milliseconds. Defaults to 30000, capped at 600000."
                }
            },
            "required": ["command"]
        })
    }

    /// Executes a command from the supplied working directory within the timeout.
    async fn execute(&self, params: Value, cwd: &Path) -> Result<ToolResult> {
        let command = match params.get("command").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => {
                return Ok(ToolResult {
                    content: "Missing required parameter: command".to_string(),
                    is_error: true,
                });
            }
        };

        let timeout_ms = params
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .filter(|requested| *requested > 0)
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS);

        let mut cmd = build_command(&command);
        cmd.current_dir(cwd);
        if let Err(error) = restrict_agent_environment(&mut cmd) {
            return Ok(ToolResult {
                content: format!("Agent environment configuration failed: {error}"),
                is_error: true,
            });
        }
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                return Ok(ToolResult {
                    content: format!("Failed to spawn command: {error}"),
                    is_error: true,
                });
            }
        };
        let stdout = child.stdout.take().expect("piped stdout must be available");
        let stderr = child.stderr.take().expect("piped stderr must be available");
        let collect = async {
            let (stdout, stderr, status) = tokio::try_join!(
                read_capped(stdout, MAX_OUTPUT_BYTES),
                read_capped(stderr, MAX_OUTPUT_BYTES),
                child.wait(),
            )?;
            Ok::<_, anyhow::Error>(ProcessOutput {
                status,
                stdout,
                stderr,
            })
        };
        let result = timeout(Duration::from_millis(timeout_ms), collect).await;

        match result {
            Err(_elapsed) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                Ok(ToolResult {
                    content: format!("Command timed out after {timeout_ms}ms"),
                    is_error: true,
                })
            }
            Ok(Err(e)) => Ok(ToolResult {
                content: format!("Failed to collect command output: {e}"),
                is_error: true,
            }),
            Ok(Ok(output)) => {
                let mut combined = output.stdout.bytes;
                combined.extend_from_slice(&output.stderr.bytes);

                let truncated = output.stdout.truncated
                    || output.stderr.truncated
                    || combined.len() > MAX_OUTPUT_BYTES;
                if truncated {
                    combined.truncate(MAX_OUTPUT_BYTES);
                }

                let mut text = String::from_utf8_lossy(&combined).into_owned();
                if truncated {
                    text.push_str("\n[truncated]");
                }

                let exit_code = output.status.code().unwrap_or(-1);
                let is_error = !output.status.success();

                if is_error && text.is_empty() {
                    text = format!("Process exited with code {exit_code}");
                }

                Ok(ToolResult {
                    content: text,
                    is_error,
                })
            }
        }
    }
}

/// Environment variables an agent-controlled command may inherit on Unix.
///
/// Deliberately excludes anything that could carry authority. Toolchain roots
/// are included because builds and tests are the tool's primary use and those
/// values are paths, not credentials.
#[cfg(not(target_os = "windows"))]
const INHERITABLE_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "TZ",
    "TMPDIR",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "GOPATH",
    "GOCACHE",
    "GOMODCACHE",
    "JAVA_HOME",
    "PYENV_ROOT",
    "NVM_DIR",
];

/// Environment variables an agent-controlled command may inherit on Windows.
///
/// `SystemRoot`, `ComSpec`, and `PATHEXT` are required for `cmd.exe` itself to
/// start, so an empty environment is not an option on this platform.
#[cfg(target_os = "windows")]
const INHERITABLE_ENV: &[&str] = &[
    "PATH",
    "PATHEXT",
    "ComSpec",
    "SystemRoot",
    "SystemDrive",
    "windir",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "ProgramData",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "OS",
    "CARGO_HOME",
    "RUSTUP_HOME",
];

/// Operator-controlled escape hatch naming extra variables to pass through.
///
/// Comma-separated. Exists so a deployment that genuinely needs a specific
/// variable (a proxy setting, a private registry host) can opt it back in
/// without patching the binary. Its own name is never forwarded.
/// `SYNTHEOS_AGENT_ENV_PASSTHROUGH` is canonical; the legacy brand alias is
/// honored read-only by the rename boundary.
const ENV_PASSTHROUGH_VAR: &str = "SYNTHEOS_AGENT_ENV_PASSTHROUGH";

/// Legacy brand alias of [`ENV_PASSTHROUGH_VAR`], never forwarded either.
const ENV_PASSTHROUGH_LEGACY_VAR: &str = "HENOSIS_AGENT_ENV_PASSTHROUGH";

/// Suffix shared by the canonical and legacy passthrough variable names.
const ENV_PASSTHROUGH_SUFFIX: &str = "AGENT_ENV_PASSTHROUGH";

/// Process-wide result of the single agent-environment configuration load.
static AGENT_ENVIRONMENT_PASSTHROUGH: OnceLock<
    std::result::Result<Vec<String>, syntheos_env::EnvError>,
> = OnceLock::new();

/// Parse one resolved comma-separated passthrough list into safe variable names.
fn parse_environment_passthrough(value: Option<String>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|name| {
            !name.is_empty() && *name != ENV_PASSTHROUGH_VAR && *name != ENV_PASSTHROUGH_LEGACY_VAR
        })
        .map(str::to_string)
        .collect()
}

/// Resolve the production passthrough setting once and emit its deprecation
/// report at that configuration-load boundary rather than for every command.
fn configured_environment_passthrough(
) -> &'static std::result::Result<Vec<String>, syntheos_env::EnvError> {
    AGENT_ENVIRONMENT_PASSTHROUGH.get_or_init(|| {
        let resolved =
            syntheos_env::resolve(ENV_PASSTHROUGH_SUFFIX).map(parse_environment_passthrough);
        syntheos_env::emit_deprecations();
        resolved
    })
}

/// Resolve an injectable passthrough setting for deterministic policy tests.
#[cfg(test)]
fn resolve_environment_passthrough_with<F>(
    mut lookup: F,
) -> std::result::Result<Vec<String>, syntheos_env::EnvError>
where
    F: FnMut(&str) -> Option<String>,
{
    syntheos_env::resolve_with(ENV_PASSTHROUGH_SUFFIX, |name| Ok(lookup(name)))
        .map(parse_environment_passthrough)
}

/// Replaces the child environment with an allowlist before the agent runs.
///
/// The shell command is model-generated and therefore untrusted, while this
/// process may hold provider keys, memory-service keys, bridge secrets, and
/// database URLs. Inheriting the full environment would let any prompt injection
/// exfiltrate all of them with a single `env`. A denylist cannot be made
/// complete -- every new secret would have to be remembered here -- so the child
/// starts empty and receives only variables known not to carry authority.
fn restrict_agent_environment(
    command: &mut Command,
) -> std::result::Result<(), &'static syntheos_env::EnvError> {
    let extra = configured_environment_passthrough().as_ref()?;
    apply_agent_environment(command, extra, |name| std::env::var(name).ok());
    Ok(())
}

/// Applies the allowlist against an injectable lookup.
///
/// Split out so tests can exercise the policy against a fixture instead of
/// mutating the real process environment, which races the parallel test harness.
fn apply_agent_environment<F>(command: &mut Command, extra: &[String], lookup: F)
where
    F: Fn(&str) -> Option<String>,
{
    command.env_clear();
    for name in INHERITABLE_ENV
        .iter()
        .copied()
        .chain(extra.iter().map(String::as_str))
    {
        if let Some(value) = lookup(name) {
            command.env(name, value);
        }
    }
}

#[cfg(target_os = "windows")]
/// Constructs a Windows command-shell process.
fn build_command(command: &str) -> Command {
    let mut cmd = Command::new("cmd");
    cmd.args(["/C", command]);
    cmd
}

#[cfg(not(target_os = "windows"))]
/// Constructs a POSIX shell process.
fn build_command(command: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", command]);
    cmd
}

/// Verifies the agent shell receives an allowlist rather than the full environment.
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::io::AsyncWriteExt;

    /// Names the child would actually receive, given a fixture environment.
    fn child_env_names(
        fixture: &HashMap<String, String>,
    ) -> std::result::Result<Vec<String>, syntheos_env::EnvError> {
        let mut command = build_command("true");
        let extra = resolve_environment_passthrough_with(|name| fixture.get(name).cloned())?;
        apply_agent_environment(&mut command, &extra, |name| fixture.get(name).cloned());
        Ok(command
            .as_std()
            .get_envs()
            .filter(|(_, value)| value.is_some())
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect())
    }

    /// Build a fixture environment from name/value pairs.
    fn fixture(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// No secret in the parent environment reaches a model-generated command,
    /// including secrets no denylist would have known to name.
    #[test]
    fn agent_shell_does_not_inherit_secrets() {
        let secrets = [
            "PIV_PIN",
            "ANTHROPIC_API_KEY",
            "KLEOS_API_KEY",
            "KLEOS_KEY",
            "SYNAPSE_PROXY_KEY",
            "JWT_SECRET",
            "DATABASE_URL",
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
        ];
        let mut pairs: Vec<(&str, &str)> = secrets.iter().map(|n| (*n, "leaked")).collect();
        pairs.push(("PATH", "/usr/bin"));

        let names = child_env_names(&fixture(&pairs)).expect("resolve empty passthrough policy");

        for name in secrets {
            assert!(
                !names.iter().any(|got| got == name),
                "{name} must not reach the agent shell"
            );
        }
    }

    /// PATH still reaches the child, or every build and test command breaks.
    #[test]
    fn agent_shell_keeps_the_toolchain_baseline() {
        let names = child_env_names(&fixture(&[("PATH", "/usr/bin"), ("SECRET", "x")]))
            .expect("resolve baseline passthrough policy");
        assert!(names.iter().any(|got| got == "PATH"));
        assert!(!names.iter().any(|got| got == "SECRET"));
    }

    /// An operator can opt a specific variable back in without patching the binary.
    #[test]
    fn passthrough_opt_in_is_honored() {
        let names = child_env_names(&fixture(&[
            (ENV_PASSTHROUGH_VAR, "HTTPS_PROXY, NEEDED_HOST"),
            ("HTTPS_PROXY", "http://proxy.internal:3128"),
            ("NEEDED_HOST", "registry.internal"),
            ("ANTHROPIC_API_KEY", "leaked"),
        ]))
        .expect("resolve canonical passthrough policy");

        assert!(names.iter().any(|got| got == "HTTPS_PROXY"));
        assert!(names.iter().any(|got| got == "NEEDED_HOST"));
        assert!(!names.iter().any(|got| got == "ANTHROPIC_API_KEY"));
        assert!(
            !names.iter().any(|got| got == ENV_PASSTHROUGH_VAR),
            "the passthrough list itself must not be forwarded"
        );
    }

    /// The legacy brand alias of the passthrough variable is honored read-only.
    #[test]
    fn passthrough_legacy_alias_is_honored() {
        let names = child_env_names(&fixture(&[
            (ENV_PASSTHROUGH_LEGACY_VAR, "NEEDED_HOST"),
            ("NEEDED_HOST", "registry.internal"),
        ]))
        .expect("resolve legacy passthrough policy");
        assert!(names.iter().any(|got| got == "NEEDED_HOST"));
        assert!(
            !names.iter().any(|got| got == ENV_PASSTHROUGH_LEGACY_VAR),
            "the legacy passthrough alias itself must not be forwarded"
        );
    }

    /// A legacy passthrough alias is available to the one load-boundary
    /// deprecation emission used by the production cache initializer.
    #[test]
    fn passthrough_legacy_alias_records_deprecation() {
        let fixture = fixture(&[(ENV_PASSTHROUGH_LEGACY_VAR, "NEEDED_HOST")]);
        resolve_environment_passthrough_with(|name| fixture.get(name).cloned())
            .expect("resolve legacy passthrough policy");
        let emitted = syntheos_env::emit_deprecations();
        assert!(emitted
            .iter()
            .any(|name| name == ENV_PASSTHROUGH_LEGACY_VAR));
    }

    /// Conflicting canonical and legacy passthrough values reject the command
    /// with key names and without inspecting either requested child variable.
    #[test]
    fn passthrough_conflict_fails_closed() {
        let error = child_env_names(&fixture(&[
            (ENV_PASSTHROUGH_VAR, "NEEDED_HOST"),
            (ENV_PASSTHROUGH_LEGACY_VAR, "OTHER_HOST"),
            ("NEEDED_HOST", "registry.internal"),
            ("OTHER_HOST", "other.internal"),
        ]))
        .expect_err("conflicting passthrough settings must reject the command");
        let message = error.to_string();
        assert!(message.contains(ENV_PASSTHROUGH_VAR));
        assert!(message.contains(ENV_PASSTHROUGH_LEGACY_VAR));
        assert!(!message.contains("NEEDED_HOST"));
        assert!(!message.contains("OTHER_HOST"));
    }

    /// A model-supplied timeout cannot exceed the documented ceiling.
    #[test]
    fn model_supplied_timeout_is_clamped() {
        let clamp = |raw: Option<u64>| {
            raw.filter(|requested| *requested > 0)
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .min(MAX_TIMEOUT_MS)
        };
        assert_eq!(clamp(Some(u64::MAX)), MAX_TIMEOUT_MS);
        assert_eq!(clamp(Some(0)), DEFAULT_TIMEOUT_MS);
        assert_eq!(clamp(None), DEFAULT_TIMEOUT_MS);
        assert_eq!(clamp(Some(5_000)), 5_000);
    }

    /// Pipe draining discards excess bytes during execution instead of buffering them all.
    #[tokio::test]
    async fn capped_reader_retains_only_the_configured_prefix() {
        let (mut writer, reader) = tokio::io::duplex(4096);
        let writer_task = tokio::spawn(async move {
            writer
                .write_all(&vec![b'x'; MAX_OUTPUT_BYTES * 2])
                .await
                .expect("write fixture output");
        });

        let output = read_capped(reader, 1024)
            .await
            .expect("drain fixture output");
        writer_task.await.expect("writer task completes");

        assert_eq!(output.bytes.len(), 1024);
        assert!(output.truncated);
    }
}
