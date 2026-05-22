use std::path::Path;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

use crate::semantic::adapter::LanguageAdapter;
use crate::semantic::common::{find_node_at_range, node_text, signature_up_to_body};
use crate::semantic::types::{ByteRange, ExtractedFile, Import, ImportKind, Symbol, SymbolKind};

/// Tree-sitter adapter for Go. Uses the Go convention that
/// uppercase-first-letter names are exported. Methods are attached
/// to a receiver type — the receiver name (possibly through a
/// pointer) becomes `parent_class`.
pub struct GoAdapter;

impl GoAdapter {
    // Thin wrappers around the shared helpers in
    // `crate::semantic::common`. Method-shape preserved so the
    // bulk of this adapter reads unchanged.
    fn text<'a>(&self, n: Node<'a>, s: &'a [u8]) -> &'a str {
        node_text(n, s)
    }
    fn range(&self, n: Node) -> ByteRange {
        ByteRange::from(n)
    }
    /// Signature is the prefix up to the function body's opening
    /// brace. `func Foo(x int) int { ... }` → `func Foo(x int) int`.
    fn signature(&self, n: Node, s: &[u8]) -> String {
        signature_up_to_body(n, s)
    }

    /// True if the name starts with an uppercase ASCII letter —
    /// Go's exported-symbol convention.
    fn is_exported(&self, name: &str) -> bool {
        name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
    }

