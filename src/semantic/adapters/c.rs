use std::path::Path;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

use crate::semantic::adapter::LanguageAdapter;
use crate::semantic::types::{ByteRange, ExtractedFile, Import, Symbol, SymbolKind};

/// Tree-sitter adapter for C.
///
/// C has no real privacy concept beyond `static` (file scope) vs
/// extern linkage. We surface `static` functions/variables as
/// non-exported and everything else as exported.
///
/// `struct` and `enum` become `SymbolKind::Class`; `typedef`
/// becomes `TypeAlias` (unless it wraps a struct, in which case
/// only the inner struct symbol is emitted to avoid duplicates).
/// Headers (`#include`) are extracted as imports.
pub struct CAdapter;

impl CAdapter {
    fn text<'a>(&self, n: Node<'a>, s: &'a [u8]) -> &'a str {
        n.utf8_text(s).unwrap_or("")
    }

    fn range(&self, n: Node) -> ByteRange {
        ByteRange {
            start_byte: n.start_byte(),
            end_byte: n.end_byte(),
            start_line: n.start_position().row + 1,
            end_line: n.end_position().row + 1,
        }
    }

    fn signature(&self, n: Node, s: &[u8]) -> String {
        if let Some(body) = n.child_by_field_name("body") {
            return String::from_utf8_lossy(&s[n.start_byte()..body.start_byte()])
                .trim()
                .to_string();
        }
        let first = self.text(n, s).lines().next().unwrap_or("");
        if first.chars().count() > 80 {
            let p: String = first.chars().take(80).collect();
            format!("{p}…")
        } else {
            first.to_string()
        }
    }

    /// Direct child `storage_class_specifier` with text "static"
    /// indicates file-scope (non-exported). Functions/variables
    /// without `static` have external linkage by default.
    fn is_static(&self, n: Node, s: &[u8]) -> bool {
        for i in 0..n.named_child_count() {
            if let Some(c) = n.named_child(i)
                && c.kind() == "storage_class_specifier"
                && self.text(c, s) == "static"
            {
                return true;
            }
        }
        false
    }

