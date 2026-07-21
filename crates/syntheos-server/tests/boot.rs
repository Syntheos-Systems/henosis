//! Real-process boot contract tests for the composed Syntheos server.

use std::process::Command;
use std::time::{Duration, Instant};

/// Construct the production binary command with isolated, write-free local service stores.
fn isolated_server() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_syntheos-server"));
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
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
