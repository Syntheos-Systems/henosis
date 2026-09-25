//! Child-process environment and direct-executor lifecycle confinement.

use std::ffi::OsStr;
use std::process::{Command as StdCommand, ExitStatus, Output};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};

/// Maximum bytes accepted from each direct executor stdout or stderr stream.
const DIRECT_EXECUTOR_OUTPUT_LIMIT_BYTES: usize = 256 * 1024;

/// Ambient variables a host-session CLI needs without receiving credentials.
const HOST_SESSION_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "COLORTERM",
    "TMPDIR",
    "TZ",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    "CODEX_HOME",
    "CLAUDE_CONFIG_DIR",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NO_COLOR",
];

/// Prevent same-UID Linux children from inspecting this process through `/proc` or ptrace.
#[cfg(target_os = "linux")]
pub(crate) fn protect_authority_process() -> std::io::Result<()> {
    // SAFETY: PR_SET_DUMPABLE accepts an integer flag and ignores the remaining
    // arguments. Passing zero removes dump/ptrace access without dereferencing
    // any pointer or transferring ownership.
    let result = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Return whether an environment variable may carry credentials or authority.
pub(crate) fn is_sensitive_environment_name(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return true;
    };
    let name = name.to_ascii_uppercase();
    name == "DATABASE_URL"
        || name == "REDIS_URL"
        || name == "MONGODB_URI"
        || name == "AMQP_URL"
        || name == "PGPASSWORD"
        || name == "PGPASSFILE"
        || name == "PGSERVICE"
        || name == "PGSERVICEFILE"
        || name == "MYSQL_PWD"
        || name == "SSH_AUTH_SOCK"
        || name == "SSH_AGENT_PID"
        || name == "SSH_ASKPASS"
        || name == "GPG_AGENT_INFO"
        || name == "KUBECONFIG"
        || name == "DOCKER_CONFIG"
        || name == "CONTAINER_HOST"
        || name == "REGISTRY_AUTH_FILE"
        || name == "NETRC"
        || name == "GIT_ASKPASS"
        || name == "GIT_SSH"
        || name == "GIT_SSH_COMMAND"
        || name == "GIT_CONFIG_PARAMETERS"
        || name == "SUDO_ASKPASS"
        || name == "CREDENTIALS_DIRECTORY"
        || name == "DBUS_SESSION_BUS_ADDRESS"
        || name == "DISPLAY"
        || name == "WAYLAND_DISPLAY"
        || name == "XAUTHORITY"
        || name == "NOTIFY_SOCKET"
        || name == "LISTEN_FDS"
        || name == "LISTEN_PID"
        || name == "LISTEN_FDNAMES"
        || name == "BASH_ENV"
        || name == "ENV"
        || name == "GOOGLE_APPLICATION_CREDENTIALS"
        || name == "AWS_SHARED_CREDENTIALS_FILE"
        || name == "AWS_CONFIG_FILE"
        || name.contains("SECRET")
        || name.contains("TOKEN")
        || name.contains("PASSWORD")
        || name.contains("PASSPHRASE")
        || name.contains("CREDENTIAL")
        || name.contains("AUTHORIZATION")
        || name.contains("BEARER")
        || name.contains("JWT")
        || name.contains("COOKIE")
        || name.contains("ACCESS_KEY")
        || name.contains("PRIVATE_KEY")
        || name.ends_with("_KEY")
        || name.ends_with("_DATABASE_URL")
        || name.starts_with("HENOSIS_")
        || name.starts_with("SYNTHEOS_")
        || name.starts_with("RIFT_")
        || name.starts_with("KLEOS_")
        || name.starts_with("PHYLAX")
        || name.starts_with("CREDD_")
        || name.starts_with("HERMES_")
        || name.starts_with("OPENAI_")
        || name.starts_with("ANTHROPIC_")
        || name.starts_with("AWS_")
        || name.starts_with("AZURE_")
        || name.starts_with("GITHUB_")
        || name.starts_with("GH_")
        || name.starts_with("GIT_CONFIG_")
        || name.starts_with("DOCKER_")
        || name.starts_with("LD_")
        || name.starts_with("DYLD_")
        || name.starts_with("VAULT_")
        || name.starts_with("POSTGRES_")
        || name.starts_with("CLOUDFLARE_")
        || name.starts_with("STRIPE_")
}

