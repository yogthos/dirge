use std::path::Path;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

use crate::semantic::adapter::LanguageAdapter;
use crate::semantic::common::{find_node_at_range, node_text, signature_up_to_body};
use crate::semantic::types::{ByteRange, ExtractedFile, Import, ImportKind, Symbol, SymbolKind};

/// Tree-sitter adapter for Rust. dirge itself is written in Rust;
/// this was a glaring gap — list_symbols / find_callers worked for
/// Python, TS, and Clojure but not for the codebase the agent is
/// most often editing.
///
/// Exports are detected via `visibility_modifier` ("pub", "pub(crate)",
/// "pub(super)", etc.). Anything visibility-tagged counts as exported;
/// items without a visibility modifier stay private.
pub struct RustAdapter;

impl RustAdapter {
    fn text<'a>(&self, n: Node<'a>, s: &'a [u8]) -> &'a str {
        node_text(n, s)
    }
    fn range(&self, n: Node) -> ByteRange {
        ByteRange::from(n)
    }
    fn signature(&self, n: Node, s: &[u8]) -> String {
        signature_up_to_body(n, s)
    }

    /// True if any direct child is a `visibility_modifier`.
    fn is_exported(&self, n: Node) -> bool {
        for i in 0..n.named_child_count() {
            if let Some(c) = n.named_child(i)
                && c.kind() == "visibility_modifier"
            {
                return true;
            }
        }
        false
    }