    /// Walks a `function_declarator` to find the underlying
    /// identifier. The declarator can be nested under
    /// `pointer_declarator` (for `int *foo()`-style returns) or
    /// `parenthesized_declarator` (function pointers).
    fn declarator_name<'a>(&self, n: Node<'a>, s: &'a [u8]) -> Option<String> {
        match n.kind() {
            "identifier" => Some(self.text(n, s).to_string()),
            "function_declarator" | "pointer_declarator" | "parenthesized_declarator" => {
                // First named child that resolves to an identifier wins.
                for i in 0..n.named_child_count() {
                    if let Some(c) = n.named_child(i)
                        && let Some(name) = self.declarator_name(c, s)
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
        // `function_definition > function_declarator > identifier`.
        let mut name: Option<String> = None;
        for i in 0..n.named_child_count() {
            if let Some(c) = n.named_child(i)
                && c.kind() == "function_declarator"
            {
                name = self.declarator_name(c, s);
                break;
            }
            if let Some(c) = n.named_child(i)
                && c.kind() == "pointer_declarator"
            {
                name = self.declarator_name(c, s);
                if name.is_some() {
                    break;
                }
            }
        }
        let Some(name) = name else { return };
        symbols.push(Symbol {
            kind: SymbolKind::Function,
            is_exported: !self.is_static(n, s),
            name,
            range: self.range(n),
            signature: self.signature(n, s),
            parent_class: None,
        });
    }

    /// `struct Foo { … }` at top level. Anonymous structs (no
    /// `type_identifier` child) are skipped — there's no useful
    /// symbol name to attach. Forward declarations (`struct Foo;`,
    /// no body) are also skipped: emitting them creates a Class
    /// symbol with no contents that confuses code navigation, and
    /// the real definition elsewhere produces the canonical symbol.
    fn handle_struct(&self, n: Node, s: &[u8], symbols: &mut Vec<Symbol>) {
        let mut name: Option<String> = None;
        let mut has_body = false;
        for i in 0..n.named_child_count() {
            let Some(c) = n.named_child(i) else { continue };
            if c.kind() == "type_identifier" && name.is_none() {
                name = Some(self.text(c, s).to_string());
            }
            if c.kind() == "field_declaration_list" {
                has_body = true;
            }
        }
        if !has_body {
            return;
        }
        let Some(name) = name else { return };
        symbols.push(Symbol {
            kind: SymbolKind::Class,
            is_exported: true,
            name,
            range: self.range(n),
            signature: self.text(n, s).lines().next().unwrap_or("").to_string(),
            parent_class: None,
        });
    }

    fn handle_enum(&self, n: Node, s: &[u8], symbols: &mut Vec<Symbol>) {
        // Find the enum's tag name + its enumerator list in a single
        // pass so we can emit both the parent enum AND its constants
        // as Variable symbols. Previously the constants (`RED`,
        // `GREEN`, …) were lost; in C they're far more frequently
        // referenced than the enum tag itself.
        let mut name: Option<String> = None;
        let mut enumerator_list: Option<Node> = None;
        for i in 0..n.named_child_count() {
            let Some(c) = n.named_child(i) else { continue };
            match c.kind() {
                "type_identifier" => {
                    if name.is_none() {
                        name = Some(self.text(c, s).to_string());
                    }
                }
                "enumerator_list" => enumerator_list = Some(c),
                _ => {}
            }
        }
        let parent_name = name.clone();
        if let Some(name) = name {
            symbols.push(Symbol {
                kind: SymbolKind::Class,
                is_exported: true,
                name,
                range: self.range(n),
                signature: self.text(n, s).lines().next().unwrap_or("").to_string(),
                parent_class: None,
            });
        }
        // Emit each constant as a Variable symbol. Anonymous enums
        // (`enum { A, B };`) still surface their constants — the
        // parent enum just isn't named.
        if let Some(list) = enumerator_list {
            for i in 0..list.named_child_count() {
                if let Some(e) = list.named_child(i)
                    && e.kind() == "enumerator"
                {
                    // Enumerator's first identifier child is the
                    // constant name.
                    for j in 0..e.named_child_count() {
                        if let Some(id) = e.named_child(j)
                            && id.kind() == "identifier"
                        {
                            symbols.push(Symbol {
                                kind: SymbolKind::Variable,
                                is_exported: true,
                                name: self.text(id, s).to_string(),
                                range: self.range(e),
                                signature: self.text(e, s).to_string(),
                                parent_class: parent_name.clone(),
                            });
                            break;
                        }
                    }
                }
            }
        }
    }

    /// `typedef <type> <alias>;`. If `<type>` is an inline struct
    /// definition, surface only the struct (avoiding a duplicate
    /// alias symbol for the same range). Otherwise emit TypeAlias
    /// keyed on the rightmost identifier.
    fn handle_typedef(&self, n: Node, s: &[u8], symbols: &mut Vec<Symbol>) {
        // Scan for an inner struct_specifier or enum_specifier with a
        // type_identifier — those carry the canonical name.
        for i in 0..n.named_child_count() {
            if let Some(c) = n.named_child(i)
                && (c.kind() == "struct_specifier" || c.kind() == "enum_specifier")
            {
                // Only suppress the alias if the struct has its own
                // name — `typedef struct { ... } Foo;` (anonymous
                // struct) DOES need the alias to be visible.
                let has_inner_name = (0..c.named_child_count()).any(|j| {
                    c.named_child(j)
                        .map(|x| x.kind() == "type_identifier")
                        .unwrap_or(false)
                });
                if has_inner_name {
                    // The inner struct/enum will be handled by its
                    // own match arm in the top-level walk; nothing
                    // more to do here.
                    return;
                }
            }
        }
        // Last identifier in the typedef is the alias name.
        let mut alias: Option<String> = None;
        for i in (0..n.named_child_count()).rev() {
            if let Some(c) = n.named_child(i)
                && c.kind() == "type_identifier"
            {
                alias = Some(self.text(c, s).to_string());
                break;
            }
        }
        let Some(name) = alias else { return };
        symbols.push(Symbol {
            kind: SymbolKind::TypeAlias,
            is_exported: true,
            name,
            range: self.range(n),
            signature: self.text(n, s).lines().next().unwrap_or("").to_string(),
            parent_class: None,
        });
    }

    fn handle_include(&self, n: Node, s: &[u8], imports: &mut Vec<Import>) {
        // `#include <stdio.h>` → `system_lib_string`.
        // `#include "local.h"` → `string_literal > string_content`.
        for i in 0..n.named_child_count() {
            let Some(c) = n.named_child(i) else { continue };
            match c.kind() {
                "system_lib_string" => {
                    let raw = self.text(c, s);
                    let path = raw
                        .trim_matches(|ch: char| ch == '<' || ch == '>')
                        .to_string();
                    imports.push(Import {
                        names: vec![path.clone()],
                        source: path,
                    });
                    return;
                }
                "string_literal" => {
                    for j in 0..c.named_child_count() {
                        if let Some(sub) = c.named_child(j)
                            && sub.kind() == "string_content"
                        {
                            let path = self.text(sub, s).to_string();
                            imports.push(Import {
                                names: vec![path.clone()],
                                source: path,
                            });
                            return;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn find_node_at_range<'a>(&self, n: Node<'a>, start: usize, end: usize) -> Option<Node<'a>> {
        if n.start_byte() == start && n.end_byte() == end {
            return Some(n);
        }
        for i in 0..n.named_child_count() {
            if let Some(c) = n.named_child(i)
                && c.start_byte() <= start
                && c.end_byte() >= end
                && let Some(f) = self.find_node_at_range(c, start, end)
            {
                return Some(f);
            }
        }
        None
    }
}

impl LanguageAdapter for CAdapter {
    fn extensions(&self) -> &[&str] {
        // Header files share C's grammar in tree-sitter-c; C++
        // headers go through tree-sitter-cpp via the cpp adapter.
        &[".c", ".h"]
    }

    fn extract(&self, file_path: &Path, source: &str) -> Result<ExtractedFile, String> {
        let lang: tree_sitter::Language = tree_sitter_c::LANGUAGE.into();
        let mut parser = Parser::new();
        parser
            .set_language(&lang)
            .map_err(|e| format!("Failed to set language: {e}"))?;
        let tree = parser.parse(source, None).ok_or("Failed to parse source")?;
        let root = tree.root_node();
        let bytes = source.as_bytes();

        let mut symbols = Vec::new();
        let mut imports = Vec::new();
        let exports = Vec::new();
        let mut warnings = Vec::new();
        if root.has_error() {
            warnings.push("tree-sitter reported syntax errors".to_string());
        }

        for i in 0..root.named_child_count() {
            let Some(c) = root.named_child(i) else {
                continue;
            };
            match c.kind() {
                "function_definition" => self.handle_function(c, bytes, &mut symbols),
                "struct_specifier" => self.handle_struct(c, bytes, &mut symbols),
                "enum_specifier" => self.handle_enum(c, bytes, &mut symbols),
                "type_definition" => self.handle_typedef(c, bytes, &mut symbols),
                "preproc_include" => self.handle_include(c, bytes, &mut imports),
                _ => {}
            }
        }

        // `typedef struct Foo { ... } AliasName;` puts the struct
        // INSIDE a type_definition. To make `Foo` show up in
        // list_symbols we walk type_definition children for inner
        // struct/enum specifiers.
        for i in 0..root.named_child_count() {
            if let Some(c) = root.named_child(i)
                && c.kind() == "type_definition"
            {
                for j in 0..c.named_child_count() {
                    let Some(inner) = c.named_child(j) else {
                        continue;
                    };
                    match inner.kind() {
                        "struct_specifier" => self.handle_struct(inner, bytes, &mut symbols),
                        "enum_specifier" => self.handle_enum(inner, bytes, &mut symbols),
                        _ => {}
                    }
                }
            }
        }

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
        let lang: tree_sitter::Language = tree_sitter_c::LANGUAGE.into();
        let mut parser = Parser::new();
        parser
            .set_language(&lang)
            .map_err(|e| format!("Failed to set language: {e}"))?;
        let tree = parser.parse(source, None).ok_or("Failed to parse source")?;
        let root = tree.root_node();
        let bytes = source.as_bytes();

        let target = self
            .find_node_at_range(root, range.start_byte, range.end_byte)
            .ok_or("Could not find node at given range")?;

        // `call_expression > identifier` for `foo()`. Member access
        // `obj.foo()` parses with field_expression; we capture the
        // accessed field too.
        let query_str = r#"
            (call_expression function: (identifier) @callee)
            (call_expression function: (field_expression field: (field_identifier) @callee))
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
    fn extracts_function_with_static_visibility() {
        let src =
            "int add(int a, int b) { return a + b; }\nstatic int helper(int x) { return x; }\n";
        let f = CAdapter.extract(&pb("a.c"), src).unwrap();
        let add = f.symbols.iter().find(|s| s.name == "add").unwrap();
        let helper = f.symbols.iter().find(|s| s.name == "helper").unwrap();
        assert!(matches!(add.kind, SymbolKind::Function));
        assert!(add.is_exported);
        assert!(!helper.is_exported);
    }

    #[test]
    fn extracts_struct_and_enum_as_class() {
        let src = "struct Foo { int n; };\nenum Color { RED, GREEN };\n";
        let f = CAdapter.extract(&pb("a.c"), src).unwrap();
        let foo = f.symbols.iter().find(|s| s.name == "Foo").unwrap();
        let col = f.symbols.iter().find(|s| s.name == "Color").unwrap();
        assert!(matches!(foo.kind, SymbolKind::Class));
        assert!(matches!(col.kind, SymbolKind::Class));
    }

    #[test]
    fn extracts_typedef_struct_as_inner_struct() {
        // Named inner struct: surface only `Point` (the struct
        // name), not the alias.
        let src = "typedef struct Point { int x; int y; } Point;\n";
        let f = CAdapter.extract(&pb("a.c"), src).unwrap();
        let points: Vec<_> = f.symbols.iter().filter(|s| s.name == "Point").collect();
        // Inner struct emits one Class symbol; alias is suppressed
        // because the struct has its own name.
        assert_eq!(points.len(), 1);
        assert!(matches!(points[0].kind, SymbolKind::Class));
    }

    #[test]
    fn extracts_anonymous_typedef_as_alias() {
        // Anonymous inner struct: emit the alias (otherwise users
        // have no name for it).
        let src = "typedef struct { int x; } Vec;\n";
        let f = CAdapter.extract(&pb("a.c"), src).unwrap();
        let vec = f.symbols.iter().find(|s| s.name == "Vec").unwrap();
        assert!(matches!(vec.kind, SymbolKind::TypeAlias));
    }

    #[test]
    fn extracts_includes() {
        let src = "#include <stdio.h>\n#include \"local.h\"\n";
        let f = CAdapter.extract(&pb("a.c"), src).unwrap();
        assert!(f.imports.iter().any(|i| i.source == "stdio.h"));
        assert!(f.imports.iter().any(|i| i.source == "local.h"));
    }

    #[test]
    fn find_callees_captures_calls() {
        let src = "void run(void) { printf(\"hi\"); helper(); }\nstatic void helper(void) {}\n";
        let f = CAdapter.extract(&pb("a.c"), src).unwrap();
        let run = f.symbols.iter().find(|s| s.name == "run").unwrap();
        let callees = CAdapter
            .find_callees_in_range(src, &pb("a.c"), run.range)
            .unwrap();
        assert!(callees.contains(&"printf".to_string()));
        assert!(callees.contains(&"helper".to_string()));
    }

    /// Enum constants — `RED`/`GREEN` are individually surfaced as
    /// Variable symbols anchored to the parent enum tag.
    #[test]
    fn extracts_enum_constants_as_variables() {
        let src = "enum Color { RED, GREEN = 5, BLUE };\n";
        let f = CAdapter.extract(&pb("a.c"), src).unwrap();
        for needed in ["Color", "RED", "GREEN", "BLUE"] {
            assert!(
                f.symbols.iter().any(|s| s.name == needed),
                "missing: {needed}",
            );
        }
        let red = f.symbols.iter().find(|s| s.name == "RED").unwrap();
        assert!(matches!(red.kind, SymbolKind::Variable));
        assert_eq!(red.parent_class.as_deref(), Some("Color"));
    }

    /// Forward `struct` declarations (no body) are skipped to
    /// avoid duplicate symbols with the real definition.
    #[test]
    fn forward_struct_declaration_is_skipped() {
        let src = "struct Foo;\nstruct Foo { int n; };\n";
        let f = CAdapter.extract(&pb("a.c"), src).unwrap();
        let foos: Vec<_> = f.symbols.iter().filter(|s| s.name == "Foo").collect();
        assert_eq!(foos.len(), 1, "only the definition should produce a symbol");
    }
}
