//! Tree-sitter parsing helpers. Accepts a file path, detects the language
//! from its extension, and returns a `ParsedFile` that bundles the syntax
//! tree with the original source text for byte-range lookups.

use std::path::Path;
use tree_sitter::{Parser, Tree};

use super::language_for_extension;

/// A successfully parsed source file: the file path, the tree-sitter `Tree`,
/// and the raw source string (kept so callers can slice by byte range).
pub struct ParsedFile {
    /// Path of the parsed file.
    pub path: String,
    /// The parsed syntax tree.
    pub tree: Tree,
    /// The file's source text (the tree borrows offsets into it).
    pub source: String,
}

/// Parse `path` with the tree-sitter grammar for its file extension. Returns
/// `None` if the extension is unsupported or the file cannot be read.
pub fn parse_file(path: &Path) -> Option<ParsedFile> {
    let ext = path.extension()?.to_str()?;
    let language = language_for_extension(ext)?;

    let source = std::fs::read_to_string(path).ok()?;

    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;

    let tree = parser.parse(&source, None)?;

    Some(ParsedFile {
        path: path.to_string_lossy().to_string(),
        tree,
        source,
    })
}
