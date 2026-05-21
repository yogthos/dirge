use std::path::Path;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

use crate::semantic::adapter::LanguageAdapter;
use crate::semantic::types::{ByteRange, ExtractedFile, Import, Symbol, SymbolKind};

/// Tree-sitter adapter for Java.
///
/// Visibility: explicit `public` modifier → exported. Anything else
/// (no modifier = package-private, plus `private` / `protected`)
/// stays non-exported. The package + import system is implicit so
/// we don't try to model package-private visibility separately.
///
/// Class methods get `parent_class = <containing class name>`;
/// interface methods get the interface name. Nested classes are
/// walked recursively.
pub struct JavaAdapter;

impl JavaAdapter {
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
        // Use prefix up to the body (block / class_body / interface_body)
        // when available; otherwise first line.
        for field in ["body"] {
            if let Some(body) = n.child_by_field_name(field) {
                return String::from_utf8_lossy(&s[n.start_byte()..body.start_byte()])
                    .trim()
                    .to_string();
            }
        }
        // Fall back to walking children for any *_body node.
        for i in 0..n.named_child_count() {
            if let Some(c) = n.named_child(i)
                && (c.kind().ends_with("_body") || c.kind() == "block")
            {
                return String::from_utf8_lossy(&s[n.start_byte()..c.start_byte()])
                    .trim()
                    .to_string();
            }
        }
        let first = self.text(n, s).lines().next().unwrap_or("");
        if first.chars().count() > 80 {
            let p: String = first.chars().take(80).collect();
            format!("{p}…")
        } else {
            first.to_string()
        }
    }

    /// Examines the `modifiers` child (if any) for the literal token
    /// `public`. Java tree-sitter exposes modifiers as a sibling node
    /// whose CHILDREN are the modifier tokens, so we use the raw
    /// text of the modifiers node and substring-match.
    fn is_public(&self, n: Node, s: &[u8]) -> bool {
        for i in 0..n.named_child_count() {
            if let Some(c) = n.named_child(i)
                && c.kind() == "modifiers"
            {
                let text = self.text(c, s);
                return text.split_whitespace().any(|t| t == "public");
            }
        }
        false
    }

    fn ident_name<'a>(&self, n: Node<'a>, s: &'a [u8]) -> Option<String> {
        for i in 0..n.named_child_count() {
            let c = n.named_child(i)?;
            if c.kind() == "identifier" {
                return Some(self.text(c, s).to_string());
            }
        }
        None
    }

    /// Walk a `class_body` / `interface_body` / `enum_body` and
    /// emit child symbols (methods, fields, nested classes).
    fn walk_class_body(&self, body: Node, s: &[u8], symbols: &mut Vec<Symbol>, parent: &str) {
        for i in 0..body.named_child_count() {
            let Some(c) = body.named_child(i) else {
                continue;
            };
            match c.kind() {
                "method_declaration" => {
                    if let Some(name) = self.ident_name(c, s) {
                        symbols.push(Symbol {
                            kind: SymbolKind::Method,
                            is_exported: self.is_public(c, s),
                            name,
                            range: self.range(c),
                            signature: self.signature(c, s),
                            parent_class: Some(parent.to_string()),
                        });
                    }
                }
                "constructor_declaration" => {
                    // Constructor name matches the class; emit as a
                    // Method so users can find it via list_symbols
                    // --kind method --parent <class>.
                    if let Some(name) = self.ident_name(c, s) {
                        symbols.push(Symbol {
                            kind: SymbolKind::Method,
                            is_exported: self.is_public(c, s),
                            name,
                            range: self.range(c),
                            signature: self.signature(c, s),
                            parent_class: Some(parent.to_string()),
                        });
                    }
                }
                "field_declaration" => {
                    // `field_declaration > variable_declarator >
                    // identifier`. There can be multiple vars per
                    // declaration (`int a, b, c;`); emit each.
                    let is_pub = self.is_public(c, s);
                    for j in 0..c.named_child_count() {
                        if let Some(decl) = c.named_child(j)
                            && decl.kind() == "variable_declarator"
                            && let Some(name_n) = decl.named_child(0)
                            && name_n.kind() == "identifier"
                        {
                            let name = self.text(name_n, s).to_string();
                            symbols.push(Symbol {
                                kind: SymbolKind::Variable,
                                is_exported: is_pub,
                                name,
                                range: self.range(c),
                                signature: self.text(c, s).lines().next().unwrap_or("").to_string(),
                                parent_class: Some(parent.to_string()),
                            });
                        }
                    }
                }
                "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration" => {
                    // Nested class — recurse from the top of the
                    // walker so the new class gets its own
                    // symbol + body walk.
                    self.walk_top(c, s, symbols);
                }
                _ => {}
            }
        }
    }

    fn walk_top(&self, n: Node, s: &[u8], symbols: &mut Vec<Symbol>) {
        match n.kind() {
            "class_declaration" => {
                let Some(name) = self.ident_name(n, s) else {
                    return;
                };
                let is_pub = self.is_public(n, s);
                symbols.push(Symbol {
                    kind: SymbolKind::Class,
                    is_exported: is_pub,
                    name: name.clone(),
                    range: self.range(n),
                    signature: self.signature(n, s),
                    parent_class: None,
                });
                if let Some(body) = n.child_by_field_name("body") {
                    self.walk_class_body(body, s, symbols, &name);
                }
            }
            "interface_declaration" => {
                let Some(name) = self.ident_name(n, s) else {
                    return;
                };
                let is_pub = self.is_public(n, s);
                symbols.push(Symbol {
                    kind: SymbolKind::Interface,
                    is_exported: is_pub,
                    name: name.clone(),
                    range: self.range(n),
                    signature: self.signature(n, s),
                    parent_class: None,
                });
                if let Some(body) = n.child_by_field_name("body") {
                    self.walk_class_body(body, s, symbols, &name);
                }
            }
            "enum_declaration" | "record_declaration" => {
                // Records (Java 16+) are immutable data carriers
                // that look like \`public record Point(int x, int y) {}\`.
                // tree-sitter exposes them as record_declaration; we
                // surface them as Class so list_symbols finds them,
                // and walk the body for any explicit methods. The
                // auto-generated accessors aren't AST-visible so we
                // can't list them, but the signature line preserves
                // the component list which is the useful info.
                let Some(name) = self.ident_name(n, s) else {
                    return;
                };
                let is_pub = self.is_public(n, s);
                symbols.push(Symbol {
                    kind: SymbolKind::Class,
                    is_exported: is_pub,
                    name: name.clone(),
                    range: self.range(n),
                    signature: self.signature(n, s),
                    parent_class: None,
                });
                if let Some(body) = n.child_by_field_name("body") {
                    self.walk_class_body(body, s, symbols, &name);
                }
            }
            _ => {}
        }
    }

    fn handle_import(&self, n: Node, s: &[u8], imports: &mut Vec<Import>) {
        // `import_declaration > scoped_identifier`. Static imports
        // and wildcards (`import java.util.*`) have similar shapes;
        // we just record the raw FQN text.
        for i in 0..n.named_child_count() {
            if let Some(c) = n.named_child(i)
                && matches!(c.kind(), "scoped_identifier" | "identifier")
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

impl LanguageAdapter for JavaAdapter {
    fn extensions(&self) -> &[&str] {
        &[".java"]
    }

    fn extract(&self, file_path: &Path, source: &str) -> Result<ExtractedFile, String> {
        let lang: tree_sitter::Language = tree_sitter_java::LANGUAGE.into();
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
                "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration" => {
                    self.walk_top(c, bytes, &mut symbols);
                }
                "import_declaration" => self.handle_import(c, bytes, &mut imports),
                _ => {}
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
        let lang: tree_sitter::Language = tree_sitter_java::LANGUAGE.into();
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

        // `method_invocation` covers both bare `foo()` and `obj.foo()`.
        // The method's NAME is the `name` field child of type
        // `identifier`. Object_creation_expression handles
        // `new Foo()` constructor calls.
        let query_str = r#"
            (method_invocation name: (identifier) @callee)
            (object_creation_expression type: (type_identifier) @callee)
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
    fn extracts_class_with_method_and_field_visibility() {
        let src = "public class App {\n  private int n;\n  public String name;\n  public void hello() {}\n  private void helper() {}\n}\n";
        let f = JavaAdapter.extract(&pb("a.java"), src).unwrap();
        let app = f.symbols.iter().find(|s| s.name == "App").unwrap();
        assert!(matches!(app.kind, SymbolKind::Class));
        assert!(app.is_exported);
        let hello = f.symbols.iter().find(|s| s.name == "hello").unwrap();
        assert!(hello.is_exported);
        assert_eq!(hello.parent_class.as_deref(), Some("App"));
        let helper = f.symbols.iter().find(|s| s.name == "helper").unwrap();
        assert!(!helper.is_exported);
        let n_field = f.symbols.iter().find(|s| s.name == "n").unwrap();
        assert!(matches!(n_field.kind, SymbolKind::Variable));
        assert!(!n_field.is_exported);
        let name_field = f.symbols.iter().find(|s| s.name == "name").unwrap();
        assert!(name_field.is_exported);
    }

    #[test]
    fn extracts_interface_with_methods() {
        let src = "public interface Greeter {\n  String greet(String n);\n  default String wave() { return \"o/\"; }\n}\n";
        let f = JavaAdapter.extract(&pb("a.java"), src).unwrap();
        let g = f.symbols.iter().find(|s| s.name == "Greeter").unwrap();
        assert!(matches!(g.kind, SymbolKind::Interface));
        let greet = f.symbols.iter().find(|s| s.name == "greet").unwrap();
        assert!(matches!(greet.kind, SymbolKind::Method));
        assert_eq!(greet.parent_class.as_deref(), Some("Greeter"));
        assert!(f.symbols.iter().any(|s| s.name == "wave"));
    }

    #[test]
    fn extracts_constructor_as_method() {
        let src = "public class App {\n  public App(String p) {}\n}\n";
        let f = JavaAdapter.extract(&pb("a.java"), src).unwrap();
        // Two `App` symbols: the class + the constructor. The
        // constructor's kind is Method.
        let methods: Vec<_> = f
            .symbols
            .iter()
            .filter(|s| s.name == "App" && matches!(s.kind, SymbolKind::Method))
            .collect();
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].parent_class.as_deref(), Some("App"));
    }

    #[test]
    fn extracts_enum_as_class() {
        let src = "enum Color { RED, GREEN }\n";
        let f = JavaAdapter.extract(&pb("a.java"), src).unwrap();
        let c = f.symbols.iter().find(|s| s.name == "Color").unwrap();
        assert!(matches!(c.kind, SymbolKind::Class));
    }

    #[test]
    fn extracts_imports() {
        let src = "import java.util.List;\nimport java.util.Map;\n";
        let f = JavaAdapter.extract(&pb("a.java"), src).unwrap();
        assert!(f.imports.iter().any(|i| i.source == "java.util.List"));
        assert!(f.imports.iter().any(|i| i.source == "java.util.Map"));
    }

    #[test]
    fn find_callees_method_and_constructor() {
        let src = "public class App {\n  public void run() {\n    helper();\n    System.out.println(\"x\");\n    Foo f = new Foo();\n  }\n  private void helper() {}\n}\n";
        let f = JavaAdapter.extract(&pb("a.java"), src).unwrap();
        let run = f.symbols.iter().find(|s| s.name == "run").unwrap();
        let callees = JavaAdapter
            .find_callees_in_range(src, &pb("a.java"), run.range)
            .unwrap();
        assert!(callees.contains(&"helper".to_string()));
        assert!(callees.contains(&"println".to_string()));
        assert!(callees.contains(&"Foo".to_string()));
    }

    #[test]
    fn nested_class_is_recursively_walked() {
        let src =
            "public class Outer {\n  public class Inner {\n    public void deep() {}\n  }\n}\n";
        let f = JavaAdapter.extract(&pb("a.java"), src).unwrap();
        assert!(
            f.symbols
                .iter()
                .any(|s| s.name == "Inner" && matches!(s.kind, SymbolKind::Class))
        );
        let deep = f.symbols.iter().find(|s| s.name == "deep").unwrap();
        assert_eq!(deep.parent_class.as_deref(), Some("Inner"));
    }

    /// Records (Java 16+) — previously silently dropped.
    #[test]
    fn extracts_record_declaration_as_class() {
        let src = "public record Point(int x, int y) {\n  public boolean isOrigin() { return x == 0 && y == 0; }\n}\n";
        let f = JavaAdapter.extract(&pb("a.java"), src).unwrap();
        let point = f.symbols.iter().find(|s| s.name == "Point").unwrap();
        assert!(matches!(point.kind, SymbolKind::Class));
        assert!(point.is_exported);
        let m = f.symbols.iter().find(|s| s.name == "isOrigin").unwrap();
        assert_eq!(m.parent_class.as_deref(), Some("Point"));
    }
}