/// Remove sensitive ambient variables from an existing standard-library command.
pub(crate) fn scrub_sensitive_environment(command: &mut StdCommand) {
    for (name, _) in std::env::vars_os() {
        if is_sensitive_environment_name(&name) {
            command.env_remove(name);
        }
    }
    // These high-value names also override values added directly to a command
    // when they are absent from the parent process environment.
    for name in [
        "DATABASE_URL",
        "HENOSIS_RIFT_DATABASE_URL",
        "HENOSIS_RIFT_JWT_SECRET",
        "HENOSIS_RIFT_AGENT_JWT_SECRET",
        "HENOSIS_RIFT_BRIDGE_SECRET",
        "SYNTHEOS_RIFT_DATABASE_URL",
        "SYNTHEOS_RIFT_JWT_SECRET",
        "SYNTHEOS_RIFT_AGENT_JWT_SECRET",
        "SYNTHEOS_RIFT_BRIDGE_SECRET",
        "JWT_SECRET",
        "AGENT_JWT_SECRET",
        "RIFT_BRIDGE_SECRET",
        "SYNTHEOS_OPERATOR_JWT_SECRET",
        "SYNTHEOS_OPERATOR_BOOTSTRAP_PASSWORD",
        "SYNTHEOS_OPERATOR_PASSWORD",
        "SYNTHEOS_STRIPE_WEBHOOK_SECRET",
        "HENOSIS_API_TOKEN",
        "SYNTHEOS_API_TOKEN",
        "KLEOS_API_KEY",
        "HERMES_PHYLAXD_TOKEN",
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "SSH_AUTH_SOCK",
        "GPG_AGENT_INFO",
    ] {
        command.env_remove(name);
    }
}

/// Construct a Tokio child command with inherited authority removed.
///
/// Dropping the spawned child kills its direct process. Arbitrary descendants
/// still require an OS process group or cgroup, while remote effects require
/// provider-side cancellation or idempotency.
pub(crate) fn secure_tokio_command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command.kill_on_drop(true);
    scrub_sensitive_environment(command.as_std_mut());
    command
}

/// Construct a standard-library child command with inherited authority removed.
pub(crate) fn secure_std_command(program: impl AsRef<OsStr>) -> StdCommand {
    let mut command = StdCommand::new(program);
    scrub_sensitive_environment(&mut command);
    command
}

/// Construct a host-session CLI command from a small non-secret allowlist.
pub(crate) fn host_session_tokio_command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command.env_clear();
    for name in HOST_SESSION_ENV_ALLOWLIST {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}

/// Keeps the direct executor's cooperative process group armed until cleanup.
///
/// A child can escape this lifecycle boundary by changing its process group or
/// creating remote effects. Namespace, cgroup, and UID isolation are outside
/// this guard's contract.
#[must_use = "the guard must live until every direct-executor wait is finished"]
pub(crate) struct DirectExecutorProcessGroupGuard {
    /// Active Linux process-group identifier reserved for the direct executor child.
    #[cfg(target_os = "linux")]
    process_group_id: Option<libc::pid_t>,
    /// Test observer that records whether cleanup runs while the leader is unreaped.
    #[cfg(all(test, target_os = "linux"))]
    cleanup_observed_zombie_leader: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

/// Provides explicit direct-executor scope cleanup before the guard is dropped.
impl DirectExecutorProcessGroupGuard {
    /// Return whether error cleanup still needs a direct-child kill fallback.
    pub(crate) fn requires_direct_child_kill(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.process_group_id.is_some()
        }
        #[cfg(not(target_os = "linux"))]
        {
            true
        }
    }

    /// Kill the Linux process group once, then discard its numeric identifier.
    ///
    /// Calling this after observing leader exit but before reaping it kills any
    /// lingering in-group descendants while the zombie still reserves the PGID.
    /// A failed signal leaves the identifier armed so `Drop` can retry once.
    pub(crate) fn cleanup(&mut self) -> std::io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            let Some(process_group_id) = self.process_group_id else {
                return Ok(());
            };
            #[cfg(test)]
            if let Some(observation) = &self.cleanup_observed_zombie_leader {
                observation.store(
                    linux_process_is_zombie(process_group_id),
                    std::sync::atomic::Ordering::SeqCst,
                );
            }
            kill_linux_process_group(process_group_id)?;
            self.process_group_id = None;
        }
        Ok(())
    }
}

/// Return whether one test-owned Linux process is currently an unreaped zombie.
#[cfg(all(test, target_os = "linux"))]
fn linux_process_is_zombie(process_id: libc::pid_t) -> bool {
    std::fs::read_to_string(format!("/proc/{process_id}/status"))
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find(|line| line.starts_with("State:"))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|state| state.chars().next())
        })
        == Some('Z')
}

