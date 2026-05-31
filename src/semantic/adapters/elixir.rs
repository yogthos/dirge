use std::path::Path;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

use crate::semantic::adapter::LanguageAdapter;
use crate::semantic::common::{find_node_at_range, node_text, signature_first_line};
use crate::semantic::types::{ByteRange, ExtractedFile, Import, ImportKind, Symbol, SymbolKind};

/// Tree-sitter adapter for Elixir.
///
/// Elixir's syntax is macro-based — `defmodule`, `def`, `defp`, etc.
/// are parsed as generic `call` nodes. We recognise definition
/// forms by matching the call's target `identifier` against known
/// Elixir kernel macros.
///
/// Visibility in Elixir: `def`/`defmacro`/`defguard` are public,
/// `defp`/`defmacrop`/`defguardp` are private.
pub struct ElixirAdapter;

impl ElixirAdapter {
    fn signature(&self, n: Node, s: &[u8]) -> String {
        signature_first_line(n, s)
    }

    /// Definition keywords recognised as module/function forms.
    const MODULE_DEFS: &[&str] = &["defmodule", "defprotocol", "defimpl"];
    const FUNCTION_DEFS: &[&str] = &[
        "def",
        "defp",
        "defmacro",
        "defmacrop",
        "defguard",
        "defguardp",
        "defdelegate",
    ];

    /// If `call` is a definition form, return the keyword.
    fn definition_kw<'a>(&self, call: Node<'a>, s: &'a [u8]) -> Option<&'a str> {
        let target = call.child_by_field_name("target")?;
        if target.kind() != "identifier" {
            return None;
        }
        let kw = node_text(target, s);
        (Self::MODULE_DEFS.contains(&kw) || Self::FUNCTION_DEFS.contains(&kw)).then_some(kw)
    }

