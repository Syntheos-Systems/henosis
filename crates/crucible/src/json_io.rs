//! JSON input/output helpers for the Crucible CLI. Every tool reads a
//! typed input struct from a file on disk and writes a uniform `Output` JSON
//! struct back. This module owns those read/write operations and the error
//! types they produce.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use thiserror::Error;

/// Errors that can occur when reading or writing the JSON payload files.
#[derive(Error, Debug)]
pub enum IoError {
    /// The input file could not be opened or read from disk.
    #[error("Failed to read input file: {0}")]
    ReadError(#[from] std::io::Error),
    /// The file was read but its contents are not valid JSON or do not match
    /// the expected type.
    #[error("Failed to parse JSON: {0}")]
    ParseError(#[from] serde_json::Error),
}

/// The canonical output envelope written to `--output` for every tool call.
/// `success` is the machine-readable pass/fail; `message` is the human summary;
/// `data` carries any structured result payload.
#[derive(Serialize)]
pub struct Output {
    /// Machine-readable pass/fail for the tool call.
    pub success: bool,
    /// Id of the artifact the call created or addressed, when there is one.
    pub id: Option<String>,
    /// Human-readable summary of what happened.
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Structured result payload, when the tool produced one.
    pub data: Option<serde_json::Value>,
}

/// Constructor helpers for the `Output` envelope.
impl Output {
    /// Create a successful output with no associated ID or data payload.
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            success: true,
            id: None,
            message: message.into(),
            data: None,
        }
    }

    /// Create a successful output that includes the newly created resource ID.
    pub fn ok_with_id(id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            success: true,
            id: Some(id.into()),
            message: message.into(),
            data: None,
        }
    }

    /// Create a failed output with `success: false` and the given error message.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            success: false,
            id: None,
            message: message.into(),
            data: None,
        }
    }
}

/// Read and deserialize a JSON input file into the caller's expected type `T`.
pub fn read_input<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, IoError> {
    let content = fs::read_to_string(path)?;
    let parsed = serde_json::from_str(&content)?;
    Ok(parsed)
}

/// Serialize `output` to pretty-printed JSON and write it to `path`.
pub fn write_output(path: &Path, output: &Output) -> Result<(), IoError> {
    let content = serde_json::to_string_pretty(output)?;
    fs::write(path, content)?;
    Ok(())
}