/// Signal every current member of one Linux direct-executor process group.
///
/// An already-empty group is a successful cleanup. Other errors remain
/// distinguishable internally, while the guard's infallible drop path deliberately
/// ignores them because destructors cannot safely report or recover from failure.
#[cfg(target_os = "linux")]
fn kill_linux_process_group(process_group_id: libc::pid_t) -> std::io::Result<()> {
    loop {
        // SAFETY: a negative PID directs kill(2) to the process group whose positive
        // identifier was captured from this freshly spawned child. SIGKILL carries
        // no borrowed memory or ownership across the FFI boundary.
        let result = unsafe { libc::kill(-process_group_id, libc::SIGKILL) };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::ESRCH) => return Ok(()),
            _ => return Err(error),
        }
    }
}

/// Spawns one direct executor under the platform's strongest supported policy.
///
/// On Linux, the direct leader enters a new process group, requests `SIGKILL`
/// when its spawning authority dies, and sets irreversible `no_new_privs` before
/// exec. The parent-death signal is not inherited by descendants, and process
/// group cleanup does not contain a child that deliberately escapes the group.
/// On other targets, Tokio's direct-child kill-on-drop behavior is the explicitly
/// weaker compatibility guarantee. Internal Git, credential, and helper commands
/// do not call this direct-executor constructor.
pub(crate) fn spawn_direct_executor(
    command: &mut Command,
) -> std::io::Result<(Child, DirectExecutorProcessGroupGuard)> {
    command.kill_on_drop(true);

    #[cfg(target_os = "linux")]
    {
        // SAFETY: getpid(2) has no pointer arguments or ownership effects and
        // returns the process identity that the forked child must still observe.
        let authority_process_id = unsafe { libc::getpid() };
        command.process_group(0);
        // SAFETY: Tokio runs this closure after fork and immediately before exec.
        // It performs only async-signal-safe prctl/getppid syscalls, constructs
        // inline OS errors, and touches no shared state or borrowed pointer.
        unsafe {
            command.pre_exec(move || {
                let parent_death_result =
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0);
                if parent_death_result != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != authority_process_id {
                    return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
                }
                let no_new_privileges_result = libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
                (no_new_privileges_result == 0)
                    .then_some(())
                    .ok_or_else(std::io::Error::last_os_error)
            });
        }
    }

    let child = command.spawn()?;

    #[cfg(target_os = "linux")]
    let process_group_id = child
        .id()
        .and_then(|process_id| libc::pid_t::try_from(process_id).ok())
        .ok_or_else(|| std::io::Error::other("spawned direct executor has no valid process id"))?;

    let guard = DirectExecutorProcessGroupGuard {
        #[cfg(target_os = "linux")]
        process_group_id: Some(process_group_id),
        #[cfg(all(test, target_os = "linux"))]
        cleanup_observed_zombie_leader: None,
    };
    Ok((child, guard))
}

/// Observe one Linux child exit without reaping its process-group-leading zombie.
#[cfg(target_os = "linux")]
async fn observe_linux_direct_executor_exit(process_id: libc::pid_t) -> std::io::Result<()> {
    let wait_identifier = libc::id_t::try_from(process_id)
        .map_err(|_| std::io::Error::other("direct executor process id is not waitable"))?;
    tokio::task::spawn_blocking(move || loop {
        let mut signal_information = std::mem::MaybeUninit::<libc::siginfo_t>::uninit();
        // SAFETY: wait_identifier names a positive child process captured at spawn.
        // waitid writes to valid local siginfo storage, and this code does not read
        // that storage. WNOWAIT leaves the exited child waitable for Tokio to reap.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                wait_identifier,
                signal_information.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(error);
        }
    })
    .await
    .map_err(|error| {
        std::io::Error::other(format!(
            "joining direct executor exit observer for process {process_id}: {error}"
        ))
    })?
}

/// Wait for a direct leader without freeing its Linux PGID before group cleanup.
///
/// Linux first observes exit with `waitid(WNOWAIT)`, cleans and disarms the
/// process group while the zombie reserves its numeric identifier, and only then
/// asks Tokio to reap the leader. Other targets retain the explicitly weaker
/// direct-child wait and kill-on-drop guarantee.
pub(crate) async fn wait_for_direct_executor_status(
    child: &mut Child,
    process_group_guard: &mut DirectExecutorProcessGroupGuard,
) -> std::io::Result<ExitStatus> {
    #[cfg(target_os = "linux")]
    {
        let process_id = child
            .id()
            .and_then(|process_id| libc::pid_t::try_from(process_id).ok())
            .ok_or_else(|| {
                std::io::Error::other("direct executor leader has no valid process id")
            })?;
        let observation_result = observe_linux_direct_executor_exit(process_id).await;
        let requires_direct_child_kill = process_group_guard.requires_direct_child_kill();
        let cleanup_result = process_group_guard.cleanup();
        if let Err(observation_error) = observation_result {
            if cleanup_result.is_ok() {
                if requires_direct_child_kill {
                    let _ = child.start_kill();
                }
                let _ = child.wait().await;
            }
            return Err(observation_error);
        }
        cleanup_result?;
        return child.wait().await;
    }

    #[cfg(not(target_os = "linux"))]
    {
        let status_result = child.wait().await;
        let cleanup_result = process_group_guard.cleanup();
        let status = status_result?;
        cleanup_result?;
        Ok(status)
    }
}