    /// Extract the receiver type for a `method_declaration`. The
    /// first `parameter_list` child is the receiver (e.g. `(p *P)`);
    /// dig past the optional `pointer_type` to find the underlying
    /// `type_identifier`.
    fn method_receiver_type<'a>(&self, n: Node<'a>, s: &'a [u8]) -> Option<String> {
        // Find the first parameter_list — that's the receiver.
        for i in 0..n.named_child_count() {
            let c = n.named_child(i)?;
            if c.kind() != "parameter_list" {
                continue;
            }
            // Inside it find parameter_declaration → its type child.
            for j in 0..c.named_child_count() {
                let pd = c.named_child(j)?;
                if pd.kind() != "parameter_declaration" {
                    continue;
                }
                // Walk children for type_identifier or pointer_type.
                for k in 0..pd.named_child_count() {
                    let t = pd.named_child(k)?;
                    match t.kind() {
                        "type_identifier" => return Some(self.text(t, s).to_string()),
                        "pointer_type" => {
                            for m in 0..t.named_child_count() {
                                if let Some(inner) = t.named_child(m)
                                    && inner.kind() == "type_identifier"
                                {
                                    return Some(self.text(inner, s).to_string());
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            // First parameter_list checked (always the receiver); bail.
            break;
        }
        None
    }

    fn handle_function(&self, n: Node, s: &[u8], symbols: &mut Vec<Symbol>) {
        let Some(name_node) = n.named_child(0).filter(|c| c.kind() == "identifier") else {
            return;
        };
        let name = self.text(name_node, s).to_string();
        symbols.push(Symbol {
            kind: SymbolKind::Function,
            is_exported: self.is_exported(&name),
            name,
            range: self.range(n),
            signature: self.signature(n, s),
            parent_class: None,
        });
    }

    fn handle_method(&self, n: Node, s: &[u8], symbols: &mut Vec<Symbol>) {
        // Method declarations have a receiver parameter_list first,
        // then a `field_identifier` for the method name.
        let mut name: Option<String> = None;
        for i in 0..n.named_child_count() {
            if let Some(c) = n.named_child(i)
                && c.kind() == "field_identifier"
            {
                name = Some(self.text(c, s).to_string());
                break;
            }
        }
        let Some(name) = name else {
            return;
        };
        symbols.push(Symbol {
            kind: SymbolKind::Method,
            is_exported: self.is_exported(&name),
            parent_class: self.method_receiver_type(n, s),
            name,
            range: self.range(n),
            signature: self.signature(n, s),
        });
    }

    /// `type Name <kind>` — Class for struct, Interface for
    /// interface, TypeAlias for everything else (alias / func
    /// type / channel type / etc.).
    fn handle_type_decl(&self, n: Node, s: &[u8], symbols: &mut Vec<Symbol>) {
        for i in 0..n.named_child_count() {
            let Some(spec) = n.named_child(i) else {
                continue;
            };
            if spec.kind() != "type_spec" && spec.kind() != "type_alias" {
                continue;
            }
            let Some(name_node) = spec
                .named_child(0)
                .filter(|c| c.kind() == "type_identifier")
            else {
                continue;
            };
            let name = self.text(name_node, s).to_string();
            // The second child is the actual type expression.
            let kind = if let Some(t) = spec.named_child(1) {
                match t.kind() {
                    "interface_type" => SymbolKind::Interface,
                    "struct_type" => SymbolKind::Class,
                    _ => SymbolKind::TypeAlias,
                }
            } else {
                SymbolKind::TypeAlias
            };
            // If interface, also extract its method declarations as
            // Method symbols.
            if kind == SymbolKind::Interface
                && let Some(iface) = spec.named_child(1)
            {
                for j in 0..iface.named_child_count() {
                    if let Some(m) = iface.named_child(j)
                        && m.kind() == "method_elem"
                        && let Some(mn) =
                            m.named_child(0).filter(|c| c.kind() == "field_identifier")
                    {
                        let mname = self.text(mn, s).to_string();
                        symbols.push(Symbol {
                            kind: SymbolKind::Method,
                            is_exported: self.is_exported(&mname),
                            parent_class: Some(name.clone()),
                            name: mname,
                            range: self.range(m),
                            signature: self.text(m, s).to_string(),
                        });
                    }
                }
            }
            symbols.push(Symbol {
                kind,
                is_exported: self.is_exported(&name),
                name,
                range: self.range(spec),
                signature: self.text(spec, s).lines().next().unwrap_or("").to_string(),
                parent_class: None,
            });
        }
    }

    /// Walks `var_declaration` / `const_declaration` (both bare and
    /// grouped) and emits one Variable per declared name. Grouped
    /// declarations have `var_spec` / `const_spec` children, each
    /// with one or more `identifier`s. Bare declarations carry the
    /// spec inline.
    fn handle_var_or_const(&self, n: Node, s: &[u8], symbols: &mut Vec<Symbol>) {
        let mut emit_from_spec = |spec: Node| {
            for j in 0..spec.named_child_count() {
                if let Some(id) = spec.named_child(j)
                    && id.kind() == "identifier"
                {
                    let name = self.text(id, s).to_string();
                    symbols.push(Symbol {
                        kind: SymbolKind::Variable,
                        is_exported: self.is_exported(&name),
                        name,
                        range: self.range(spec),
                        signature: self.text(spec, s).lines().next().unwrap_or("").to_string(),
                        parent_class: None,
                    });
                }
            }
        };
        for i in 0..n.named_child_count() {
            let Some(c) = n.named_child(i) else { continue };
            match c.kind() {
                "var_spec" | "const_spec" => emit_from_spec(c),
                // Grouped form `var ( a = 1; b = 2 )` wraps the
                // specs in a `var_spec_list` (or `const_spec_list`);
                // unwrap to get each spec.
                "var_spec_list" | "const_spec_list" => {
                    for j in 0..c.named_child_count() {
                        if let Some(spec) = c.named_child(j)
                            && matches!(spec.kind(), "var_spec" | "const_spec")
                        {
                            emit_from_spec(spec);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn handle_imports(&self, n: Node, s: &[u8], imports: &mut Vec<Import>) {
        // import_declaration has import_spec children (or import_spec_list).
        let mut harvest = |spec: Node| {
            for j in 0..spec.named_child_count() {
                if let Some(lit) = spec.named_child(j)
                    && lit.kind() == "interpreted_string_literal"
                {
                    let raw = self.text(lit, s);
                    let path = raw.trim_matches('"').to_string();
                    imports.push(Import {
                        names: vec![path.clone()],
                        source: path,
                        kind: ImportKind::Module,
                    });
                }
            }
        };
        for i in 0..n.named_child_count() {
            let Some(c) = n.named_child(i) else { continue };
            match c.kind() {
                "import_spec" => harvest(c),
                "import_spec_list" => {
                    for j in 0..c.named_child_count() {
                        if let Some(spec) = c.named_child(j)
                            && spec.kind() == "import_spec"
                        {
                            harvest(spec);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

impl LanguageAdapter for GoAdapter {
    fn extensions(&self) -> &[&str] {
        &[".go"]
    }

    fn extract(&self, file_path: &Path, source: &str) -> Result<ExtractedFile, String> {
        let lang: tree_sitter::Language = tree_sitter_go::LANGUAGE.into();
        let mut parser = Parser::new();
        parser
            .set_language(&lang)
            .map_err(|e| format!("Failed to set language: {e}"))?;
        let tree = parser.parse(source, None).ok_or("Failed to parse source")?;
        let root = tree.root_node();
        let source_bytes = source.as_bytes();

        let mut symbols = Vec::new();
        let mut imports = Vec::new();
        let mut warnings = Vec::new();

        if root.has_error() {
            warnings.push("tree-sitter reported syntax errors".to_string());
        }

        for i in 0..root.named_child_count() {
            let Some(c) = root.named_child(i) else {
                continue;
            };
            match c.kind() {
                "function_declaration" => self.handle_function(c, source_bytes, &mut symbols),
                "method_declaration" => self.handle_method(c, source_bytes, &mut symbols),
                "type_declaration" => self.handle_type_decl(c, source_bytes, &mut symbols),
                "import_declaration" => self.handle_imports(c, source_bytes, &mut imports),
                // Top-level `var x = ...;` / `const x = ...;` and
                // their grouped forms `var ( x = 1; y = 2 )`. Both
                // shapes use the same node kinds at the top level;
                // grouped declarations have multiple `var_spec` /
                // `const_spec` children.
                "var_declaration" | "const_declaration" => {
                    self.handle_var_or_const(c, source_bytes, &mut symbols);
                }
                _ => {}
            }
        }

        // Populate `exports` from is_exported symbols so consumers
        // get a quick "what does this file export?" view without
        // re-iterating the symbol vec. Matches the pattern used by
        // every other adapter.
        let exports: Vec<String> = symbols
            .iter()
            .filter(|s| s.is_exported)
            .map(|s| s.name.clone())
            .collect();

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
        let lang: tree_sitter::Language = tree_sitter_go::LANGUAGE.into();
        let mut parser = Parser::new();
        parser
            .set_language(&lang)
            .map_err(|e| format!("Failed to set language: {e}"))?;
        let tree = parser.parse(source, None).ok_or("Failed to parse source")?;
        let root = tree.root_node();
        let source_bytes = source.as_bytes();

        let target = find_node_at_range(root, range.start_byte, range.end_byte)
            .ok_or("Could not find node at given range")?;

        // Direct calls: `foo(...)`. Method calls: `obj.bar(...)` —
        // capture both the identifier and the selector's
        // field_identifier so callers see method names too.
        let query_str = r#"
            (call_expression function: (identifier) @callee)
            (call_expression function: (selector_expression field: (field_identifier) @callee))
        "#;
        let query = Query::new(&lang, query_str).map_err(|e| format!("Query error: {e}"))?;
        let mut cursor = QueryCursor::new();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pb(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(name)
    }

    #[test]
    fn extracts_function_with_export_convention() {
        let src = "package main\nfunc Hello() {}\nfunc private() {}\n";
        let f = GoAdapter.extract(&pb("a.go"), src).unwrap();
        let pub_sym = f.symbols.iter().find(|s| s.name == "Hello").unwrap();
        let priv_sym = f.symbols.iter().find(|s| s.name == "private").unwrap();
        assert!(pub_sym.is_exported);
        assert!(!priv_sym.is_exported);
        assert!(matches!(pub_sym.kind, SymbolKind::Function));
    }

    #[test]
    fn extracts_method_with_receiver_type() {
        let src = "package main\ntype P struct{}\nfunc (p *P) Greet() string { return \"hi\" }\n";
        let f = GoAdapter.extract(&pb("a.go"), src).unwrap();
        let m = f.symbols.iter().find(|s| s.name == "Greet").unwrap();
        assert!(matches!(m.kind, SymbolKind::Method));
        assert_eq!(m.parent_class.as_deref(), Some("P"));
    }

    #[test]
    fn extracts_struct_as_class_and_interface_as_interface() {
        let src = "package main\ntype Foo struct{ name string }\ntype Bar interface { M() string }\ntype N = int\n";
        let f = GoAdapter.extract(&pb("a.go"), src).unwrap();
        let foo = f.symbols.iter().find(|s| s.name == "Foo").unwrap();
        let bar = f.symbols.iter().find(|s| s.name == "Bar").unwrap();
        let n = f.symbols.iter().find(|s| s.name == "N").unwrap();
        assert!(matches!(foo.kind, SymbolKind::Class));
        assert!(matches!(bar.kind, SymbolKind::Interface));
        assert!(matches!(n.kind, SymbolKind::TypeAlias));
        // Interface method must appear as Method with parent_class.
        let m = f.symbols.iter().find(|s| s.name == "M").unwrap();
        assert_eq!(m.parent_class.as_deref(), Some("Bar"));
    }

    #[test]
    fn extracts_imports() {
        let src = "package main\nimport \"fmt\"\nimport (\n  \"os\"\n  \"strings\"\n)\n";
        let f = GoAdapter.extract(&pb("a.go"), src).unwrap();
        assert!(f.imports.iter().any(|i| i.source == "fmt"));
        assert!(f.imports.iter().any(|i| i.source == "os"));
        assert!(f.imports.iter().any(|i| i.source == "strings"));
    }

    #[test]
    fn find_callees_captures_direct_and_method_calls() {
        let src = "package main\nimport \"fmt\"\nfunc Run() { fmt.Println(\"a\"); helper() }\nfunc helper() {}\n";
        let f = GoAdapter.extract(&pb("a.go"), src).unwrap();
        let run = f.symbols.iter().find(|s| s.name == "Run").unwrap();
        let callees = GoAdapter
            .find_callees_in_range(src, &pb("a.go"), run.range)
            .unwrap();
        assert!(callees.contains(&"Println".to_string()));
        assert!(callees.contains(&"helper".to_string()));
    }

    /// Top-level `var` / `const` (both bare and grouped) — previously
    /// dropped entirely.
    #[test]
    fn extracts_var_and_const_declarations() {
        let src = "package main\nvar Single = 1\nvar (\n  Grouped = 2\n  Other = 3\n)\nconst Max = 100\nconst (\n  A = \"a\"\n  B = \"b\"\n)\n";
        let f = GoAdapter.extract(&pb("a.go"), src).unwrap();
        for needed in ["Single", "Grouped", "Other", "Max", "A", "B"] {
            assert!(
                f.symbols.iter().any(|s| s.name == needed),
                "missing var/const: {needed}; got {:?}",
                f.symbols.iter().map(|s| &s.name).collect::<Vec<_>>(),
            );
        }
        // All these uppercase names should be exported.
        for needed in ["Single", "Grouped", "Max"] {
            let sym = f.symbols.iter().find(|s| s.name == needed).unwrap();
            assert!(sym.is_exported, "{needed} should be exported");
        }
    }
}
