//! Mechanical comment-coverage scanner. Walks a source file, finds every
//! declaration (fn/struct/enum/trait/impl/mod/type/class/method), and reports
//! which ones lack a leading comment. Used by the pre-commit ratchet and by
//! `challenge_code` to surface concrete documentation gaps.

use crate::db::Database;
use crate::json_io::Output;
use crate::tools::{ToolError, ToolResult};
use serde::Deserialize;
use std::path::Path;

/// Input payload for the `comment_check` tool: path of the file to scan.
#[derive(Deserialize)]
pub struct CommentCheckInput {
    /// File whose comment coverage is checked.
    pub file_path: Option<String>,
}

/// One undocumented declaration: source line, item kind, and the literal text.
#[derive(serde::Serialize)]
struct Finding {
    line: usize,
    item: String,
    declaration: String,
}

/// Scan a file for declarations missing a leading comment and return a report.
pub fn comment_check(_db: &Database, input: CommentCheckInput) -> ToolResult {
    let file_path = input
        .file_path
        .ok_or_else(|| ToolError::MissingField("file_path".into()))?;

    let content = std::fs::read_to_string(&file_path)
        .map_err(|e| ToolError::IoError(format!("Cannot read {}: {}", file_path, e)))?;

    let ext = Path::new(&file_path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    let raw_lines: Vec<&str> = content.lines().collect();
    let (declarations, findings) = match ext {
        "rs" => scan_rust(&raw_lines),
        "ts" | "tsx" | "js" | "jsx" | "go" | "java" | "kt" | "swift" | "c" | "cpp" | "h"
        | "hpp" => scan_c_family(&raw_lines),
        "py" => scan_python(&raw_lines),
        _ => (0, Vec::new()),
    };

    let total = declarations;
    let missing = findings.len();
    let documented = total.saturating_sub(missing);
    let coverage = if total == 0 {
        1.0
    } else {
        documented as f64 / total as f64
    };

    let summary = if missing == 0 {
        format!("All {} declarations in {} are commented", total, file_path)
    } else {
        format!(
            "{}/{} declarations in {} lack a leading comment",
            missing, total, file_path
        )
    };

    let mut output = if missing == 0 {
        Output::ok(summary)
    } else {
        Output::error(summary)
    };
    output.data = Some(serde_json::json!({
        "file": file_path,
        "language": ext,
        "declarations": total,
        "documented": documented,
        "missing": missing,
        "coverage": coverage,
        "undocumented": findings,
        "rule": "Every declaration (fn, struct, enum, trait, impl, mod, type, class, method) must be preceded by a comment describing what the code does. Module/file headers required for non-trivial files.",
    }));
    Ok(output)
}

/// Scan Rust source for declarations without a preceding `//`/`///` comment.
/// Returns (total_declarations, undocumented_findings).
fn scan_rust(lines: &[&str]) -> (usize, Vec<Finding>) {
    let mut total = 0usize;
    let mut findings = Vec::new();

    for (idx, raw) in lines.iter().enumerate() {
        let trimmed = raw.trim_start();
        let item = match rust_decl_kind(trimmed) {
            Some(k) => k,
            None => continue,
        };
        total += 1;
        if !preceded_by_rust_comment(lines, idx) {
            findings.push(Finding {
                line: idx + 1,
                item: item.into(),
                declaration: trimmed.trim_end().chars().take(120).collect(),
            });
        }
    }

    (total, findings)
}

/// Classify a Rust line: which top-level declaration kind, if any, does it open?
fn rust_decl_kind(line: &str) -> Option<&'static str> {
    let stripped = line
        .trim_start_matches("pub ")
        .trim_start_matches("pub(crate) ")
        .trim_start();
    let stripped = stripped
        .trim_start_matches("async ")
        .trim_start_matches("unsafe ");
    if stripped.starts_with("fn ") {
        return Some("fn");
    }
    if stripped.starts_with("struct ") {
        return Some("struct");
    }
    if stripped.starts_with("enum ") {
        return Some("enum");
    }
    if stripped.starts_with("trait ") {
        return Some("trait");
    }
    if stripped.starts_with("type ") {
        return Some("type");
    }
    // mod foo { ... } counts; mod foo; (re-export) does not
    if let Some(rest) = stripped.strip_prefix("mod ") {
        if rest.contains('{') {
            return Some("mod");
        }
    }
    // impl blocks at column zero
    if stripped.starts_with("impl ") || stripped.starts_with("impl<") {
        return Some("impl");
    }
    None
}