    fn ident_child<'a>(&self, n: Node<'a>, s: &'a [u8]) -> Option<String> {
        // `function_item` uses `identifier`; `struct_item`/`enum_item`/
        // `trait_item`/`type_item` use `type_identifier`. Try both.
        for i in 0..n.named_child_count() {
            let c = n.named_child(i)?;
            if matches!(c.kind(), "identifier" | "type_identifier") {
                return Some(self.text(c, s).to_string());
            }
        }
        None
    }

    /// Extract the base type name from a possibly-generic type
    /// expression: `Foo` → `Foo`, `Foo<T>` → `Foo`, `Box<Foo<T>>` →
    /// `Box`. Used by `handle_impl` so `impl<T> Trait for Foo<T>`
    /// attaches its methods to `Foo`, not the generic param `T`.
    fn type_leaf_name(&self, n: Node, s: &[u8]) -> Option<String> {
        match n.kind() {
            "type_identifier" => Some(self.text(n, s).to_string()),
            "generic_type" | "scoped_type_identifier" => {
                // First named child is the base type expr.
                for i in 0..n.named_child_count() {
                    if let Some(c) = n.named_child(i)
                        && let Some(name) = self.type_leaf_name(c, s)
                    {
                        return Some(name);
                    }
                }
                None
            }
            _ => None,
        }
    }

    fn handle_function(&self, n: Node, s: &[u8], symbols: &mut Vec<Symbol>) {
        let Some(name) = self.ident_child(n, s) else {
            return;
        };
        symbols.push(Symbol {
            kind: SymbolKind::Function,
            is_exported: self.is_exported(n),
            name,
            range: self.range(n),
            signature: self.signature(n, s),
            parent_class: None,
        });
    }

    fn handle_struct_or_enum(&self, n: Node, s: &[u8], symbols: &mut Vec<Symbol>) {
        let Some(name) = self.ident_child(n, s) else {
            return;
        };
        symbols.push(Symbol {
            kind: SymbolKind::Class,
            is_exported: self.is_exported(n),
            name,
            range: self.range(n),
            signature: self.text(n, s).lines().next().unwrap_or("").to_string(),
            parent_class: None,
        });
    }

    fn handle_trait(&self, n: Node, s: &[u8], symbols: &mut Vec<Symbol>) {
        let Some(trait_name) = self.ident_child(n, s) else {
            return;
        };
        symbols.push(Symbol {
            kind: SymbolKind::Interface,
            is_exported: self.is_exported(n),
            name: trait_name.clone(),
            range: self.range(n),
            signature: self.text(n, s).lines().next().unwrap_or("").to_string(),
            parent_class: None,
        });
        // Walk trait body for required-method signatures + provided
        // method bodies; both become Method symbols anchored to the
        // trait name.
        for i in 0..n.named_child_count() {
            let Some(c) = n.named_child(i) else { continue };
            if c.kind() != "declaration_list" {
                continue;
            }
            for j in 0..c.named_child_count() {
                let Some(m) = c.named_child(j) else { continue };
                let mname = match m.kind() {
                    "function_item" | "function_signature_item" => self.ident_child(m, s),
                    _ => None,
                };
                if let Some(mname) = mname {
                    symbols.push(Symbol {
                        kind: SymbolKind::Method,
                        is_exported: true,
                        name: mname,
                        range: self.range(m),
                        signature: self.signature(m, s),
                        parent_class: Some(trait_name.clone()),
                    });
                }
            }
        }
    }

    fn handle_impl(&self, n: Node, s: &[u8], symbols: &mut Vec<Symbol>) {
        // `impl Type { ... }` or `impl Trait for Type { ... }`. The
        // RECEIVING type (the implementor) is the most useful
        // parent_class — it's what the user types when they want
        // "all methods on Foo".
        //
        // tree-sitter-rust exposes the receiving type via the `type`
        // field, which is robust against generic args, lifetime
        // params, and `<T as Trait>::` paths that would otherwise
        // confuse a positional last-type_identifier walk. Fall back
        // to the positional walk only when the field is absent.
        let receiving = n
            .child_by_field_name("type")
            .and_then(|t| self.type_leaf_name(t, s))
            .or_else(|| {
                let mut last: Option<String> = None;
                for i in 0..n.named_child_count() {
                    if let Some(c) = n.named_child(i)
                        && c.kind() == "type_identifier"
                    {
                        last = Some(self.text(c, s).to_string());
                    }
                }
                last
            });
        let Some(receiving) = receiving else {
            return;
        };
        for i in 0..n.named_child_count() {
            let Some(c) = n.named_child(i) else { continue };
            if c.kind() != "declaration_list" {
                continue;
            }
            for j in 0..c.named_child_count() {
                let Some(m) = c.named_child(j) else { continue };
                if m.kind() != "function_item" {
                    continue;
                }
                if let Some(mname) = self.ident_child(m, s) {
                    symbols.push(Symbol {
                        kind: SymbolKind::Method,
                        is_exported: self.is_exported(m),
                        name: mname,
                        range: self.range(m),
                        signature: self.signature(m, s),
                        parent_class: Some(receiving.clone()),
                    });
                }
            }
        }
    }

    fn handle_type_alias(&self, n: Node, s: &[u8], symbols: &mut Vec<Symbol>) {
        let Some(name) = self.ident_child(n, s) else {
            return;
        };
        symbols.push(Symbol {
            kind: SymbolKind::TypeAlias,
            is_exported: self.is_exported(n),
            name,
            range: self.range(n),
            signature: self.text(n, s).lines().next().unwrap_or("").to_string(),
            parent_class: None,
        });
    }

    fn handle_const_or_static(&self, n: Node, s: &[u8], symbols: &mut Vec<Symbol>) {
        // `const NAME: T = ...;` / `static NAME: T = ...;` — the
        // name is an `identifier` child.
        let mut name: Option<String> = None;
        for i in 0..n.named_child_count() {
            if let Some(c) = n.named_child(i)
                && c.kind() == "identifier"
            {
                name = Some(self.text(c, s).to_string());
                break;
            }
        }
        let Some(name) = name else { return };
        symbols.push(Symbol {
            kind: SymbolKind::Variable,
            is_exported: self.is_exported(n),
            name,
            range: self.range(n),
            signature: self.text(n, s).lines().next().unwrap_or("").to_string(),
            parent_class: None,
        });
    }

    fn handle_mod(&self, n: Node, s: &[u8], symbols: &mut Vec<Symbol>, imports: &mut Vec<Import>) {
        let Some(name) = self.ident_child(n, s) else {
            return;
        };
        symbols.push(Symbol {
            kind: SymbolKind::Class,
            is_exported: self.is_exported(n),
            name,
            range: self.range(n),
            signature: self.text(n, s).lines().next().unwrap_or("").to_string(),
            parent_class: None,
        });
        // Inline modules carry a `declaration_list`; file-only
        // `mod foo;` doesn't, and we skip that case (the file's
        // own indexing covers its contents).
        for i in 0..n.named_child_count() {
            let Some(c) = n.named_child(i) else { continue };
            if c.kind() != "declaration_list" {
                continue;
            }
            for j in 0..c.named_child_count() {
                let Some(item) = c.named_child(j) else {
                    continue;
                };
                match item.kind() {
                    "function_item" => self.handle_function(item, s, symbols),
                    "struct_item" | "enum_item" | "union_item" => {
                        self.handle_struct_or_enum(item, s, symbols);
                    }
                    "trait_item" => self.handle_trait(item, s, symbols),
                    "impl_item" => self.handle_impl(item, s, symbols),
                    "type_item" => self.handle_type_alias(item, s, symbols),
                    "const_item" | "static_item" => self.handle_const_or_static(item, s, symbols),
                    "use_declaration" => self.handle_use(item, s, imports),
                    "mod_item" => self.handle_mod(item, s, symbols, imports),
                    "macro_definition" => self.handle_macro(item, s, symbols),
                    "foreign_mod_item" => self.handle_extern_block(item, s, symbols),
                    _ => {}
                }
            }
        }
    }

    fn handle_macro(&self, n: Node, s: &[u8], symbols: &mut Vec<Symbol>) {
        // `macro_rules! NAME { ... }`. The macro name appears as an
        // `identifier` child of the macro_definition node.
        for i in 0..n.named_child_count() {
            if let Some(c) = n.named_child(i)
                && c.kind() == "identifier"
            {
                let name = self.text(c, s).to_string();
                symbols.push(Symbol {
                    kind: SymbolKind::Function,
                    is_exported: self.is_exported(n),
                    name,
                    range: self.range(n),
                    signature: self.text(n, s).lines().next().unwrap_or("").to_string(),
                    parent_class: None,
                });
                return;
            }
        }
    }

    fn handle_extern_block(&self, n: Node, s: &[u8], symbols: &mut Vec<Symbol>) {
        // `extern "C" { ... }` — body is a `declaration_list` with
        // `function_signature_item`, `static_item`, and
        // `type_item` children.
        for i in 0..n.named_child_count() {
            let Some(c) = n.named_child(i) else { continue };
            if c.kind() != "declaration_list" {
                continue;
            }
            for j in 0..c.named_child_count() {
                let Some(item) = c.named_child(j) else {
                    continue;
                };
                match item.kind() {
                    "function_signature_item" => {
                        if let Some(name) = self.ident_child(item, s) {
                            symbols.push(Symbol {
                                kind: SymbolKind::Function,
                                is_exported: true,
                                name,
                                range: self.range(item),
                                signature: self.signature(item, s),
                                parent_class: None,
                            });
                        }
                    }
                    "static_item" => self.handle_const_or_static(item, s, symbols),
                    "type_item" => self.handle_type_alias(item, s, symbols),
                    _ => {}
                }
            }
        }
    }

    fn handle_use(&self, n: Node, s: &[u8], imports: &mut Vec<Import>) {
        // `use std::sync::Arc;` — the first non-keyword child is
        // the path. Render it as a single import string; opencode/pi
        // do similar.
        for i in 0..n.named_child_count() {
            let Some(c) = n.named_child(i) else { continue };
            match c.kind() {
                "scoped_identifier" | "identifier" | "use_list" | "use_as_clause" => {
                    let path = self.text(c, s).to_string();
                    imports.push(Import {
                        names: vec![path.clone()],
                        source: path,
                        kind: ImportKind::Qualified,
                    });
                    break;
                }
                _ => {}
            }
        }
    }
}

