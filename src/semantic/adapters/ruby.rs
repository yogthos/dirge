use std::path::Path;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

use crate::semantic::adapter::LanguageAdapter;
use crate::semantic::common::{find_node_at_range, node_text, signature_first_line};
use crate::semantic::types::{ByteRange, ExtractedFile, Import, ImportKind, Symbol, SymbolKind};

/// Tree-sitter adapter for Ruby.
///
/// Ruby visibility (`public` / `private` / `protected`) is set by
/// keyword inside a class body, not at the method declaration site.
/// Without scanning the surrounding token stream we can't tell which
/// methods are private, so every method is reported as exported.
/// Users who want privacy-aware analysis should rely on LSP
/// (solargraph) for that signal.
///
/// `module` becomes `SymbolKind::Interface` since Ruby modules are
/// closest in spirit to an interface/mixin contract; `class`
/// becomes `SymbolKind::Class`; `def` becomes Function at top
/// level and Method inside a class/module.
pub struct RubyAdapter;

impl RubyAdapter {
    fn text<'a>(&self, n: Node<'a>, s: &'a [u8]) -> &'a str {
        node_text(n, s)
    }
    fn range(&self, n: Node) -> ByteRange {
        ByteRange::from(n)
    }
    fn signature(&self, n: Node, s: &[u8]) -> String {
        signature_first_line(n, s)
    }