/// Read one direct-executor stream until EOF or one byte beyond its fixed ceiling.
async fn read_direct_executor_output<R>(
    reader: R,
    stream_name: &'static str,
) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut limited_reader = reader.take((DIRECT_EXECUTOR_OUTPUT_LIMIT_BYTES + 1) as u64);
    let mut output = Vec::with_capacity(DIRECT_EXECUTOR_OUTPUT_LIMIT_BYTES + 1);
    limited_reader.read_to_end(&mut output).await?;
    if output.len() > DIRECT_EXECUTOR_OUTPUT_LIMIT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "direct executor {stream_name} exceeded the {DIRECT_EXECUTOR_OUTPUT_LIMIT_BYTES}-byte limit"
            ),
        ));
    }
    Ok(output)
}

/// Wait for a direct leader while draining output and clean its group immediately on exit.
///
/// Cleanup lives inside the leader-wait future rather than after the combined
/// output wait. A descendant that inherits stdout or stderr therefore cannot
/// hold its pipe open and postpone group termination after the leader exits.
/// Each stream is capped at 256 KiB; exceeding either cap fails the execution
/// and terminates the group instead of draining an attacker-controlled writer.
pub(crate) async fn wait_for_direct_executor_output(
    mut child: Child,
    mut process_group_guard: DirectExecutorProcessGroupGuard,
) -> std::io::Result<Output> {
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let read_stdout = async {
        if let Some(pipe) = stdout_pipe.as_mut() {
            read_direct_executor_output(pipe, "stdout").await
        } else {
            Ok(Vec::new())
        }
    };
    let read_stderr = async {
        if let Some(pipe) = stderr_pipe.as_mut() {
            read_direct_executor_output(pipe, "stderr").await
        } else {
            Ok(Vec::new())
        }
    };
    let wait_for_exit = wait_for_direct_executor_status(&mut child, &mut process_group_guard);

    let result = tokio::try_join!(wait_for_exit, read_stdout, read_stderr);
    let (status, stdout, stderr) = match result {
        Ok(output) => output,
        Err(error) => {
            let requires_direct_child_kill = process_group_guard.requires_direct_child_kill();
            let cleanup_result = process_group_guard.cleanup();
            // The group cleanup closes every in-group writer on Linux. This direct
            // leader fallback gives other targets deterministic fail-closed behavior.
            if requires_direct_child_kill {
                let _ = child.start_kill();
            }
            if let Err(cleanup_error) = cleanup_result {
                // Retry the armed group guard while the child still reserves its PID,
                // then let Tokio's kill-on-drop fallback own the unreaped child.
                drop(process_group_guard);
                drop(child);
                return Err(cleanup_error);
            }
            let _ = child.wait().await;
            return Err(error);
        }
    };
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Cleans up direct-executor process scope on every completion or cancellation path.
impl Drop for DirectExecutorProcessGroupGuard {
    /// Signals the Linux group without ever panicking from a destructor.
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

#[cfg(test)]
/// Proves authority classification, environment removal, and child lifecycle cleanup.
mod tests {
    use super::*;

    /// Return one Linux process state from procfs, if the process still exists.
    #[cfg(target_os = "linux")]
    fn linux_process_state(process_id: u32) -> Option<char> {
        let status = std::fs::read_to_string(format!("/proc/{process_id}/status")).ok()?;
        status
            .lines()
            .find(|line| line.starts_with("State:"))?
            .split_whitespace()
            .nth(1)?
            .chars()
            .next()
    }

    /// Return whether a Linux process has exited or remains only as a non-running zombie.
    #[cfg(target_os = "linux")]
    fn linux_process_is_inactive(process_id: u32) -> bool {
        matches!(linux_process_state(process_id), None | Some('X' | 'Z'))
    }

    /// Wait for Linux processes to stop executing within the cleanup deadline.
    #[cfg(target_os = "linux")]
    async fn wait_for_linux_processes_to_be_inactive(process_ids: &[u32]) -> bool {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if process_ids
                    .iter()
                    .all(|process_id| linux_process_is_inactive(*process_id))
                {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .is_ok()
    }

    /// Wait for a child with the requested exec name or process state to appear.
    #[cfg(target_os = "linux")]
    async fn wait_for_linux_child_process(
        parent_process_id: u32,
        expected_name: Option<&str>,
        expected_state: Option<char>,
    ) -> Option<u32> {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let tasks_path = format!("/proc/{parent_process_id}/task");
                if let Ok(tasks) = std::fs::read_dir(tasks_path) {
                    for task in tasks.flatten() {
                        let children = std::fs::read_to_string(task.path().join("children"))
                            .unwrap_or_default();
                        for process_id in children
                            .split_whitespace()
                            .filter_map(|value| value.parse::<u32>().ok())
                        {
                            let name_matches = expected_name.is_none_or(|expected| {
                                std::fs::read_to_string(format!("/proc/{process_id}/comm"))
                                    .is_ok_and(|name| name.trim() == expected)
                            });
                            let state_matches = expected_state.is_none_or(|expected| {
                                linux_process_state(process_id) == Some(expected)
                            });
                            if name_matches && state_matches {
                                return process_id;
                            }
                        }
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .ok()
    }

    /// Kill one test-owned Linux process so a failed probe cannot leak a sleeper.
    #[cfg(target_os = "linux")]
    fn emergency_kill_linux_process(process_id: u32) {
        // SAFETY: containment tests call this only for a freshly discovered child
        // of their private helper process and send the non-catchable SIGKILL.
        unsafe {
            libc::kill(process_id as libc::pid_t, libc::SIGKILL);
        }
    }

    /// Wait for Linux process IDs to vanish from procfs within the cleanup deadline.
    #[cfg(target_os = "linux")]
    async fn wait_for_linux_processes_to_exit(process_ids: &[u32]) -> bool {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if process_ids.iter().all(|process_id| {
                    !std::path::Path::new(&format!("/proc/{process_id}")).exists()
                }) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .is_ok()
    }

    /// Authority, broker, provider, and human SSH variables are classified as sensitive.
    #[test]
    fn sensitive_environment_classifier_covers_security_roots() {
        for name in [
            "HENOSIS_RIFT_JWT_SECRET",
            "SYNTHEOS_OPERATOR_JWT_SECRET",
            "KLEOS_API_KEY",
            "OPENAI_API_KEY",
            "SSH_AUTH_SOCK",
            "CUSTOM_SIGNING_KEY",
            "PGPASSWORD",
            "GIT_SSH_COMMAND",
            "LD_PRELOAD",
            "AWS_SESSION_TOKEN",
            "DATABASE_URL",
        ] {
            assert!(is_sensitive_environment_name(OsStr::new(name)), "{name}");
        }
        assert!(!is_sensitive_environment_name(OsStr::new("PATH")));
        assert!(!is_sensitive_environment_name(OsStr::new("HOME")));
    }

    /// Host-session commands receive only explicitly reviewed non-secret variables.
    #[test]
    fn host_session_environment_is_an_allowlist() {
        let command = host_session_tokio_command("env");
        for (name, value) in command.as_std().get_envs() {
            if value.is_some() {
                let name = name.to_string_lossy();
                assert!(
                    HOST_SESSION_ENV_ALLOWLIST.contains(&name.as_ref()),
                    "unexpected host-session environment: {name}"
                );
                assert!(
                    !is_sensitive_environment_name(OsStr::new(name.as_ref())),
                    "sensitive host-session environment: {name}"
                );
            }
        }
    }

    /// A non-dumpable authority process denies a same-UID child access to its environment.
    #[cfg(target_os = "linux")]
    #[test]
    fn nondumpable_parent_blocks_proc_environment_access() {
        let mode = std::env::var_os("SYNTHEOS_PROCESS_SECURITY_TEST_MODE");
        if mode.as_deref() == Some(OsStr::new("reader")) {
            let parent_pid = std::env::var("SYNTHEOS_PROCESS_SECURITY_PARENT_PID")
                .expect("reader receives the authority parent PID");
            let error = std::fs::read(format!("/proc/{parent_pid}/environ"))
                .expect_err("same-UID reader must not read a non-dumpable parent environment");
            assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
            return;
        }

        if mode.as_deref() == Some(OsStr::new("authority")) {
            assert_eq!(
                std::env::var("SYNTHEOS_PROCESS_SECURITY_MARKER").as_deref(),
                Ok("authority-root-marker")
            );
            protect_authority_process().expect("authority process becomes non-dumpable");
            let executable = std::env::current_exe().expect("current test executable");
            let output = StdCommand::new(executable)
                .env_clear()
                .env("SYNTHEOS_PROCESS_SECURITY_TEST_MODE", "reader")
                .env(
                    "SYNTHEOS_PROCESS_SECURITY_PARENT_PID",
                    std::process::id().to_string(),
                )
                .arg("--exact")
                .arg("process_security::tests::nondumpable_parent_blocks_proc_environment_access")
                .arg("--nocapture")
                .output()
                .expect("spawn same-UID environment reader");
            assert!(
                output.status.success(),
                "reader probe failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let executable = std::env::current_exe().expect("current test executable");
        let output = StdCommand::new(executable)
            .env_clear()
            .env("SYNTHEOS_PROCESS_SECURITY_TEST_MODE", "authority")
            .env("SYNTHEOS_PROCESS_SECURITY_MARKER", "authority-root-marker")
            .arg("--exact")
            .arg("process_security::tests::nondumpable_parent_blocks_proc_environment_access")
            .arg("--nocapture")
            .output()
            .expect("spawn isolated authority process");
        assert!(
            output.status.success(),
            "authority probe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Explicitly configured authority is removed while inert environment remains.
    #[test]
    fn scrub_removes_explicit_authority_values() {
        let mut command = StdCommand::new("env");
        command.env("HENOSIS_RIFT_JWT_SECRET", "human-root");
        command.env("SSH_AUTH_SOCK", "/tmp/human-agent.sock");
        command.env("SAFE_MARKER", "retained");

        scrub_sensitive_environment(&mut command);

        let configured = command
            .get_envs()
            .map(|(name, value)| (name.to_string_lossy().into_owned(), value.is_some()))
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(configured.get("HENOSIS_RIFT_JWT_SECRET"), Some(&false));
        assert_eq!(configured.get("SSH_AUTH_SOCK"), Some(&false));
        assert_eq!(configured.get("SAFE_MARKER"), Some(&true));
    }

    /// Dropping a secure Tokio child terminates and reaps its direct Linux process.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn secure_tokio_command_kills_direct_child_on_drop() {
        let child = secure_tokio_command("/bin/sleep")
            .arg("60")
            .spawn()
            .expect("spawn disposable direct child");
        let process_id = child.id().expect("spawned child has a process id");
        let process_path = std::path::PathBuf::from(format!("/proc/{process_id}"));
        assert!(process_path.exists());

        drop(child);

        let exited = tokio::time::timeout(std::time::Duration::from_millis(250), async {
            while process_path.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .is_ok();
        if !exited {
            // SAFETY: the test owns this freshly spawned PID, sends only SIGKILL,
            // and does not dereference memory or transfer ownership.
            unsafe {
                libc::kill(process_id as libc::pid_t, libc::SIGKILL);
            }
        }
        assert!(exited, "dropping the secure child must terminate it");
    }

    /// Normal completion cleans the Linux group while its leader still reserves the PGID.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn direct_executor_cleanup_precedes_leader_reaping() {
        let cleanup_observed_zombie_leader =
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut command = Command::new("/bin/true");
        let (child, mut process_group_guard) =
            spawn_direct_executor(&mut command).expect("spawn cleanup-order probe");
        process_group_guard.cleanup_observed_zombie_leader =
            Some(std::sync::Arc::clone(&cleanup_observed_zombie_leader));

        let output = wait_for_direct_executor_output(child, process_group_guard)
            .await
            .expect("wait for cleanup-order probe");

        assert!(output.status.success());
        assert!(
            cleanup_observed_zombie_leader.load(std::sync::atomic::Ordering::SeqCst),
            "group cleanup must run before Child::wait reaps the leader"
        );
    }

    /// Oversized Linux direct output fails promptly and leaves no writer process behind.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn direct_executor_output_limit_kills_process_group() {
        for stream_name in ["stdout", "stderr"] {
            let mut command = Command::new("/bin/sh");
            let script = if stream_name == "stdout" {
                "sleep 60 & exec yes x"
            } else {
                "sleep 60 & exec yes x >&2"
            };
            command
                .arg("-c")
                .arg(script)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());

            let (child, process_group_guard) =
                spawn_direct_executor(&mut command).expect("spawn oversized-output probe");
            let leader_process_id = child.id().expect("output probe leader has a process id");
            let descendant_process_id =
                wait_for_linux_child_process(leader_process_id, Some("sleep"), Some('S')).await;
            let Some(descendant_process_id) = descendant_process_id else {
                drop((child, process_group_guard));
                panic!("oversized-output probe did not spawn its sleeper");
            };

            let result = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                wait_for_direct_executor_output(child, process_group_guard),
            )
            .await;
            let error = match result {
                Ok(Err(error)) => Some(error),
                Ok(Ok(_)) | Err(_) => None,
            };
            let process_ids = [leader_process_id, descendant_process_id];
            let exited = wait_for_linux_processes_to_exit(&process_ids).await;
            if !exited {
                for process_id in process_ids {
                    emergency_kill_linux_process(process_id);
                }
                let _ = wait_for_linux_processes_to_exit(&process_ids).await;
            }

            assert!(exited, "oversized output must not leak its process group");
            let error = error.expect("oversized output must fail before the timeout");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            assert!(error.to_string().contains(stream_name));
            assert!(
                error
                    .to_string()
                    .contains(&DIRECT_EXECUTOR_OUTPUT_LIMIT_BYTES.to_string()),
                "the error reports the enforced byte ceiling"
            );
        }
    }

    /// A Linux direct executor observes the irreversible `no_new_privs` bit.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn direct_executor_child_observes_no_new_privs() {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(
                "while IFS=: read -r key value; do if [ \"$key\" = NoNewPrivs ]; then echo $value; exit 0; fi; done < /proc/self/status",
            )
            .stdout(std::process::Stdio::piped());

        let (child, process_group_guard) =
            spawn_direct_executor(&mut command).expect("spawn no-new-privileges probe");
        let output = wait_for_direct_executor_output(child, process_group_guard)
            .await
            .expect("wait for no-new-privileges probe");

        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "1");
    }

    /// A Linux direct executor dies if its authority process is abruptly killed.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn direct_executor_dies_when_authority_parent_is_killed() {
        let mode = std::env::var_os("SYNTHEOS_PROCESS_SECURITY_TEST_MODE");
        if mode.as_deref() == Some(OsStr::new("pdeath-authority")) {
            let mut command = Command::new("/bin/sleep");
            command.arg("60");
            let (child, process_group_guard) =
                spawn_direct_executor(&mut command).expect("spawn parent-death probe");
            std::future::pending::<()>().await;
            drop((child, process_group_guard));
            return;
        }

        let executable = std::env::current_exe().expect("current test executable");
        let mut authority_command = Command::new(executable);
        authority_command
            .env_clear()
            .env("SYNTHEOS_PROCESS_SECURITY_TEST_MODE", "pdeath-authority")
            .arg("--exact")
            .arg("process_security::tests::direct_executor_dies_when_authority_parent_is_killed")
            .arg("--nocapture")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut authority = authority_command
            .spawn()
            .expect("spawn isolated authority helper");
        let authority_process_id = authority.id().expect("authority helper has a process id");
        let child_process_id =
            wait_for_linux_child_process(authority_process_id, Some("sleep"), Some('S')).await;
        let Some(child_process_id) = child_process_id else {
            let _ = authority.start_kill();
            let _ = authority.wait().await;
            panic!("authority helper did not exec its direct child");
        };

        authority
            .start_kill()
            .expect("abruptly kill the authority helper");
        authority.wait().await.expect("reap authority helper");
        let inactive = wait_for_linux_processes_to_be_inactive(&[child_process_id]).await;
        if !inactive {
            emergency_kill_linux_process(child_process_id);
            let _ = wait_for_linux_processes_to_be_inactive(&[child_process_id]).await;
        }
        assert!(
            inactive,
            "authority death must stop the direct executor child"
        );
    }

    /// A child orphaned before its parent-death signal is armed fails before exec.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn direct_executor_parent_death_setup_closes_the_fork_race() {
        let mode = std::env::var_os("SYNTHEOS_PROCESS_SECURITY_TEST_MODE");
        if mode.as_deref() == Some(OsStr::new("pdeath-race-authority")) {
            // Match the production authority before forking. Rust may abort when
            // reporting the rejected spawn to its dead parent; core-dump handling
            // must not delay this test's observation of the orphan's exit.
            protect_authority_process().expect("race authority becomes non-dumpable");
            let mut command = Command::new("/bin/sleep");
            command.arg("60");
            // SAFETY: this test-only hook uses async-signal-safe signal operations
            // to stop before the production hook arms the parent-death signal.
            unsafe {
                command.pre_exec(|| {
                    if libc::signal(libc::SIGHUP, libc::SIG_IGN) == libc::SIG_ERR {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::raise(libc::SIGSTOP) == 0 {
                        Ok(())
                    } else {
                        Err(std::io::Error::last_os_error())
                    }
                });
            }
            let _ = spawn_direct_executor(&mut command);
            return;
        }

        let executable = std::env::current_exe().expect("current test executable");
        let mut authority_command = Command::new(executable);
        authority_command
            .env_clear()
            .env(
                "SYNTHEOS_PROCESS_SECURITY_TEST_MODE",
                "pdeath-race-authority",
            )
            .arg("--exact")
            .arg("process_security::tests::direct_executor_parent_death_setup_closes_the_fork_race")
            .arg("--nocapture")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut authority = authority_command
            .spawn()
            .expect("spawn isolated authority race helper");
        let authority_process_id = authority.id().expect("authority helper has a process id");
        let child_process_id =
            wait_for_linux_child_process(authority_process_id, None, Some('T')).await;
        let Some(child_process_id) = child_process_id else {
            let _ = authority.start_kill();
            let _ = authority.wait().await;
            panic!("direct child did not stop before its production pre-exec hook");
        };

        authority
            .start_kill()
            .expect("kill authority before parent-death setup");
        authority.wait().await.expect("reap authority race helper");
        // SAFETY: the test owns this stopped child and SIGCONT only resumes its
        // production pre-exec hook so the parent identity check can run.
        unsafe {
            libc::kill(child_process_id as libc::pid_t, libc::SIGCONT);
        }
        let inactive = wait_for_linux_processes_to_be_inactive(&[child_process_id]).await;
        if !inactive {
            emergency_kill_linux_process(child_process_id);
            let _ = wait_for_linux_processes_to_be_inactive(&[child_process_id]).await;
        }
        assert!(
            inactive,
            "a child orphaned before parent-death setup must not exec"
        );
    }

    /// Explicit cleanup kills a descendant left behind by a completed group leader.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn direct_executor_cleanup_kills_lingering_descendant() {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(
                "sleep 60 </dev/null >/dev/null 2>&1 & descendant=$!; printf '%s\\n' \"$descendant\"",
            )
            .stdout(std::process::Stdio::piped());

        let (child, mut process_group_guard) =
            spawn_direct_executor(&mut command).expect("spawn cleanup probe");
        let output = child
            .wait_with_output()
            .await
            .expect("wait for completed group leader");
        assert!(output.status.success());
        let descendant_process_id = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u32>()
            .expect("numeric descendant process identifier");
        assert!(
            !linux_process_is_inactive(descendant_process_id),
            "descendant remains active until explicit cleanup"
        );

        process_group_guard
            .cleanup()
            .expect("explicitly clean completed process group");
        let inactive = wait_for_linux_processes_to_be_inactive(&[descendant_process_id]).await;
        if !inactive {
            emergency_kill_linux_process(descendant_process_id);
            let _ = wait_for_linux_processes_to_be_inactive(&[descendant_process_id]).await;
        }
        assert!(inactive, "explicit cleanup must stop lingering descendants");
    }

    /// Leader completion cleans descendants before inherited output pipes can stall.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn direct_executor_output_wait_cleans_inherited_descendant_pipes() {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("sleep 60 & descendant=$!; printf '%s\\n' \"$descendant\"")
            .stdout(std::process::Stdio::piped());

        let (child, process_group_guard) =
            spawn_direct_executor(&mut command).expect("spawn inherited-pipe probe");
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            wait_for_direct_executor_output(child, process_group_guard),
        )
        .await
        .expect("leader completion closes inherited descendant pipes")
        .expect("wait for inherited-pipe probe");
        assert!(output.status.success());
        let descendant_process_id = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u32>()
            .expect("numeric inherited-pipe descendant process identifier");
        assert!(
            wait_for_linux_processes_to_be_inactive(&[descendant_process_id]).await,
            "leader completion must stop the descendant that inherited stdout"
        );
    }

    /// Dropping a Linux direct-executor scope removes both its leader and descendant.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn direct_executor_scope_drop_kills_process_group() {
        use tokio::io::AsyncBufReadExt;

        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("sleep 60 & descendant=$!; printf '%s %s\\n' $$ \"$descendant\"; wait")
            .stdout(std::process::Stdio::piped());

        let (mut child, process_group_guard) =
            spawn_direct_executor(&mut command).expect("spawn process-group probe");
        let stdout = child.stdout.take().expect("capture process identifiers");
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        let process_line =
            tokio::time::timeout(std::time::Duration::from_secs(2), lines.next_line())
                .await
                .expect("process identifiers arrive before timeout")
                .expect("read process identifiers")
                .expect("process probe emits one line");
        let process_ids = process_line
            .split_whitespace()
            .map(|value| value.parse::<u32>().expect("numeric process identifier"))
            .collect::<Vec<_>>();
        assert_eq!(
            process_ids.len(),
            2,
            "leader and descendant IDs are reported"
        );

        drop(lines);
        drop(child);
        drop(process_group_guard);

        let exited = wait_for_linux_processes_to_exit(&process_ids).await;
        if !exited {
            if let Some(descendant_id) = process_ids.get(1) {
                // SAFETY: the test owns this freshly spawned 60-second sleeper and
                // sends only SIGKILL to prevent a failed containment probe from leaking.
                unsafe {
                    libc::kill(*descendant_id as libc::pid_t, libc::SIGKILL);
                }
            }
            let _ = wait_for_linux_processes_to_exit(&process_ids).await;
        }
        assert!(
            exited,
            "scope drop must remove the leader and its descendant"
        );
    }
}