impl LanguageAdapter for RustAdapter {
    fn extensions(&self) -> &[&str] {
        &[".rs"]
    }

    fn extract(&self, file_path: &Path, source: &str) -> Result<ExtractedFile, String> {
        let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let mut parser = Parser::new();
        parser
            .set_language(&lang)
            .map_err(|e| format!("Failed to set language: {e}"))?;
        let tree = parser.parse(source, None).ok_or("Failed to parse source")?;
        let root = tree.root_node();
        let bytes = source.as_bytes();

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
                "function_item" => self.handle_function(c, bytes, &mut symbols),
                "struct_item" | "enum_item" | "union_item" => {
                    self.handle_struct_or_enum(c, bytes, &mut symbols);
                }
                "trait_item" => self.handle_trait(c, bytes, &mut symbols),
                "impl_item" => self.handle_impl(c, bytes, &mut symbols),
                "type_item" => self.handle_type_alias(c, bytes, &mut symbols),
                "const_item" | "static_item" => self.handle_const_or_static(c, bytes, &mut symbols),
                "use_declaration" => self.handle_use(c, bytes, &mut imports),
                // Top-level `mod inner { ... }` — surface the module
                // itself as a Class so list_symbols anchors on it,
                // then recurse into the body so contents are indexed
                // alongside top-level items.
                "mod_item" => self.handle_mod(c, bytes, &mut symbols, &mut imports),
                // `macro_rules! foo { ... }` — declarative macros.
                // Treated as Function (closest match in SymbolKind);
                // the LLM commonly invokes them like fns.
                "macro_definition" => self.handle_macro(c, bytes, &mut symbols),
                // `extern "ABI" { fn foo(); type Bar; }` — FFI
                // blocks. Walk the body for declarations so the
                // declared signatures are visible.
                "foreign_mod_item" => self.handle_extern_block(c, bytes, &mut symbols),
                _ => {}
            }
        }

        // Populate `exports` from is_exported symbols (each adapter
        // does this so consumers don't re-iterate the symbol vec).
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
        })
    }

    fn find_callees_in_range(
        &self,
        source: &str,
        _file_path: &Path,
        range: ByteRange,
    ) -> Result<Vec<String>, String> {
        let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let mut parser = Parser::new();
        parser
            .set_language(&lang)
            .map_err(|e| format!("Failed to set language: {e}"))?;
        let tree = parser.parse(source, None).ok_or("Failed to parse source")?;
        let root = tree.root_node();
        let bytes = source.as_bytes();

        let target = find_node_at_range(root, range.start_byte, range.end_byte)
            .ok_or("Could not find node at given range")?;

        // Direct call: `foo(...)`. Method call: `obj.bar(...)` —
        // tree-sitter-rust models the call as `call_expression` with
        // function = `field_expression`; we capture the field name.
        // Macro invocations (`println!`, etc.) appear separately as
        // `macro_invocation`; capture their identifier too.
        let query_str = r#"
            (call_expression function: (identifier) @callee)
            (call_expression function: (field_expression field: (field_identifier) @callee))
            (macro_invocation macro: (identifier) @callee)
        "#;
        let query = Query::new(&lang, query_str).map_err(|e| format!("Query error: {e}"))?;
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, target, bytes);

        let mut callees = Vec::new();
        while let Some(m) = matches.next() {
            for capture in m.captures {
                let name = capture.node.utf8_text(bytes).unwrap_or("");
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

    fn pb(n: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(n)
    }

    #[test]
    fn extracts_pub_fn_as_exported_and_private_fn_not() {
        let src = "pub fn a() {}\nfn b() {}\n";
        let f = RustAdapter.extract(&pb("x.rs"), src).unwrap();
        let a = f.symbols.iter().find(|s| s.name == "a").unwrap();
        let b = f.symbols.iter().find(|s| s.name == "b").unwrap();
        assert!(a.is_exported);
        assert!(!b.is_exported);
        assert!(matches!(a.kind, SymbolKind::Function));
    }

    #[test]
    fn extracts_struct_enum_as_class() {
        let src = "pub struct Foo { name: String }\npub enum Bar { A, B }\n";
        let f = RustAdapter.extract(&pb("x.rs"), src).unwrap();
        assert!(
            f.symbols
                .iter()
                .any(|s| s.name == "Foo" && matches!(s.kind, SymbolKind::Class))
        );
        assert!(
            f.symbols
                .iter()
                .any(|s| s.name == "Bar" && matches!(s.kind, SymbolKind::Class))
        );
    }

    #[test]
    fn extracts_trait_with_method_signatures() {
        let src = "pub trait Greeter {\n  fn greet(&self) -> String;\n  fn default_greet(&self) -> String { \"hi\".to_string() }\n}\n";
        let f = RustAdapter.extract(&pb("x.rs"), src).unwrap();
        let trait_sym = f.symbols.iter().find(|s| s.name == "Greeter").unwrap();
        assert!(matches!(trait_sym.kind, SymbolKind::Interface));
        let g = f.symbols.iter().find(|s| s.name == "greet").unwrap();
        assert_eq!(g.parent_class.as_deref(), Some("Greeter"));
        let dg = f
            .symbols
            .iter()
            .find(|s| s.name == "default_greet")
            .unwrap();
        assert_eq!(dg.parent_class.as_deref(), Some("Greeter"));
    }

    #[test]
    fn impl_methods_attach_to_receiving_type() {
        let src = "pub struct Foo;\nimpl Greeter for Foo {\n  fn greet(&self) -> String { String::new() }\n}\n";
        let f = RustAdapter.extract(&pb("x.rs"), src).unwrap();
        let g = f.symbols.iter().find(|s| s.name == "greet").unwrap();
        assert!(matches!(g.kind, SymbolKind::Method));
        // For `impl Trait for Type`, the receiving type (Foo) is the
        // last type_identifier; that's what list_symbols filter on
        // `--parent Foo` should match.
        assert_eq!(g.parent_class.as_deref(), Some("Foo"));
    }

    #[test]
    fn extracts_type_alias() {
        let src = "pub type Id = u64;\n";
        let f = RustAdapter.extract(&pb("x.rs"), src).unwrap();
        let id = f.symbols.iter().find(|s| s.name == "Id").unwrap();
        assert!(matches!(id.kind, SymbolKind::TypeAlias));
        assert!(id.is_exported);
    }

    #[test]
    fn extracts_const_and_static_as_variable() {
        let src = "pub const MAX: u32 = 42;\nstatic GLOBAL: i32 = 0;\n";
        let f = RustAdapter.extract(&pb("x.rs"), src).unwrap();
        let m = f.symbols.iter().find(|s| s.name == "MAX").unwrap();
        let g = f.symbols.iter().find(|s| s.name == "GLOBAL").unwrap();
        assert!(matches!(m.kind, SymbolKind::Variable));
        assert!(m.is_exported);
        assert!(!g.is_exported);
    }

    #[test]
    fn extracts_use_imports() {
        let src = "use std::sync::Arc;\nuse crate::foo::Bar;\n";
        let f = RustAdapter.extract(&pb("x.rs"), src).unwrap();
        assert!(
            f.imports
                .iter()
                .any(|i| i.source.contains("std::sync::Arc"))
        );
        assert!(
            f.imports
                .iter()
                .any(|i| i.source.contains("crate::foo::Bar"))
        );
    }

    #[test]
    fn find_callees_captures_direct_method_and_macro() {
        let src = "pub fn run() { helper(); foo.bar(); println!(\"x\"); }\nfn helper() {}\n";
        let f = RustAdapter.extract(&pb("x.rs"), src).unwrap();
        let run = f.symbols.iter().find(|s| s.name == "run").unwrap();
        let callees = RustAdapter
            .find_callees_in_range(src, &pb("x.rs"), run.range)
            .unwrap();
        assert!(callees.contains(&"helper".to_string()));
        assert!(callees.contains(&"bar".to_string()));
        assert!(callees.contains(&"println".to_string()));
    }

    /// Inline `mod inner { ... }` — previously the module + every
    /// item inside it were silently dropped.
    #[test]
    fn extracts_inline_module_and_its_items() {
        let src = "pub mod inner {\n  pub fn deep() -> u32 { 42 }\n  pub struct Held;\n}\n";
        let f = RustAdapter.extract(&pb("x.rs"), src).unwrap();
        let m = f.symbols.iter().find(|s| s.name == "inner").unwrap();
        assert!(matches!(m.kind, SymbolKind::Class));
        assert!(m.is_exported);
        assert!(
            f.symbols
                .iter()
                .any(|s| s.name == "deep" && matches!(s.kind, SymbolKind::Function))
        );
        assert!(
            f.symbols
                .iter()
                .any(|s| s.name == "Held" && matches!(s.kind, SymbolKind::Class))
        );
    }

    /// `extern "C" { ... }` blocks — FFI declarations now visible.
    #[test]
    fn extracts_extern_block_signatures() {
        let src =
            "extern \"C\" {\n  fn foreign_fn(x: i32) -> i32;\n  static FOREIGN_GLOBAL: i32;\n}\n";
        let f = RustAdapter.extract(&pb("x.rs"), src).unwrap();
        let ff = f.symbols.iter().find(|s| s.name == "foreign_fn").unwrap();
        assert!(matches!(ff.kind, SymbolKind::Function));
        let g = f
            .symbols
            .iter()
            .find(|s| s.name == "FOREIGN_GLOBAL")
            .unwrap();
        assert!(matches!(g.kind, SymbolKind::Variable));
    }

    /// `macro_rules!` declarations.
    #[test]
    fn extracts_macro_rules() {
        let src = "macro_rules! my_mac { ($x:expr) => { $x + 1 }; }\n";
        let f = RustAdapter.extract(&pb("x.rs"), src).unwrap();
        let m = f.symbols.iter().find(|s| s.name == "my_mac").unwrap();
        assert!(matches!(m.kind, SymbolKind::Function));
    }

    /// `impl<T> Trait for Generic<T>` — the receiving type's base
    /// name (Generic) must be parent_class, not the generic param.
    #[test]
    fn impl_for_generic_type_uses_base_name() {
        let src = "pub struct Bag<T>(T);\nimpl<T: Clone> AsRef<T> for Bag<T> { fn as_ref(&self) -> &T { &self.0 } }\n";
        let f = RustAdapter.extract(&pb("x.rs"), src).unwrap();
        let m = f.symbols.iter().find(|s| s.name == "as_ref").unwrap();
        assert_eq!(m.parent_class.as_deref(), Some("Bag"));
    }
}
