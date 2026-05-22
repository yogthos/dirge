use std::path::Path;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query};

use crate::semantic::adapter::LanguageAdapter;
use crate::semantic::common::{find_node_at_range, node_text};
use crate::semantic::types::{ByteRange, ExtractedFile, Import, ImportKind, Symbol, SymbolKind};

pub struct PythonAdapter;

impl PythonAdapter {
    fn node_text<'a>(&self, n: Node<'a>, s: &'a [u8]) -> &'a str {
        node_text(n, s)
    }
    fn make_range(&self, n: Node) -> ByteRange {
        ByteRange::from(n)
    }

    fn signature_from_node(&self, node: Node, source: &[u8]) -> String {
        let body = node.child_by_field_name("body");
        let end = body.map(|b| b.start_byte()).unwrap_or(node.end_byte());
        let sig_bytes = &source[node.start_byte()..end];
        String::from_utf8_lossy(sig_bytes).trim().to_string()
    }

    fn walk_class_body(
        &self,
        node: Node,
        source: &[u8],
        symbols: &mut Vec<Symbol>,
        class_name: &str,
    ) {
        for i in 0..node.named_child_count() {
            if let Some(child) = node.named_child(i) {
                match child.kind() {
                    "function_definition" => {
                        self.push_method(child, source, symbols, class_name);
                    }
                    "decorated_definition" => {
                        if let Some(inner) = child.child_by_field_name("definition") {
                            if inner.kind() == "function_definition" {
                                let range = self.make_range(child);
                                self.push_method_with_range(
                                    inner, range, source, symbols, class_name,
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn push_method(&self, node: Node, source: &[u8], symbols: &mut Vec<Symbol>, class_name: &str) {
        let range = self.make_range(node);
        self.push_method_with_range(node, range, source, symbols, class_name);
    }

    fn push_method_with_range(
        &self,
        node: Node,
        range: crate::semantic::types::ByteRange,
        source: &[u8],
        symbols: &mut Vec<Symbol>,
        class_name: &str,
    ) {
        if let Some(name_node) = node.child_by_field_name("name") {
            let name = self.node_text(name_node, source).to_string();
            let signature = self.signature_from_node(node, source);
            symbols.push(Symbol {
                kind: SymbolKind::Method,
                name,
                range,
                signature,
                is_exported: false,
                parent_class: Some(class_name.to_string()),
            });
        }
    }

    fn walk_top_level(
        &self,
        node: Node,
        source: &[u8],
        symbols: &mut Vec<Symbol>,
        imports: &mut Vec<Import>,
        _exports: &mut Vec<String>,
    ) {
        for i in 0..node.named_child_count() {
            let Some(child) = node.named_child(i) else {
                continue;
            };
            match child.kind() {
                "import_statement" => {
                    let mut names = Vec::new();
                    let mut module = String::new();
                    for j in 0..child.named_child_count() {
                        if let Some(c) = child.named_child(j) {
                            match c.kind() {
                                "dotted_name" => {
                                    names.push(self.node_text(c, source).to_string());
                                }
                                "aliased_import" => {
                                    if let Some(alias) = c.child_by_field_name("alias") {
                                        names.push(self.node_text(alias, source).to_string());
                                    } else if let Some(n) = c.child_by_field_name("name") {
                                        names.push(self.node_text(n, source).to_string());
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    if names.is_empty() {
                        if let Some(name_node) = child.child_by_field_name("name") {
                            names.push(self.node_text(name_node, source).to_string());
                        }
                    }
                    if module.is_empty() {
                        if let Some(name_node) = child.child_by_field_name("name") {
                            module = self.node_text(name_node, source).to_string();
                        }
                    }
                    imports.push(Import {
                        names,
                        source: module,
                        kind: ImportKind::Module,
                    });
                }
                "import_from_statement" => {
                    let module = child
                        .child_by_field_name("module_name")
                        .map(|n| self.node_text(n, source).to_string())
                        .unwrap_or_default();
                    let mut names = Vec::new();
                    for j in 0..child.named_child_count() {
                        if let Some(c) = child.named_child(j) {
                            if c.kind() == "dotted_name" {
                                names.push(self.node_text(c, source).to_string());
                            } else if c.kind() == "aliased_import" {
                                if let Some(alias) = c.child_by_field_name("alias") {
                                    names.push(self.node_text(alias, source).to_string());
                                } else if let Some(n) = c.child_by_field_name("name") {
                                    names.push(self.node_text(n, source).to_string());
                                }
                            }
                        }
                    }
                    imports.push(Import {
                        names,
                        source: module,
                        kind: ImportKind::Module,
                    });
                }
                "function_definition" | "decorated_definition" => {
                    let func_node = if child.kind() == "decorated_definition" {
                        child.child_by_field_name("definition")
                    } else {
                        Some(child)
                    };
                    if let Some(node) = func_node {
                        if let Some(name_node) = node.child_by_field_name("name") {
                            let name = self.node_text(name_node, source).to_string();
                            // Dunder methods (`__init__`, `__call__`, `__repr__`, …)
                            // are part of Python's public protocol; they look
                            // "private" by the leading-underscore heuristic but
                            // are externally callable. Treat them as exported.
                            // Single-underscore names (`_helper`, `_internal`)
                            // stay non-exported.
                            let is_dunder = name.starts_with("__") && name.ends_with("__");
                            let is_exported = is_dunder || !name.starts_with('_');
                            let range = self.make_range(child);
                            let signature = self.signature_from_node(node, source);
                            symbols.push(Symbol {
                                kind: SymbolKind::Function,
                                name,
                                range,
                                signature,
                                is_exported,
                                parent_class: None,
                            });
                        }
                    }
                }
                "class_definition" => {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        let name = self.node_text(name_node, source).to_string();
                        let is_exported = !name.starts_with('_');
                        let range = self.make_range(child);
                        let signature = self.signature_from_node(child, source);
                        symbols.push(Symbol {
                            kind: SymbolKind::Class,
                            name: name.clone(),
                            range,
                            signature,
                            is_exported,
                            parent_class: None,
                        });
                        if let Some(body) = child.child_by_field_name("body") {
                            self.walk_class_body(body, source, symbols, &name);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

impl LanguageAdapter for PythonAdapter {
    fn extensions(&self) -> &[&str] {
        &[".py"]
    }

    fn extract(&self, file_path: &Path, source: &str) -> Result<ExtractedFile, String> {
        let lang = tree_sitter_python::LANGUAGE.into();
        let mut parser = Parser::new();
        parser
            .set_language(&lang)
            .map_err(|e| format!("Failed to set language: {e}"))?;

        let tree = parser.parse(source, None).ok_or("Failed to parse source")?;

        let root = tree.root_node();
        let source_bytes = source.as_bytes();

        let mut symbols = Vec::new();
        let mut imports = Vec::new();
        let mut exports = Vec::new();
        let mut warnings = Vec::new();

        if root.has_error() {
            warnings.push("tree-sitter reported syntax errors".to_string());
        }

        self.walk_top_level(root, source_bytes, &mut symbols, &mut imports, &mut exports);

        if exports.is_empty() {
            exports.extend(
                symbols
                    .iter()
                    .filter(|s| s.is_exported)
                    .map(|s| s.name.clone()),
            );
        }

        Ok(ExtractedFile {
            file_path: file_path.to_path_buf(),
            symbols,
            imports,
            exports,
            warnings,
            mtime: std::time::SystemTime::now(),
            size: 0,
        })
    }

    fn find_callees_in_range(
        &self,
        source: &str,
        _file_path: &Path,
        range: ByteRange,
    ) -> Result<Vec<String>, String> {
        let lang: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
        let mut parser = Parser::new();
        parser
            .set_language(&lang)
            .map_err(|e| format!("Failed to set language: {e}"))?;

        let tree = parser.parse(source, None).ok_or("Failed to parse source")?;

        let root = tree.root_node();
        let source_bytes = source.as_bytes();

        let target = find_node_at_range(root, range.start_byte, range.end_byte)
            .ok_or("Could not find node at given range")?;

        let query_str = "(call function: (identifier) @callee)";
        let query = Query::new(&lang, query_str).map_err(|e| format!("Query error: {e}"))?;
        let mut cursor = tree_sitter::QueryCursor::new();
        let mut matches = cursor.matches(&query, target, source_bytes);

        let mut callees = Vec::new();
        while let Some(m) = matches.next() {
            for capture in m.captures {
                let name = capture.node.utf8_text(source_bytes).unwrap_or("");
                callees.push(name.to_string());
            }
        }
        callees.sort();
        callees.dedup();
        Ok(callees)
    }
}
