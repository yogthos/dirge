use std::path::Path;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query};

use crate::semantic::adapter::LanguageAdapter;
use crate::semantic::common::{find_node_at_range, node_text};
use crate::semantic::types::{ByteRange, ExtractedFile, Import, ImportKind, Symbol, SymbolKind};

pub struct TypescriptAdapter;

impl TypescriptAdapter {
    fn language(&self, file_path: &Path) -> tree_sitter::Language {
        let ext = file_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("ts");
        match ext {
            "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
            _ => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        }
    }

    fn signature_from_node(&self, node: Node, source: &[u8]) -> String {
        let body = node.child_by_field_name("body");
        let end = body.map(|b| b.start_byte()).unwrap_or(node.end_byte());
        let sig_bytes = &source[node.start_byte()..end];
        String::from_utf8_lossy(sig_bytes).trim().to_string()
    }

    fn extract_import(&self, node: Node, source: &[u8]) -> Option<Import> {
        let source_node = node.child_by_field_name("source")?;
        let module_path = node_text(source_node, source);
        let module_path = module_path.trim_matches(&['\'', '"'][..]).to_string();

        let mut names = Vec::new();

        if let Some(clause) = node.child_by_field_name("import") {
            if let Some(name_node) = clause.child_by_field_name("name") {
                names.push(node_text(name_node, source).to_string());
            }
            for i in 0..clause.named_child_count() {
                if let Some(child) = clause.named_child(i) {
                    if child.kind() == "named_imports" {
                        for j in 0..child.named_child_count() {
                            if let Some(spec) = child.named_child(j) {
                                if spec.kind() == "import_specifier" {
                                    if let Some(n) = spec.child_by_field_name("name") {
                                        names.push(node_text(n, source).to_string());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        Some(Import {
            names,
            source: module_path,
            kind: ImportKind::Qualified,
        })
    }

    fn extract_exports(&self, node: Node, source: &[u8]) -> Vec<String> {
        let mut exports = Vec::new();
        match node.kind() {
            "export_statement" => {
                if let Some(decl) = node.child_by_field_name("declaration") {
                    if let Some(name) = decl.child_by_field_name("name") {
                        exports.push(node_text(name, source).to_string());
                    }
                }
                if let Some(export_clause) = node.child_by_field_name("export") {
                    for i in 0..export_clause.named_child_count() {
                        if let Some(spec) = export_clause.named_child(i) {
                            if spec.kind() == "export_specifier" {
                                if let Some(n) = spec.child_by_field_name("name") {
                                    exports.push(node_text(n, source).to_string());
                                }
                            }
                        }
                    }
                }
            }
            "function_declaration" | "class_declaration" => {
                if let Some(name) = node.child_by_field_name("name") {
                    exports.push(node_text(name, source).to_string());
                }
            }
            "lexical_declaration" | "variable_declaration" => {
                if let Some(export_node) = node.parent() {
                    if export_node.kind() == "export_statement" {
                        for i in 0..node.named_child_count() {
                            if let Some(decl) = node.named_child(i) {
                                if decl.kind() == "variable_declarator" {
                                    if let Some(name) = decl.child_by_field_name("name") {
                                        exports.push(node_text(name, source).to_string());
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        exports
    }

    fn walk_variable_value(
        &self,
        node: Node,
        source: &[u8],
        _file_path: &Path,
        symbols: &mut Vec<Symbol>,
        is_exported: bool,
    ) {
        match node.kind() {
            "arrow_function" | "function_expression" => {
                if let Some(parent) = node.parent() {
                    if parent.kind() == "variable_declarator" {
                        if let Some(name_node) = parent.child_by_field_name("name") {
                            let name = node_text(name_node, source).to_string();
                            let range = ByteRange::from(parent);
                            let signature = format!(
                                "const {} = {}",
                                name,
                                self.signature_from_node(node, source)
                            );
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
            }
            _ => {}
        }
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
                if child.kind() == "method_definition" {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        let name = node_text(name_node, source).to_string();
                        let range = ByteRange::from(child);
                        let signature = self.signature_from_node(child, source);
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
            }
        }
    }

    fn walk_top_level(
        &self,
        node: Node,
        source: &[u8],
        file_path: &Path,
        symbols: &mut Vec<Symbol>,
        imports: &mut Vec<Import>,
        exports: &mut Vec<String>,
    ) {
        for i in 0..node.named_child_count() {
            let Some(child) = node.named_child(i) else {
                continue;
            };
            let kind = child.kind();
            match kind {
                "import_statement" => {
                    if let Some(imp) = self.extract_import(child, source) {
                        imports.push(imp);
                    }
                }
                "export_statement" => {
                    if let Some(decl) = child.child_by_field_name("declaration") {
                        exports.extend(self.extract_exports(child, source));
                        self.walk_top_level_node(decl, source, file_path, symbols, true);
                    } else {
                        exports.extend(self.extract_exports(child, source));
                    }
                }
                "function_declaration"
                | "class_declaration"
                | "interface_declaration"
                | "type_alias_declaration"
                | "lexical_declaration"
                | "variable_declaration" => {
                    self.walk_top_level_node(child, source, file_path, symbols, false);
                }
                _ => {}
            }
        }
    }

    fn walk_top_level_node(
        &self,
        node: Node,
        source: &[u8],
        file_path: &Path,
        symbols: &mut Vec<Symbol>,
        is_exported: bool,
    ) {
        match node.kind() {
            "function_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = node_text(name_node, source).to_string();
                    let range = ByteRange::from(node);
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
            "class_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = node_text(name_node, source).to_string();
                    let range = ByteRange::from(node);
                    let signature = self.signature_from_node(node, source);
                    symbols.push(Symbol {
                        kind: SymbolKind::Class,
                        name: name.clone(),
                        range,
                        signature,
                        is_exported,
                        parent_class: None,
                    });
                    if let Some(body) = node.child_by_field_name("body") {
                        self.walk_class_body(body, source, symbols, &name);
                    }
                }
            }
            "interface_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = node_text(name_node, source).to_string();
                    let range = ByteRange::from(node);
                    let signature = self.signature_from_node(node, source);
                    symbols.push(Symbol {
                        kind: SymbolKind::Interface,
                        name,
                        range,
                        signature,
                        is_exported,
                        parent_class: None,
                    });
                }
            }
            "type_alias_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = node_text(name_node, source).to_string();
                    let range = ByteRange::from(node);
                    let signature = self.signature_from_node(node, source);
                    symbols.push(Symbol {
                        kind: SymbolKind::TypeAlias,
                        name,
                        range,
                        signature,
                        is_exported,
                        parent_class: None,
                    });
                }
            }
            "lexical_declaration" | "variable_declaration" => {
                for i in 0..node.named_child_count() {
                    if let Some(decl) = node.named_child(i) {
                        if decl.kind() == "variable_declarator" {
                            if let Some(value) = decl.child_by_field_name("value") {
                                self.walk_variable_value(
                                    value,
                                    source,
                                    file_path,
                                    symbols,
                                    is_exported,
                                );
                            } else if let Some(name_node) = decl.child_by_field_name("name") {
                                let name = node_text(name_node, source).to_string();
                                let range = ByteRange::from(decl);
                                symbols.push(Symbol {
                                    kind: SymbolKind::Variable,
                                    name,
                                    range,
                                    signature: String::new(),
                                    is_exported,
                                    parent_class: None,
                                });
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

impl LanguageAdapter for TypescriptAdapter {
    fn extensions(&self) -> &[&str] {
        &[".ts", ".tsx"]
    }

    fn extract(&self, file_path: &Path, source: &str) -> Result<ExtractedFile, String> {
        let lang = self.language(file_path);
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

        self.walk_top_level(
            root,
            source_bytes,
            file_path,
            &mut symbols,
            &mut imports,
            &mut exports,
        );

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
        file_path: &Path,
        range: ByteRange,
    ) -> Result<Vec<String>, String> {
        let lang = self.language(file_path);
        let mut parser = Parser::new();
        parser
            .set_language(&lang)
            .map_err(|e| format!("Failed to set language: {e}"))?;

        let tree = parser.parse(source, None).ok_or("Failed to parse source")?;

        let root = tree.root_node();
        let source_bytes = source.as_bytes();

        let target = find_node_at_range(root, range.start_byte, range.end_byte)
            .ok_or("Could not find node at given range")?;

        let query_str = "(call_expression function: (identifier) @callee)";
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
