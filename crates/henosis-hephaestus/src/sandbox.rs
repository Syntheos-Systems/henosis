//! Isolated code execution via Podman containers. Each invocation runs in a
//! network-isolated container with a configurable memory ceiling and timeout.
//! Stdout and stderr are combined and returned to the caller.

use std::time::Duration;

use tokio::process::Command;

/// Execute `code` in an isolated Podman container.
/// Returns combined stdout + stderr on success or an error string on failure.
/// Timeout and memory are taken from config.
pub async fn run_code(
    language: &str,
    code: &str,
    timeout_secs: u64,
    memory: &str,
) -> Result<String, String> {
    let (image, cmd_args): (&str, Vec<&str>) = match language {
        "python" => (
            "docker.io/library/python:3.12-slim",
            vec!["python3", "-c", code],
        ),
        "bash" => ("docker.io/library/alpine:latest", vec!["sh", "-c", code]),
        "javascript" | "js" | "node" => {
            ("docker.io/library/node:22-slim", vec!["node", "-e", code])
        }
        other => return Err(format!("unsupported language: {other}")),
    };

    let mut podman = Command::new("podman");
    podman
        .args([
            "run",
            "--rm",
            "--network",
            "none",
            "--memory",
            memory,
            "--timeout",
            &timeout_secs.to_string(),
            image,
        ])
        .args(&cmd_args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let fut = podman.output();
    let output = match tokio::time::timeout(Duration::from_secs(timeout_secs + 5), fut).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(format!("podman exec error: {e}")),
        Err(_) => return Err(format!("execution timed out after {timeout_secs}s")),
    };

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if !output.status.success() {
        let code_str = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let mut msg = format!("exit code {code_str}");
        if !stdout.is_empty() {
            msg.push_str("\nSTDOUT:\n");
            msg.push_str(&stdout);
        }
        if !stderr.is_empty() {
            msg.push_str("\nSTDERR:\n");
            msg.push_str(&stderr);
        }
        return Err(msg);
    }

    let mut combined = stdout;
    if !stderr.is_empty() {
        if !combined.is_empty() {
            combined.push_str("\nSTDERR:\n");
        }
        combined.push_str(&stderr);
    }
    Ok(combined)
}
