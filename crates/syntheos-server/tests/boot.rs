//! Real-process boot contract tests for the composed Syntheos server.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Create a unique temporary home for one real-process boot test.
fn temporary_home() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("henosis-boot-test-{}-{nonce}", std::process::id()))
}

/// Reserve and release one loopback address for the child server.
fn available_loopback_addr() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .expect("reserve loopback listener")
        .local_addr()
        .expect("read reserved listener address")
}

/// Return true only after the child answers its real health endpoint successfully.
fn health_is_ready(addr: SocketAddr) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(200)) else {
        return false;
    };
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("set health read timeout");
    if stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut response = String::new();
    stream.read_to_string(&mut response).is_ok()
        && response.starts_with("HTTP/1.1 200")
        && response.ends_with("ok")
}

/// Stop a verified test child and remove only the unique home created for it.
fn stop_and_remove(mut child: Child, home: &Path) {
    child.kill().expect("stop local test server");
    child.wait().expect("reap local test server");
    std::fs::remove_dir_all(home).expect("remove unique boot test home");
}

/// Construct the production binary command with isolated, write-free local service stores.
fn isolated_server() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_henosis"));
    let isolated_home = temporary_home();
    command
        .arg("serve")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HENOSIS_HOME", isolated_home)
        .env(
            "HERMES_PHYLAXD_TOKEN",
            "boot-test-broker-token-32-bytes-minimum",
        )
        .env("RUST_LOG", "off")
        .env("SYNTHEOS_ADDR", "127.0.0.1:0")
        .env("SYNTHEOS_IDENTITY_DB", ":memory:")
        .env("SYNTHEOS_CHIASM_DB", ":memory:")
        .env("SYNTHEOS_SOMA_DB", ":memory:")
        .env("SYNTHEOS_BROCA_DB", ":memory:")
        .env("SYNTHEOS_LOOM_DB", ":memory:")
        .env("SYNTHEOS_THYMUS_DB", ":memory:");
    command
}

/// Quick initialization produces configuration that reaches the real local health endpoint.
#[test]
fn quick_initialization_boots_local_server() {
    let home = temporary_home();
    let init = Command::new(env!("CARGO_BIN_EXE_henosis"))
        .args(["init", "--quick"])
        .env_clear()
        .env("HOME", "/tmp")
        .env("PATH", "/usr/bin:/bin")
        .env("HENOSIS_HOME", &home)
        .output()
        .expect("run quick initialization");
    assert!(
        init.status.success(),
        "quick initialization failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let config = std::fs::read_to_string(home.join("config.env"))
        .expect("read generated local configuration");
    assert!(config
        .lines()
        .any(|line| line == "HENOSIS_ROOM_MODE=disabled"));

    let addr = available_loopback_addr();
    let mut child = Command::new(env!("CARGO_BIN_EXE_henosis"))
        .arg("serve")
        .env_clear()
        .env("HOME", "/tmp")
        .env("PATH", "/usr/bin:/bin")
        .env("HENOSIS_HOME", &home)
        .env("RUST_LOG", "off")
        .env("SYNTHEOS_ADDR", addr.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start quick-initialized server");
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if health_is_ready(addr) {
            stop_and_remove(child, &home);
            return;
        }
        if let Some(status) = child.try_wait().expect("poll local server") {
            let mut stderr = String::new();
            child
                .stderr
                .take()
                .expect("capture local server stderr")
                .read_to_string(&mut stderr)
                .expect("read local server stderr");
            panic!("quick-initialized server exited with {status}: {stderr}");
        }
        thread::sleep(Duration::from_millis(100));
    }
    stop_and_remove(child, &home);
    panic!("quick-initialized server did not become healthy at {addr}");
}

/// The production binary fails before binding when its required Plutus authority is absent.
#[test]
fn missing_plutus_database_fails_boot() {
    let output = isolated_server()
        .output()
        .expect("syntheos-server binary must launch");

    assert!(
        !output.status.success(),
        "server must reject boot without the Plutus authority"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("SYNTHEOS_PLUTUS_DB is required"),
        "stderr must identify the missing required authority: {stderr}"
    );
}

/// An unreachable required Plutus authority fails within its configured acquisition deadline.
#[test]
fn unreachable_plutus_database_fails_within_configured_timeout() {
    let started = Instant::now();
    let output = isolated_server()
        .env(
            "SYNTHEOS_PLUTUS_DB",
            "postgresql://probe:sentinel-not-a-secret@127.0.0.1:1/plutus",
        )
        .env("SYNTHEOS_PLUTUS_ACQUIRE_TIMEOUT_SECS", "1")
        .output()
        .expect("syntheos-server binary must launch");
    let elapsed = started.elapsed();

    assert!(
        !output.status.success(),
        "server must reject boot when the required Plutus authority is unreachable"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "server exceeded its one-second Plutus acquisition deadline: {elapsed:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("plutus store open failed") && stderr.contains("timed out"),
        "stderr must identify the bounded Plutus open failure: {stderr}"
    );
    assert!(
        !stderr.contains("sentinel-not-a-secret"),
        "stderr must not expose Postgres URL userinfo: {stderr}"
    );
}

/// Invalid acquisition timeout configuration fails before any Postgres connection attempt.
#[test]
fn invalid_plutus_timeout_fails_boot() {
    let output = isolated_server()
        .env(
            "SYNTHEOS_PLUTUS_DB",
            "postgresql://probe:sentinel-not-a-secret@127.0.0.1:1/plutus",
        )
        .env("SYNTHEOS_PLUTUS_ACQUIRE_TIMEOUT_SECS", "0")
        .output()
        .expect("syntheos-server binary must launch");

    assert!(!output.status.success(), "invalid timeout must fail boot");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr
            .contains("SYNTHEOS_PLUTUS_ACQUIRE_TIMEOUT_SECS must be an integer from 1 through 300"),
        "stderr must identify the invalid timeout: {stderr}"
    );
    assert!(
        !stderr.contains("sentinel-not-a-secret"),
        "validation errors must not expose Postgres URL userinfo: {stderr}"
    );
}