/// Walk backwards from `idx`, skipping blanks and `#[...]` attributes, and
/// return true if the first real line is a `//` or `///` comment.
fn preceded_by_rust_comment(lines: &[&str], idx: usize) -> bool {
    let mut i = idx;
    while i > 0 {
        i -= 1;
        let t = lines[i].trim();
        if t.is_empty() {
            continue;
        }
        // Skip Rust attributes (#[...] or #![...]) -- they sit between docs and decl
        if t.starts_with("#[") || t.starts_with("#![") {
            continue;
        }
        return t.starts_with("//");
    }
    false
}

/// Scan C-family languages (TS/JS/Go/Java/Kt/Swift/C/C++) for declarations
/// without a preceding `//` or `/* */` comment.
fn scan_c_family(lines: &[&str]) -> (usize, Vec<Finding>) {
    let mut total = 0usize;
    let mut findings = Vec::new();

    for (idx, raw) in lines.iter().enumerate() {
        let trimmed = raw.trim_start();
        let item = match c_family_decl_kind(trimmed) {
            Some(k) => k,
            None => continue,
        };
        total += 1;
        if !preceded_by_c_comment(lines, idx) {
            findings.push(Finding {
                line: idx + 1,
                item: item.into(),
                declaration: trimmed.trim_end().chars().take(120).collect(),
            });
        }
    }

    (total, findings)
}

/// Classify a C-family line: which declaration keyword (if any) opens it?
fn c_family_decl_kind(line: &str) -> Option<&'static str> {
    // TS/JS
    for prefix in [
        "export function ",
        "export async function ",
        "export default function ",
        "function ",
        "async function ",
        "export class ",
        "class ",
        "export interface ",
        "interface ",
        "export type ",
        "type ",
        "export enum ",
        "enum ",
    ] {
        if line.starts_with(prefix) {
            return Some(prefix.trim_end());
        }
    }
    // Go
    if line.starts_with("func ") {
        return Some("func");
    }
    // Java/Kotlin/Swift loose match
    if line.starts_with("public class ") || line.starts_with("private class ") {
        return Some("class");
    }
    if line.starts_with("public fun ") || line.starts_with("fun ") {
        return Some("fun");
    }
    None
}

/// Walk backwards from `idx`, skipping blanks and `@decorator` lines, and
/// return true if the first real line is a `//`, `/*`, or block-continuation comment.
fn preceded_by_c_comment(lines: &[&str], idx: usize) -> bool {
    let mut i = idx;
    while i > 0 {
        i -= 1;
        let t = lines[i].trim();
        if t.is_empty() {
            continue;
        }
        // Skip decorators / annotations
        if t.starts_with('@') {
            continue;
        }
        return t.starts_with("//")
            || t.ends_with("*/")
            || t.starts_with("/*")
            || t.starts_with("*");
    }
    false
}

/// Scan Python for `def`/`class` lacking a docstring or preceding `#` comment.
fn scan_python(lines: &[&str]) -> (usize, Vec<Finding>) {
    let mut total = 0usize;
    let mut findings = Vec::new();

    for (idx, raw) in lines.iter().enumerate() {
        let trimmed = raw.trim_start();
        let item = if trimmed.starts_with("def ") || trimmed.starts_with("async def ") {
            "def"
        } else if trimmed.starts_with("class ") {
            "class"
        } else {
            continue;
        };
        total += 1;

        let has_preceding_comment = {
            let mut i = idx;
            let mut found = false;
            while i > 0 {
                i -= 1;
                let t = lines[i].trim();
                if t.is_empty() {
                    continue;
                }
                if t.starts_with('@') {
                    continue;
                }
                found = t.starts_with('#');
                break;
            }
            found
        };

        let has_docstring = {
            let mut j = idx + 1;
            let mut found = false;
            while j < lines.len() {
                let t = lines[j].trim();
                if t.is_empty() {
                    j += 1;
                    continue;
                }
                found = t.starts_with("\"\"\"") || t.starts_with("'''");
                break;
            }
            found
        };

        if !has_preceding_comment && !has_docstring {
            findings.push(Finding {
                line: idx + 1,
                item: item.into(),
                declaration: trimmed.trim_end().chars().take(120).collect(),
            });
        }
    }

    (total, findings)
}