    fn method_name<'a>(&self, n: Node<'a>, s: &'a [u8]) -> Option<String> {
        // `def name(args); body; end` — the name child is an
        // `identifier` (instance method) or `constant` / scope-qualified
        // for class methods (`def self.foo`).
        for i in 0..n.named_child_count() {
            let c = n.named_child(i)?;
            match c.kind() {
                "identifier" | "constant" | "operator" => return Some(self.text(c, s).to_string()),
                _ => {}
            }
        }
        None
    }

    fn class_or_module_name<'a>(&self, n: Node<'a>, s: &'a [u8]) -> Option<String> {
        // First `constant` child is the class/module name. (Ruby
        // allows reopening qualified names like `Foo::Bar` — we
        // surface the leaf.)
        for i in 0..n.named_child_count() {
            let c = n.named_child(i)?;
            if c.kind() == "constant" {
                return Some(self.text(c, s).to_string());
            }
            if c.kind() == "scope_resolution" {
                // Reach the rightmost `constant` inside.
                let mut last: Option<&str> = None;
                for j in 0..c.named_child_count() {
                    if let Some(sub) = c.named_child(j)
                        && sub.kind() == "constant"
                    {
                        last = Some(self.text(sub, s));
                    }
                }
                return last.map(str::to_string);
            }
        }
        None
    }

    /// Walk the body of a class/module and emit Method symbols
    /// anchored to `parent`. Tracks visibility state across the
    /// walk: a bare `private` / `protected` / `public` statement
    /// flips the visibility for subsequent `def`s in this scope,
    /// matching Ruby semantics (audit L4 — every method was
    /// silently emitted as `is_exported: true` regardless).
    fn walk_class_body(&self, n: Node, s: &[u8], symbols: &mut Vec<Symbol>, parent: &str) {
        // Current visibility for the next method. `true` = public,
        // `false` = non-public (private OR protected — we collapse
        // both to "not exported" because dirge's Symbol shape only
        // has a binary is_exported flag).
        let mut visibility_public = true;
        // The body is `body_statement`; iterate its named children
        // and pick up `method` and `singleton_method` (class methods).
        for i in 0..n.named_child_count() {
            let Some(c) = n.named_child(i) else { continue };
            // Detect visibility-toggle statements before per-kind
            // handling. tree-sitter-ruby parses a bare `private` as
            // an `identifier` node; `private :foo` and `private def`
            // are `call` nodes — we ignore those (they only toggle
            // the named target, not subsequent defs).
            if c.kind() == "identifier" {
                let txt = self.text(c, s);
                match txt {
                    "public" => {
                        visibility_public = true;
                        continue;
                    }
                    "private" | "protected" => {
                        visibility_public = false;
                        continue;
                    }
                    _ => {}
                }
            }
            match c.kind() {
                "method" => {
                    if let Some(name) = self.method_name(c, s) {
                        symbols.push(Symbol {
                            kind: SymbolKind::Method,
                            name,
                            range: self.range(c),
                            signature: self.signature(c, s),
                            is_exported: visibility_public,
                            parent_class: Some(parent.to_string()),
                        });
                    }
                }
                "singleton_method" => {
                    // `def self.foo` — method name is in the
                    // `identifier` child after the `self` / receiver.
                    let mut name: Option<String> = None;
                    for j in 0..c.named_child_count() {
                        if let Some(sub) = c.named_child(j)
                            && sub.kind() == "identifier"
                        {
                            name = Some(self.text(sub, s).to_string());
                            break;
                        }
                    }
                    if let Some(name) = name {
                        symbols.push(Symbol {
                            kind: SymbolKind::Method,
                            name,
                            range: self.range(c),
                            signature: self.signature(c, s),
                            // Class methods (`def self.foo`) don't
                            // participate in instance-method
                            // private/protected visibility — they're
                            // always public unless declared via
                            // `private_class_method`. Keep as public.
                            is_exported: true,
                            parent_class: Some(parent.to_string()),
                        });
                    }
                }
                // Nested class / module — recurse.
                "class" | "module" => self.walk_top(c, s, symbols, Some(parent.to_string())),
                // `class << self ... end` — singleton class block.
                // Methods defined inside are class methods on the
                // enclosing class. tree-sitter exposes the block as
                // `singleton_class`; walk its body and emit each
                // `method` as a Method on `parent`. Without this,
                // the entire `class << self` block of methods was
                // invisible to list_symbols.
                "singleton_class" => {
                    for j in 0..c.named_child_count() {
                        if let Some(body) = c.named_child(j)
                            && body.kind() == "body_statement"
                        {
                            self.walk_class_body(body, s, symbols, parent);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Top-level walker, also used for nested class/module
    /// recursion. When `inside_parent` is `Some`, top-level `def`s
    /// inside a class body still become Methods.
    fn walk_top(
        &self,
        n: Node,
        s: &[u8],
        symbols: &mut Vec<Symbol>,
        inside_parent: Option<String>,
    ) {
        match n.kind() {
            "class" => {
                let Some(name) = self.class_or_module_name(n, s) else {
                    return;
                };
                symbols.push(Symbol {
                    kind: SymbolKind::Class,
                    name: name.clone(),
                    range: self.range(n),
                    signature: self.signature(n, s),
                    is_exported: true,
                    parent_class: inside_parent.clone(),
                });
                // Body statement is the children container.
                for i in 0..n.named_child_count() {
                    if let Some(body) = n.named_child(i)
                        && body.kind() == "body_statement"
                    {
                        self.walk_class_body(body, s, symbols, &name);
                    }
                }
            }
            "module" => {
                let Some(name) = self.class_or_module_name(n, s) else {
                    return;
                };
                symbols.push(Symbol {
                    kind: SymbolKind::Interface,
                    name: name.clone(),
                    range: self.range(n),
                    signature: self.signature(n, s),
                    is_exported: true,
                    parent_class: inside_parent.clone(),
                });
                for i in 0..n.named_child_count() {
                    if let Some(body) = n.named_child(i)
                        && body.kind() == "body_statement"
                    {
                        self.walk_class_body(body, s, symbols, &name);
                    }
                }
            }
            "method" => {
                if let Some(name) = self.method_name(n, s) {
                    symbols.push(Symbol {
                        kind: SymbolKind::Function,
                        name,
                        range: self.range(n),
                        signature: self.signature(n, s),
                        is_exported: true,
                        parent_class: None,
                    });
                }
            }
            _ => {}
        }
    }

    /// `require '<name>'` and `require_relative '<name>'` — surfaced
    /// as imports.
    fn maybe_import(&self, n: Node, s: &[u8], imports: &mut Vec<Import>) {
        if n.kind() != "call" {
            return;
        }
        // First child is the called identifier.
        let Some(id) = n.named_child(0).filter(|c| c.kind() == "identifier") else {
            return;
        };
        let id_text = self.text(id, s);
        if !matches!(id_text, "require" | "require_relative" | "load") {
            return;
        }
        // Find the argument_list → string → string_content.
        for i in 0..n.named_child_count() {
            let Some(c) = n.named_child(i) else { continue };
            if c.kind() != "argument_list" {
                continue;
            }
            for j in 0..c.named_child_count() {
                if let Some(arg) = c.named_child(j)
                    && arg.kind() == "string"
                {
                    let raw = self.text(arg, s);
                    let path = raw
                        .trim_matches(|c: char| c == '"' || c == '\'')
                        .to_string();
                    imports.push(Import {
                        names: vec![path.clone()],
                        source: path,
                        kind: ImportKind::Module,
                    });
                }
            }
        }
    }
}

impl LanguageAdapter for RubyAdapter {
    fn extensions(&self) -> &[&str] {
        &[".rb", ".rake", ".gemspec"]
    }

    fn extract(&self, file_path: &Path, source: &str) -> Result<ExtractedFile, String> {
        let lang: tree_sitter::Language = tree_sitter_ruby::LANGUAGE.into();
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
            self.walk_top(c, bytes, &mut symbols, None);
            self.maybe_import(c, bytes, &mut imports);
        }

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
        let lang: tree_sitter::Language = tree_sitter_ruby::LANGUAGE.into();
        let mut parser = Parser::new();
        parser
            .set_language(&lang)
            .map_err(|e| format!("Failed to set language: {e}"))?;
        let tree = parser.parse(source, None).ok_or("Failed to parse source")?;
        let root = tree.root_node();
        let bytes = source.as_bytes();

        let target = find_node_at_range(root, range.start_byte, range.end_byte)
            .ok_or("Could not find node at given range")?;

        // `(call method: (identifier) @callee)` catches `obj.foo`
        // and bare `foo()`. Identifiers in non-call position are
        // skipped (otherwise we'd flag every variable reference).
        let query_str = "(call method: (identifier) @callee)";
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
    fn extracts_class_with_methods() {
        let src = "class User\n  def initialize(name)\n    @name = name\n  end\n  def greet\n    puts @name\n  end\nend\n";
        let f = RubyAdapter.extract(&pb("a.rb"), src).unwrap();
        let cls = f.symbols.iter().find(|s| s.name == "User").unwrap();
        assert!(matches!(cls.kind, SymbolKind::Class));
        let init = f.symbols.iter().find(|s| s.name == "initialize").unwrap();
        assert!(matches!(init.kind, SymbolKind::Method));
        assert_eq!(init.parent_class.as_deref(), Some("User"));
        assert!(f.symbols.iter().any(|s| s.name == "greet"));
    }

    #[test]
    fn extracts_module_as_interface() {
        let src = "module Greetable\n  def hello; end\nend\n";
        let f = RubyAdapter.extract(&pb("a.rb"), src).unwrap();
        let m = f.symbols.iter().find(|s| s.name == "Greetable").unwrap();
        assert!(matches!(m.kind, SymbolKind::Interface));
        let hello = f.symbols.iter().find(|s| s.name == "hello").unwrap();
        assert_eq!(hello.parent_class.as_deref(), Some("Greetable"));
    }

    #[test]
    fn extracts_top_level_def_as_function() {
        let src = "def helper(x)\n  x + 1\nend\n";
        let f = RubyAdapter.extract(&pb("a.rb"), src).unwrap();
        let h = f.symbols.iter().find(|s| s.name == "helper").unwrap();
        assert!(matches!(h.kind, SymbolKind::Function));
        assert!(h.parent_class.is_none());
    }

    #[test]
    fn extracts_require_imports() {
        let src = "require 'json'\nrequire_relative './helper'\n";
        let f = RubyAdapter.extract(&pb("a.rb"), src).unwrap();
        assert!(f.imports.iter().any(|i| i.source == "json"));
        assert!(f.imports.iter().any(|i| i.source == "./helper"));
    }

    #[test]
    fn find_callees_for_method_body() {
        let src = "class A\n  def run\n    puts(\"x\")\n    helper()\n  end\nend\n";
        let f = RubyAdapter.extract(&pb("a.rb"), src).unwrap();
        let run = f.symbols.iter().find(|s| s.name == "run").unwrap();
        let callees = RubyAdapter
            .find_callees_in_range(src, &pb("a.rb"), run.range)
            .unwrap();
        assert!(callees.contains(&"puts".to_string()));
        assert!(callees.contains(&"helper".to_string()));
    }

    /// `class << self` singleton class blocks define class methods.
    /// Previously the entire block's methods were invisible.
    #[test]
    fn extracts_singleton_class_methods_as_class_methods() {
        let src = "class A\n  class << self\n    def factory; new; end\n    def banner; puts \"hi\"; end\n  end\nend\n";
        let f = RubyAdapter.extract(&pb("a.rb"), src).unwrap();
        let factory = f.symbols.iter().find(|s| s.name == "factory").unwrap();
        let banner = f.symbols.iter().find(|s| s.name == "banner").unwrap();
        assert!(matches!(factory.kind, SymbolKind::Method));
        assert_eq!(factory.parent_class.as_deref(), Some("A"));
        assert_eq!(banner.parent_class.as_deref(), Some("A"));
    }
}