    /// Get the `arguments` child of a call, if any.
    fn args<'a>(&self, call: Node<'a>) -> Option<Node<'a>> {
        for i in 0..call.named_child_count() {
            let c = call.named_child(i)?;
            if c.kind() == "arguments" {
                return Some(c);
            }
        }
        None
    }

    /// Get the `do_block` child of a call, if any.
    fn do_block<'a>(&self, call: Node<'a>) -> Option<Node<'a>> {
        for i in 0..call.named_child_count() {
            let c = call.named_child(i)?;
            if c.kind() == "do_block" {
                return Some(c);
            }
        }
        None
    }

    /// Extract the module name from a `defmodule` / `defprotocol` /
    /// `defimpl` call. The first `alias` (or `dot` for dotted names)
    /// inside `arguments` is the module name.
    fn module_name<'a>(&self, call: Node<'a>, s: &'a [u8]) -> Option<String> {
        let args = self.args(call)?;
        for i in 0..args.named_child_count() {
            let Some(c) = args.named_child(i) else {
                continue;
            };
            match c.kind() {
                "alias" => return Some(node_text(c, s).to_string()),
                "dot" => {
                    // Dotted module name: `MyApp.User`
                    if let Some(left) = c.child_by_field_name("left") {
                        let right = c
                            .child_by_field_name("right")
                            .map(|n| node_text(n, s))
                            .unwrap_or("");
                        return Some(format!("{}.{right}", node_text(left, s)));
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Extract the function name from a `def` / `defp` / etc. call.
    /// The function name is inside `arguments` — it can be:
    /// - `arguments → identifier "greet"` (no-arg: `def greet, do: ...`)
    /// - `arguments → call → identifier "greet"` (with args: `def greet(name)`)
    fn function_name<'a>(&self, call: Node<'a>, s: &'a [u8]) -> Option<String> {
        let args = self.args(call)?;
        for i in 0..args.named_child_count() {
            let Some(c) = args.named_child(i) else {
                continue;
            };
            match c.kind() {
                "identifier" => return Some(node_text(c, s).to_string()),
                "call" => {
                    // `greet(name)` — the inner call's target is the name.
                    if let Some(target) = c.child_by_field_name("target")
                        && target.kind() == "identifier"
                    {
                        return Some(node_text(target, s).to_string());
                    }
                }
                _ => continue,
            }
        }
        None
    }

    /// Walk the children of a `do_block` for nested definitions
    /// and import statements.
    fn walk_do_block(
        &self,
        block: Node,
        s: &[u8],
        symbols: &mut Vec<Symbol>,
        imports: &mut Vec<Import>,
        parent: &str,
    ) {
        for i in 0..block.named_child_count() {
            let Some(c) = block.named_child(i) else {
                continue;
            };
            if c.kind() == "call" && !self.walk_call(c, s, symbols, imports, Some(parent)) {
                self.maybe_import(c, s, imports);
            }
        }
    }

    /// Process a `call` node that might be a definition. If it is
    /// a module definition, recurses into its `do_block` to find
    /// nested defs and imports. Returns true if consumed as a
    /// definition.
    fn walk_call(
        &self,
        call: Node,
        s: &[u8],
        symbols: &mut Vec<Symbol>,
        imports: &mut Vec<Import>,
        inside_parent: Option<&str>,
    ) -> bool {
        let Some(kw) = self.definition_kw(call, s) else {
            return false;
        };

        if Self::MODULE_DEFS.contains(&kw) {
            if let Some(name) = self.module_name(call, s) {
                let kind = if kw == "defprotocol" {
                    SymbolKind::Interface
                } else {
                    SymbolKind::Class
                };
                symbols.push(Symbol {
                    kind,
                    name: name.clone(),
                    range: ByteRange::from(call),
                    signature: self.signature(call, s),
                    is_exported: true,
                    parent_class: inside_parent.map(str::to_string),
                });
                if let Some(db) = self.do_block(call) {
                    self.walk_do_block(db, s, symbols, imports, &name);
                }
            }
            return true;
        }

        // Function/macro/guard definition.
        if let Some(name) = self.function_name(call, s) {
            let is_exported = !matches!(kw, "defp" | "defmacrop" | "defguardp");
            symbols.push(Symbol {
                kind: SymbolKind::Function,
                name,
                range: ByteRange::from(call),
                signature: self.signature(call, s),
                is_exported,
                parent_class: inside_parent.map(str::to_string),
            });
            return true;
        }

        false
    }

    /// Try to parse a `call` as an import-like statement:
    /// `import`, `alias`, `require`, `use`.
    fn maybe_import(&self, call: Node, s: &[u8], imports: &mut Vec<Import>) {
        let target = match call.child_by_field_name("target") {
            Some(t) if t.kind() == "identifier" => t,
            _ => return,
        };
        let kw = node_text(target, s);
        if !matches!(kw, "import" | "alias" | "require" | "use") {
            return;
        }
        let Some(args) = self.args(call) else { return };

        // Collect alias/dot names from arguments.
        let mut names = Vec::new();
        let mut source = String::new();
        for i in 0..args.named_child_count() {
            let Some(c) = args.named_child(i) else {
                continue;
            };
            match c.kind() {
                "alias" => {
                    let name = node_text(c, s).to_string();
                    if source.is_empty() {
                        source = name.clone();
                    }
                    names.push(name);
                }
                "dot" => {
                    // `alias MyApp.{Repo, User}` — dot with tuple.
                    // Reconstruct the full source text.
                    let raw = node_text(c, s).to_string();
                    if source.is_empty() {
                        source = raw.clone();
                    }
                    names.push(raw);
                }
                _ => {}
            }
        }
        if !names.is_empty() {
            imports.push(Import {
                names,
                source,
                kind: ImportKind::Module,
            });
        }
    }
}

impl LanguageAdapter for ElixirAdapter {
    fn extensions(&self) -> &[&str] {
        &[".ex", ".exs", ".heex"]
    }

    fn extract(&self, file_path: &Path, source: &str) -> Result<ExtractedFile, String> {
        let lang: tree_sitter::Language = tree_sitter_elixir::LANGUAGE.into();
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

        // Walk top-level `call` nodes. tree-sitter-elixir wraps the
        // entire file in a `source` node; `defmodule` etc. appear as
        // top-level `call` children.
        //
        // `walk_call` recurses into do_blocks for nested defs AND
        // imports. We still need the top-level check for bare imports
        // outside any module (rare but valid in .exs scripts).
        for i in 0..root.named_child_count() {
            let Some(c) = root.named_child(i) else {
                continue;
            };
            if c.kind() == "call" && !self.walk_call(c, bytes, &mut symbols, &mut imports, None) {
                // Not a definition — check for import/alias/require/use.
                self.maybe_import(c, bytes, &mut imports);
            }
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
            head_hash: 0,
        })
    }

    fn find_callees_in_range(
        &self,
        source: &str,
        _file_path: &Path,
        range: ByteRange,
    ) -> Result<Vec<String>, String> {
        let lang: tree_sitter::Language = tree_sitter_elixir::LANGUAGE.into();
        let mut parser = Parser::new();
        parser
            .set_language(&lang)
            .map_err(|e| format!("Failed to set language: {e}"))?;
        let tree = parser.parse(source, None).ok_or("Failed to parse source")?;
        let root = tree.root_node();
        let bytes = source.as_bytes();

        let target = find_node_at_range(root, range.start_byte, range.end_byte)
            .ok_or("Could not find node at given range")?;

        // Match:
        // 1. Bare function calls: `foo()` or `foo arg`
        // 2. Remote calls: `Mod.foo()` → dot's right identifier
        // 3. Pipe operator RHS: `x |> foo()`
        //
        // We skip `def`/`defp`/etc by excluding known definition
        // keywords from capture name (done post-capture).
        let query_str = r#"
            (call target: (identifier) @callee)
            (call target: (dot right: (identifier) @remote_callee))
            (binary_operator
                operator: "|>"
                right: [
                    (call target: (identifier) @piped_callee)
                    (call target: (dot right: (identifier) @piped_remote))
                    (identifier) @piped_callee
                ])
        "#;
        let query = Query::new(&lang, query_str).map_err(|e| format!("Query error: {e}"))?;
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, target, bytes);

        /// Keywords that introduce definitions, not real callees.
        const DEF_KW: &[&str] = &[
            "def",
            "defp",
            "defmodule",
            "defmacro",
            "defmacrop",
            "defprotocol",
            "defimpl",
            "defguard",
            "defguardp",
            "defdelegate",
            "defstruct",
            "defexception",
        ];

        let mut callees = Vec::new();
        while let Some(m) = matches.next() {
            for capture in m.captures {
                if capture.node.kind() == "identifier" {
                    let name = capture.node.utf8_text(bytes).unwrap_or("");
                    if !DEF_KW.contains(&name) {
                        callees.push(name.to_string());
                    }
                }
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
    fn extracts_module_with_functions() {
        let src = r#"
defmodule MyApp.User do
  def greet(name) do
    "Hello, #{name}"
  end

  defp secret, do: :shh
end
"#;
        let f = ElixirAdapter.extract(&pb("user.ex"), src).unwrap();
        let module = f.symbols.iter().find(|s| s.name == "MyApp.User").unwrap();
        assert!(matches!(module.kind, SymbolKind::Class));
        assert!(module.is_exported);

        let greet = f.symbols.iter().find(|s| s.name == "greet").unwrap();
        assert!(matches!(greet.kind, SymbolKind::Function));
        assert_eq!(greet.parent_class.as_deref(), Some("MyApp.User"));
        assert!(greet.is_exported);

        let secret = f.symbols.iter().find(|s| s.name == "secret").unwrap();
        assert!(!secret.is_exported);
    }

    #[test]
    fn extracts_defmacro() {
        let src = r#"
defmodule MyApp.Macros do
  defmacro unless(condition, do: block) do
    quote do
      if !unquote(condition), do: unquote(block)
    end
  end
end
"#;
        let f = ElixirAdapter.extract(&pb("macros.ex"), src).unwrap();
        let unless = f.symbols.iter().find(|s| s.name == "unless").unwrap();
        assert!(matches!(unless.kind, SymbolKind::Function));
        assert!(unless.is_exported);
    }

    #[test]
    fn extracts_imports() {
        let src = r#"
defmodule MyApp do
  import Ecto.Query
  alias MyApp.{Repo, User}
  require Logger
  use GenServer
end
"#;
        let f = ElixirAdapter.extract(&pb("my_app.ex"), src).unwrap();
        assert!(f.imports.iter().any(|i| i.source == "Ecto.Query"));
        assert!(f.imports.iter().any(|i| i.source == "MyApp.{Repo, User}"));
        assert!(f.imports.iter().any(|i| i.source == "Logger"));
        assert!(f.imports.iter().any(|i| i.source == "GenServer"));
    }

    #[test]
    fn extracts_defprotocol_as_interface() {
        let src = r#"
defprotocol MyApp.Serialisable do
  def to_json(data)
  def from_json(json)
end
"#;
        let f = ElixirAdapter.extract(&pb("protocol.ex"), src).unwrap();
        let proto = f
            .symbols
            .iter()
            .find(|s| s.name == "MyApp.Serialisable")
            .unwrap();
        assert!(matches!(proto.kind, SymbolKind::Interface));
        assert!(f.symbols.iter().any(|s| s.name == "to_json"));
        assert!(f.symbols.iter().any(|s| s.name == "from_json"));
    }

    #[test]
    fn extracts_nested_modules() {
        let src = r#"
defmodule MyApp do
  defmodule Nested do
    def helper, do: :ok
  end
end
"#;
        let f = ElixirAdapter.extract(&pb("nested.ex"), src).unwrap();
        let parent = f.symbols.iter().find(|s| s.name == "MyApp").unwrap();
        assert!(matches!(parent.kind, SymbolKind::Class));
        let nested = f.symbols.iter().find(|s| s.name == "Nested").unwrap();
        assert!(matches!(nested.kind, SymbolKind::Class));
        assert_eq!(nested.parent_class.as_deref(), Some("MyApp"));
        let helper = f.symbols.iter().find(|s| s.name == "helper").unwrap();
        assert_eq!(helper.parent_class.as_deref(), Some("Nested"));
    }

    #[test]
    fn find_callees_for_function_body() {
        let src = r#"
defmodule A do
  def run do
    Logger.info("start")
    process_data()
    data |> filter_valid()
  end
end
"#;
        let f = ElixirAdapter.extract(&pb("a.ex"), src).unwrap();
        let run = f.symbols.iter().find(|s| s.name == "run").unwrap();
        let callees = ElixirAdapter
            .find_callees_in_range(src, &pb("a.ex"), run.range)
            .unwrap();
        assert!(
            callees.contains(&"info".to_string()),
            "should find Logger.info call, got: {callees:?}"
        );
        assert!(
            callees.contains(&"process_data".to_string()),
            "should find process_data call, got: {callees:?}"
        );
        assert!(
            callees.contains(&"filter_valid".to_string()),
            "should find filter_valid via pipe operator, got: {callees:?}"
        );
    }

    #[test]
    fn skips_unquote_in_def_names() {
        let src = r#"
defmodule MyApp do
  def unquote(:"Elixir.")(name) do
    name
  end
end
"#;
        let result = ElixirAdapter.extract(&pb("unquote.ex"), src);
        assert!(result.is_ok());
    }
}
