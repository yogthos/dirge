use std::path::Path;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

use crate::semantic::adapter::LanguageAdapter;
use crate::semantic::types::{ByteRange, ExtractedFile, Import, Symbol, SymbolKind};

/// Tree-sitter adapter for C++.
///
/// Built on the C adapter's vocabulary plus C++ extras:
/// - `class_specifier` → Class with method children walked.
///   Public-by-default visibility is inverted vs `struct`: a
///   `class` body without an access label has private members,
///   while `struct` is public-by-default.
/// - `namespace_definition` → recursed; nested classes/functions
///   carry the namespace as a textual parent prefix in their
///   signature line (no separate Namespace SymbolKind exists).
/// - `template_declaration` → unwraps to the inner function /
///   class so generic code shows up alongside non-generic.
/// - `function_definition` (top-level) → Function.
/// - `using_declaration` / `using_directive` → Imports (best-effort).
pub struct CppAdapter;

impl CppAdapter {
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

    /// Recursive declarator walk shared with C — finds the
    /// identifier buried under pointer/reference/parenthesized
    /// wrappers.
    fn declarator_name<'a>(&self, n: Node<'a>, s: &'a [u8]) -> Option<String> {
        match n.kind() {
            "identifier" | "field_identifier" | "destructor_name" | "operator_name" => {
                Some(self.text(n, s).to_string())
            }
            "qualified_identifier" | "template_function" => {
                // Last named child holds the name.
                for i in (0..n.named_child_count()).rev() {
                    if let Some(c) = n.named_child(i)
                        && let Some(name) = self.declarator_name(c, s)
                    {
                        return Some(name);
                    }
                }
                None
            }
            "function_declarator"
            | "pointer_declarator"
            | "reference_declarator"
            | "parenthesized_declarator" => {
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

    fn function_name<'a>(&self, n: Node<'a>, s: &'a [u8]) -> Option<String> {
        for i in 0..n.named_child_count() {
            let c = n.named_child(i)?;
            match c.kind() {
                "function_declarator" | "pointer_declarator" | "reference_declarator" => {
                    if let Some(name) = self.declarator_name(c, s) {
                        return Some(name);
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn handle_function(&self, n: Node, s: &[u8], symbols: &mut Vec<Symbol>) {
        let Some(name) = self.function_name(n, s) else {
            return;
        };
        // Out-of-line method definition (`void Foo::bar() {}`)
        // carries the receiving class via a qualified_identifier
        // in the declarator. Detect that and emit a Method with
        // the qualifier as parent_class so users can find these
        // alongside in-class methods via list_symbols --parent Foo.
        let parent = self.qualified_owner(n, s);
        let kind = if parent.is_some() {
            SymbolKind::Method
        } else {
            SymbolKind::Function
        };
        symbols.push(Symbol {
            kind,
            // C++ top-level functions are externally visible
            // unless they're in an anonymous namespace or marked
            // `static`. We don't recurse into anonymous namespace
            // visibility tracking — treat as exported.
            is_exported: true,
            name,
            range: self.range(n),
            signature: self.signature(n, s),
            parent_class: parent,
        });
    }

    /// If the function's declarator is a `qualified_identifier`
    /// (`Foo::bar` / `ns::Foo::bar`), return everything before the
    /// last `::` as the parent_class. Returns None for plain
    /// non-qualified function definitions.
    fn qualified_owner(&self, n: Node, s: &[u8]) -> Option<String> {
        for i in 0..n.named_child_count() {
            let Some(decl) = n.named_child(i) else {
                continue;
            };
            // Walk through the wrappers; we want the actual
            // function_declarator's name child.
            let inner = if matches!(
                decl.kind(),
                "function_declarator" | "pointer_declarator" | "reference_declarator"
            ) {
                // function_declarator's `declarator` field holds the name.
                decl.child_by_field_name("declarator").unwrap_or(decl)
            } else {
                continue;
            };
            if inner.kind() != "qualified_identifier" {
                continue;
            }
            // Take all text before the trailing `::name`. tree-sitter
            // exposes scope vs name fields, but we use raw text since
            // it round-trips namespaces correctly.
            let raw = self.text(inner, s);
            if let Some((scope, _name)) = raw.rsplit_once("::") {
                return Some(scope.to_string());
            }
        }
        None
    }

    /// Walk a `field_declaration_list` (the body of `class`/`struct`)
    /// collecting method declarations + inline definitions. C++
    /// access (`public:` / `private:` / `protected:`) labels are
    /// sibling nodes that change subsequent items' visibility; we
    /// track the running label so each method gets the right
    /// is_exported.
    fn walk_class_body(
        &self,
        body: Node,
        s: &[u8],
        symbols: &mut Vec<Symbol>,
        parent: &str,
        default_public: bool,
    ) {
        let mut public = default_public;
        for i in 0..body.named_child_count() {
            let Some(c) = body.named_child(i) else {
                continue;
            };
            match c.kind() {
                "access_specifier" => {
                    public = self.text(c, s).trim_end_matches(':') == "public";
                }
                "function_definition" | "declaration" => {
                    // Field name is reachable via the same
                    // declarator walk used for top-level functions.
                    if let Some(name) = self.function_name(c, s) {
                        symbols.push(Symbol {
                            kind: SymbolKind::Method,
                            is_exported: public,
                            name,
                            range: self.range(c),
                            signature: self.signature(c, s),
                            parent_class: Some(parent.to_string()),
                        });
                    }
                }
                "field_declaration" => {
                    // A field declaration may carry a method
                    // declarator (e.g. `virtual void f() = 0;`),
                    // a plain field, or a constructor proto.
                    if let Some(name) = self.function_name(c, s) {
                        symbols.push(Symbol {
                            kind: SymbolKind::Method,
                            is_exported: public,
                            name,
                            range: self.range(c),
                            signature: self.text(c, s).lines().next().unwrap_or("").to_string(),
                            parent_class: Some(parent.to_string()),
                        });
                    }
                }
                "class_specifier" | "struct_specifier" => {
                    // Nested type — recurse.
                    self.handle_class_or_struct(c, s, symbols, c.kind() == "struct_specifier");
                }
                _ => {}
            }
        }
    }

    fn class_or_struct_name<'a>(&self, n: Node<'a>, s: &'a [u8]) -> Option<String> {
        for i in 0..n.named_child_count() {
            if let Some(c) = n.named_child(i)
                && c.kind() == "type_identifier"
            {
                return Some(self.text(c, s).to_string());
            }
        }
        None
    }

    fn handle_class_or_struct(
        &self,
        n: Node,
        s: &[u8],
        symbols: &mut Vec<Symbol>,
        is_struct: bool,
    ) {
        let Some(name) = self.class_or_struct_name(n, s) else {
            return;
        };
        symbols.push(Symbol {
            kind: SymbolKind::Class,
            is_exported: true,
            name: name.clone(),
            range: self.range(n),
            signature: self.text(n, s).lines().next().unwrap_or("").to_string(),
            parent_class: None,
        });
        for i in 0..n.named_child_count() {
            if let Some(c) = n.named_child(i)
                && c.kind() == "field_declaration_list"
            {
                self.walk_class_body(c, s, symbols, &name, is_struct);
            }
        }
    }

    /// `namespace ns { ... }` — recurse into its children at the
    /// top-level walker. Symbols defined inside don't carry the
    /// namespace prefix structurally, but their signature line
    /// includes the `namespace ns {` context which is good enough
    /// for the LLM to disambiguate.
    fn handle_namespace(
        &self,
        n: Node,
        s: &[u8],
        symbols: &mut Vec<Symbol>,
        imports: &mut Vec<Import>,
    ) {
        for i in 0..n.named_child_count() {
            if let Some(c) = n.named_child(i)
                && c.kind() == "declaration_list"
            {
                for j in 0..c.named_child_count() {
                    let Some(inner) = c.named_child(j) else {
                        continue;
                    };
                    self.dispatch_top(inner, s, symbols, imports);
                }
            }
        }
    }

    fn handle_using(&self, n: Node, s: &[u8], imports: &mut Vec<Import>) {
        // `using std::cout;` or `using namespace std;` — both carry
        // an identifier/scoped name we can record.
        for i in 0..n.named_child_count() {
            if let Some(c) = n.named_child(i)
                && matches!(c.kind(), "qualified_identifier" | "identifier")
            {
                let path = self.text(c, s).to_string();
                imports.push(Import {
                    names: vec![path.clone()],
                    source: path,
                });
                return;
            }
        }
    }

    fn handle_include(&self, n: Node, s: &[u8], imports: &mut Vec<Import>) {
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

    fn dispatch_top(
        &self,
        c: Node,
        bytes: &[u8],
        symbols: &mut Vec<Symbol>,
        imports: &mut Vec<Import>,
    ) {
        match c.kind() {
            "function_definition" => self.handle_function(c, bytes, symbols),
            "class_specifier" => self.handle_class_or_struct(c, bytes, symbols, false),
            "struct_specifier" => self.handle_class_or_struct(c, bytes, symbols, true),
            "namespace_definition" => self.handle_namespace(c, bytes, symbols, imports),
            "template_declaration" => {
                // Unwrap to the inner function or class definition.
                for i in 0..c.named_child_count() {
                    if let Some(inner) = c.named_child(i) {
                        self.dispatch_top(inner, bytes, symbols, imports);
                    }
                }
            }
            "preproc_include" => self.handle_include(c, bytes, imports),
            "using_declaration" | "using_directive" => self.handle_using(c, bytes, imports),
            // `extern "C" { ... }` blocks. tree-sitter-cpp exposes
            // them as `linkage_specification`. Walk the inner
            // declaration_list as if it were the top level so FFI
            // functions show up in list_symbols.
            "linkage_specification" => {
                for i in 0..c.named_child_count() {
                    if let Some(inner) = c.named_child(i)
                        && inner.kind() == "declaration_list"
                    {
                        for j in 0..inner.named_child_count() {
                            if let Some(item) = inner.named_child(j) {
                                self.dispatch_top(item, bytes, symbols, imports);
                            }
                        }
                    } else if let Some(inner) = c.named_child(i) {
                        // Single-statement extern (no braces).
                        self.dispatch_top(inner, bytes, symbols, imports);
                    }
                }
            }
            _ => {}
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

impl LanguageAdapter for CppAdapter {
    fn extensions(&self) -> &[&str] {
        // C++ source and header conventions. We claim `.h` headers
        // when the build is C++ — but the C adapter also claims
        // `.h`. The registry's `find_for_file` returns the first
        // match, and adapters are inserted in alphabetical order in
        // `SemanticManager` so C wins for `.h`. If a project is
        // primarily C++ users can flip the file extension to
        // `.hpp` / `.hh` / `.hxx` to route through here.
        &[".cpp", ".cc", ".cxx", ".hpp", ".hh", ".hxx"]
    }

    fn extract(&self, file_path: &Path, source: &str) -> Result<ExtractedFile, String> {
        let lang: tree_sitter::Language = tree_sitter_cpp::LANGUAGE.into();
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
            if let Some(c) = root.named_child(i) {
                self.dispatch_top(c, bytes, &mut symbols, &mut imports);
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
        let lang: tree_sitter::Language = tree_sitter_cpp::LANGUAGE.into();
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

        // Direct calls + member calls. Constructor calls
        // (`new Foo()`) show up as `new_expression` with a
        // `type_identifier`; capture that too.
        let query_str = r#"
            (call_expression function: (identifier) @callee)
            (call_expression function: (field_expression field: (field_identifier) @callee))
            (call_expression function: (qualified_identifier name: (identifier) @callee))
            (new_expression type: (type_identifier) @callee)
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
    fn extracts_top_level_function() {
        let src = "int add(int a, int b) { return a + b; }\n";
        let f = CppAdapter.extract(&pb("a.cpp"), src).unwrap();
        let add = f.symbols.iter().find(|s| s.name == "add").unwrap();
        assert!(matches!(add.kind, SymbolKind::Function));
    }

    #[test]
    fn extracts_class_with_public_private_visibility() {
        let src = "class App {\npublic:\n  void hello() {}\nprivate:\n  void helper() {}\n};\n";
        let f = CppAdapter.extract(&pb("a.cpp"), src).unwrap();
        let app = f.symbols.iter().find(|s| s.name == "App").unwrap();
        assert!(matches!(app.kind, SymbolKind::Class));
        let hello = f.symbols.iter().find(|s| s.name == "hello").unwrap();
        assert!(hello.is_exported, "hello is under public:");
        let helper = f.symbols.iter().find(|s| s.name == "helper").unwrap();
        assert!(!helper.is_exported, "helper is under private:");
        assert_eq!(hello.parent_class.as_deref(), Some("App"));
    }

    #[test]
    fn extracts_struct_default_public() {
        // `struct` in C++ defaults to PUBLIC visibility.
        let src = "struct V { int x; void show() {} };\n";
        let f = CppAdapter.extract(&pb("a.cpp"), src).unwrap();
        let v = f.symbols.iter().find(|s| s.name == "V").unwrap();
        assert!(matches!(v.kind, SymbolKind::Class));
        let show = f.symbols.iter().find(|s| s.name == "show").unwrap();
        assert!(show.is_exported, "struct member is public by default");
    }

    #[test]
    fn extracts_namespaced_class() {
        let src = "namespace ns {\nclass Inner { public: void m() {} };\n}\n";
        let f = CppAdapter.extract(&pb("a.cpp"), src).unwrap();
        // Inner appears, walked through the namespace.
        assert!(f.symbols.iter().any(|s| s.name == "Inner"));
        let m = f.symbols.iter().find(|s| s.name == "m").unwrap();
        assert_eq!(m.parent_class.as_deref(), Some("Inner"));
    }

    #[test]
    fn extracts_template_function() {
        let src = "template<typename T> T identity(T x) { return x; }\n";
        let f = CppAdapter.extract(&pb("a.cpp"), src).unwrap();
        assert!(
            f.symbols
                .iter()
                .any(|s| s.name == "identity" && matches!(s.kind, SymbolKind::Function))
        );
    }

    #[test]
    fn extracts_includes() {
        let src = "#include <string>\n#include \"local.hpp\"\n";
        let f = CppAdapter.extract(&pb("a.cpp"), src).unwrap();
        assert!(f.imports.iter().any(|i| i.source == "string"));
        assert!(f.imports.iter().any(|i| i.source == "local.hpp"));
    }

    #[test]
    fn find_callees_captures_direct_member_qualified_and_new() {
        let src = "void run() { helper(); foo.bar(); std::cout(); auto p = new Foo(); }\nvoid helper() {}\n";
        let f = CppAdapter.extract(&pb("a.cpp"), src).unwrap();
        let run = f.symbols.iter().find(|s| s.name == "run").unwrap();
        let callees = CppAdapter
            .find_callees_in_range(src, &pb("a.cpp"), run.range)
            .unwrap();
        assert!(callees.contains(&"helper".to_string()));
        assert!(callees.contains(&"bar".to_string()));
        assert!(callees.contains(&"Foo".to_string()));
    }

    /// `extern "C" { ... }` blocks — previously dropped entirely.
    #[test]
    fn extracts_extern_c_block_contents() {
        let src = "extern \"C\" {\n  int c_function(int x);\n  void c_void(void);\n}\nint c_function(int x) { return x; }\nvoid c_void(void) {}\n";
        let f = CppAdapter.extract(&pb("a.cpp"), src).unwrap();
        assert!(f.symbols.iter().any(|s| s.name == "c_function"));
        assert!(f.symbols.iter().any(|s| s.name == "c_void"));
    }

    /// Out-of-line method definitions `void Foo::bar() {}` attach
    /// to the qualifying class via parent_class.
    #[test]
    fn out_of_line_method_attaches_to_qualifying_class() {
        let src = "class Foo { public: void bar(); };\nvoid Foo::bar() {}\n";
        let f = CppAdapter.extract(&pb("a.cpp"), src).unwrap();
        // There are two `bar` symbols: the declaration inside Foo,
        // and the out-of-line definition. Both should be Methods
        // with parent_class = "Foo".
        let bars: Vec<_> = f.symbols.iter().filter(|s| s.name == "bar").collect();
        assert!(!bars.is_empty(), "no bar symbols");
        assert!(
            bars.iter().all(|s| matches!(s.kind, SymbolKind::Method)),
            "every bar should be a Method; got {:?}",
            bars.iter()
                .map(|s| (s.kind, s.parent_class.clone()))
                .collect::<Vec<_>>(),
        );
        assert!(
            bars.iter()
                .all(|s| s.parent_class.as_deref() == Some("Foo")),
            "every bar's parent_class should be Foo",
        );
    }
}
