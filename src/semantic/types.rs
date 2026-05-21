use std::fmt;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    Function,
    Class,
    Method,
    Interface,
    TypeAlias,
    Variable,
}

impl fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SymbolKind::Function => write!(f, "function"),
            SymbolKind::Class => write!(f, "class"),
            SymbolKind::Method => write!(f, "method"),
            SymbolKind::Interface => write!(f, "interface"),
            SymbolKind::TypeAlias => write!(f, "type"),
            SymbolKind::Variable => write!(f, "variable"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ByteRange {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub end_line: usize,
}

#[cfg(feature = "semantic")]
impl From<tree_sitter::Node<'_>> for ByteRange {
    fn from(n: tree_sitter::Node) -> Self {
        ByteRange {
            start_byte: n.start_byte(),
            end_byte: n.end_byte(),
            start_line: n.start_position().row + 1,
            end_line: n.end_position().row + 1,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Symbol {
    pub kind: SymbolKind,
    pub name: String,
    pub range: ByteRange,
    pub signature: String,
    pub is_exported: bool,
    pub parent_class: Option<String>,
}

/// What kind of import path this entry carries. Different
/// languages express imports in incompatible textual forms
/// (`stdio.h`, `clojure.string`, `std::sync::Arc`, …); the
/// `kind` lets cross-language queries (e.g., "what files import
/// this module?") normalize without re-parsing the string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(dead_code)]
pub enum ImportKind {
    /// C / C++ preprocessor `#include` — local or system header.
    Header,
    /// Single-token module names: Go's `"fmt"`, Python's
    /// `os.path`, Ruby's `'json'`, Clojure's `clojure.string`.
    /// Hierarchy is segment-based (dot or `/`).
    Module,
    /// Fully-qualified names with explicit scoping syntax:
    /// Java's `java.util.List`, Rust's `std::sync::Arc`,
    /// TypeScript's relative-path-style imports.
    Qualified,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Import {
    pub names: Vec<String>,
    pub source: String,
    pub kind: ImportKind,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ExtractedFile {
    pub file_path: PathBuf,
    pub symbols: Vec<Symbol>,
    pub imports: Vec<Import>,
    /// Names exported from this file. Derived from the symbol
    /// list: every symbol with `is_exported=true` contributes its
    /// `name`. Pre-computed at extract time so consumers don't
    /// have to re-iterate the symbol vec.
    pub exports: Vec<String>,
    pub warnings: Vec<String>,
    pub mtime: std::time::SystemTime,
}
