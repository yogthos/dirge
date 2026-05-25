use std::path::Path;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

use crate::semantic::adapter::LanguageAdapter;
use crate::semantic::common::{find_node_at_range, node_text};
use crate::semantic::types::{ByteRange, ExtractedFile, Import, ImportKind, Symbol, SymbolKind};

/// Tree-sitter adapter for Clojure / ClojureScript / cljc / edn / bb.
/// Uses `tree-sitter-clojure` (sogaiu's grammar, packaged on crates.io).
///
/// Clojure has no per-method receiver concept the way Python/TS classes
/// do — what we surface as `SymbolKind::Class` are protocols, records,
/// and types declared at the top level. Methods come from `defmethod`
/// + `extend-protocol`. The grammar is minimalist (everything is a
/// list/symbol/etc.) so symbol extraction is mostly "look at the head
/// of each top-level list and dispatch by name".
pub struct ClojureAdapter;

impl ClojureAdapter {
    // `node_text` and `make_range` previously lived here; now from
    // `crate::semantic::common`. The local wrapper functions are
    // kept as thin shims so the rest of this adapter reads
    // unchanged — `node_text(n, s)` etc.

    /// The text of a `sym_lit` is in its `sym_name` child. For
    /// namespace-qualified symbols like `clojure.string/blank?` the
    /// grammar exposes `sym_ns` (namespace) and `sym_name` (leaf)
    /// children separately — we want only `blank?`. When neither
    /// child exists (older grammar revisions or malformed input),
    /// fall back to the raw text BUT strip any `ns/` prefix so the
    /// callees list / symbol index don't get polluted with fully
    /// qualified names.
    fn sym_name<'a>(&self, sym_lit: Node<'a>, source: &'a [u8]) -> Option<&'a str> {
        for i in 0..sym_lit.named_child_count() {
            if let Some(c) = sym_lit.named_child(i)
                && c.kind() == "sym_name"
            {
                return Some(node_text(c, source));
            }
        }
        // Fallback: raw node text with the `ns/` prefix stripped so
        // `clojure.string/blank?` is normalized to `blank?`. Symbols
        // with no namespace prefix pass through unchanged.
        let raw = node_text(sym_lit, source);
        Some(raw.rsplit_once('/').map(|(_, leaf)| leaf).unwrap_or(raw))
    }

    /// Children of a `list_lit` that are actual forms (symbols / lists /
    /// vectors / …) — skips the bare paren tokens. Returns a Vec of
    /// (form-index, node) so callers can ask "what's the 2nd form?".
    fn list_forms<'a>(&self, list_lit: Node<'a>) -> Vec<Node<'a>> {
        let mut out = Vec::new();
        for i in 0..list_lit.named_child_count() {
            if let Some(c) = list_lit.named_child(i) {
                out.push(c);
            }
        }
        out
    }

    /// Build the leading line of source up to the first non-arglist /
    /// body element. For `(defn foo [x y] (+ x y))` returns
    /// `(defn foo [x y]`. Used as the symbol signature for display.
    fn signature_line(&self, node: Node, source: &[u8]) -> String {
        let text = node_text(node, source);
        // First newline cap; many Clojure defs sprawl over multiple
        // lines once they include docstrings + bodies, but the leading
        // line is enough for the listing.
        let first_line = text.lines().next().unwrap_or(text);
        // If the leading line itself is huge (single-line entire
        // function), trim at ~80 chars + ellipsis.
        if first_line.chars().count() > 80 {
            let prefix: String = first_line.chars().take(80).collect();
            format!("{prefix}…")
        } else {
            first_line.to_string()
        }
    }

    /// Inspect a top-level `list_lit` and emit zero or more symbols.
    /// The "head" symbol (`defn`, `def`, `defprotocol`, …) drives
    /// dispatch.
    fn extract_from_top_list(
        &self,
        list_lit: Node,
        source: &[u8],
        symbols: &mut Vec<Symbol>,
        imports: &mut Vec<Import>,
        exports: &mut Vec<String>,
    ) {
        let forms = self.list_forms(list_lit);
        if forms.is_empty() {
            return;
        }
        let head = forms[0];
        if head.kind() != "sym_lit" {
            return;
        }
        let Some(head_name) = self.sym_name(head, source) else {
            return;
        };

        let range = ByteRange::from(list_lit);
        let signature = self.signature_line(list_lit, source);

        match head_name {
            // Function-like defs. `defn-` is private (Clojure
            // convention: `^:private` metadata also exists but the
            // dashed-name form is overwhelmingly common).
            "defn" | "defn-" | "defmacro" | "defmulti" => {
                if let Some(name_node) = forms.get(1)
                    && let Some(name) = self.sym_name(*name_node, source)
                {
                    let is_exported = head_name != "defn-";
                    symbols.push(Symbol {
                        kind: SymbolKind::Function,
                        name: name.to_string(),
                        range,
                        signature,
                        is_exported,
                        parent_class: None,
                    });
                }
            }
            // Value defs.
            "def" => {
                if let Some(name_node) = forms.get(1)
                    && let Some(name) = self.sym_name(*name_node, source)
                {
                    // Names beginning with `-` aren't a Clojure
                    // privacy convention (that's just defn-), but
                    // metadata `^:private` and `defonce` are; we
                    // can't easily inspect metadata here so default
                    // to exported.
                    symbols.push(Symbol {
                        kind: SymbolKind::Variable,
                        name: name.to_string(),
                        range,
                        signature,
                        is_exported: true,
                        parent_class: None,
                    });
                }
            }
            // Defmethod: `(defmethod fname dispatch-val [args] ...)`.
            // We surface this as a Method anchored to the multifn name.
            "defmethod" => {
                if let Some(name_node) = forms.get(1)
                    && let Some(name) = self.sym_name(*name_node, source)
                {
                    // Audit L5: the multifn name *is* the symbol name;
                    // setting `parent_class = Some(name)` was
                    // self-referential and useless for disambiguation.
                    // The dispatch value (`forms[2]`) is what makes
                    // sibling defmethods on the same multifn distinct
                    // (`(defmethod shape :circle …)` vs
                    // `(defmethod shape :square …)`), so anchor the
                    // method to that dispatch value instead. Falls
                    // back to `None` when forms[2] isn't extractable.
                    let dispatch_val = forms.get(2).map(|n| node_text(*n, source).to_string());
                    symbols.push(Symbol {
                        kind: SymbolKind::Method,
                        name: name.to_string(),
                        range,
                        signature,
                        is_exported: true,
                        parent_class: dispatch_val,
                    });
                }
            }
            // Protocols → Interface; records/types → Class.
            // Methods inside `defprotocol` are reported as their own
            // Method symbols so `list_symbols --kind method` finds them.
            "defprotocol" | "definterface" => {
                if let Some(name_node) = forms.get(1)
                    && let Some(name) = self.sym_name(*name_node, source)
                {
                    let proto_name = name.to_string();
                    symbols.push(Symbol {
                        kind: SymbolKind::Interface,
                        name: proto_name.clone(),
                        range,
                        signature,
                        is_exported: true,
                        parent_class: None,
                    });
                    // Methods listed in a defprotocol body look like
                    // `(method-name [this & args] "docstring")`. We
                    // walk forms[2..] and emit any list whose head
                    // is a sym_lit.
                    for body_form in forms.iter().skip(2) {
                        if body_form.kind() != "list_lit" {
                            continue;
                        }
                        let sub = self.list_forms(*body_form);
                        if let Some(method_head) = sub.first()
                            && method_head.kind() == "sym_lit"
                            && let Some(method_name) = self.sym_name(*method_head, source)
                        {
                            symbols.push(Symbol {
                                kind: SymbolKind::Method,
                                name: method_name.to_string(),
                                range: ByteRange::from(*body_form),
                                signature: self.signature_line(*body_form, source),
                                is_exported: true,
                                parent_class: Some(proto_name.clone()),
                            });
                        }
                    }
                }
            }
            "defrecord" | "deftype" => {
                if let Some(name_node) = forms.get(1)
                    && let Some(name) = self.sym_name(*name_node, source)
                {
                    symbols.push(Symbol {
                        kind: SymbolKind::Class,
                        name: name.to_string(),
                        range,
                        signature,
                        is_exported: true,
                        parent_class: None,
                    });
                }
            }
            // `(ns my.ns (:require [other.ns :as alias]))`. We don't
            // emit a top-level symbol for `ns` (it's a declaration,
            // not a value), but we DO harvest `:require` entries
            // into imports so the import index works.
            "ns" => {
                for form in forms.iter().skip(2) {
                    if form.kind() != "list_lit" {
                        continue;
                    }
                    let inner = self.list_forms(*form);
                    // First child should be a keyword like :require
                    // or :use.
                    let Some(directive) = inner.first() else {
                        continue;
                    };
                    if directive.kind() != "kwd_lit" {
                        continue;
                    }
                    let dir_text = node_text(*directive, source);
                    if dir_text != ":require" && dir_text != ":use" {
                        continue;
                    }
                    // Each subsequent vector / symbol is a requirement.
                    for req in inner.iter().skip(1) {
                        match req.kind() {
                            "sym_lit" => {
                                if let Some(name) = self.sym_name(*req, source) {
                                    imports.push(Import {
                                        names: vec![name.to_string()],
                                        source: name.to_string(),
                                        kind: ImportKind::Module,
                                    });
                                }
                            }
                            "vec_lit" => {
                                // `[clojure.string :as str]` —
                                // first symbol is the namespace.
                                for i in 0..req.named_child_count() {
                                    if let Some(c) = req.named_child(i)
                                        && c.kind() == "sym_lit"
                                        && let Some(name) = self.sym_name(c, source)
                                    {
                                        imports.push(Import {
                                            names: vec![name.to_string()],
                                            source: name.to_string(),
                                            kind: ImportKind::Module,
                                        });
                                        break;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {
                // Other top-level forms (comment blocks, side-effect
                // calls, etc.) are not extracted. exports stays as a
                // dirge-internal hint surface; Clojure namespaces
                // implicitly export all public vars, so we leave the
                // `exports` vec empty and rely on `is_exported` per
                // symbol.
                let _ = exports;
            }
        }
    }
}

impl LanguageAdapter for ClojureAdapter {
    fn extensions(&self) -> &[&str] {
        &[".clj", ".cljs", ".cljc", ".edn", ".bb"]
    }

    fn extract(&self, file_path: &Path, source: &str) -> Result<ExtractedFile, String> {
        let lang: tree_sitter::Language = tree_sitter_clojure::LANGUAGE.into();
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

        // Walk top-level forms only. `source` is the root; each
        // direct named child is a top-level form (list_lit, etc.).
        for i in 0..root.named_child_count() {
            if let Some(child) = root.named_child(i)
                && child.kind() == "list_lit"
            {
                self.extract_from_top_list(
                    child,
                    source_bytes,
                    &mut symbols,
                    &mut imports,
                    &mut exports,
                );
            }
        }

        // Backfill exports from is_exported symbols. Adapters that
        // have an explicit export list (TS index re-exports, etc.)
        // populate `exports` directly; for everything else, the
        // is_exported flag on each symbol is authoritative and we
        // mirror it here so consumers don't have to re-iterate.
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
        let lang: tree_sitter::Language = tree_sitter_clojure::LANGUAGE.into();
        let mut parser = Parser::new();
        parser
            .set_language(&lang)
            .map_err(|e| format!("Failed to set language: {e}"))?;
        let tree = parser.parse(source, None).ok_or("Failed to parse source")?;
        let root = tree.root_node();
        let source_bytes = source.as_bytes();

        let target = find_node_at_range(root, range.start_byte, range.end_byte)
            .ok_or("Could not find node at given range")?;

        // Match every `list_lit` and pull its leading symbol — the
        // function in head position. Filter out Clojure special
        // forms / binding forms so the LLM sees real call sites and
        // not every `let` or `if` in the body.
        let query_str = "(list_lit . (sym_lit (sym_name) @callee))";
        let query = Query::new(&lang, query_str).map_err(|e| format!("Query error: {e}"))?;
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, target, source_bytes);

        const SPECIAL_FORMS: &[&str] = &[
            "def",
            "defn",
            "defn-",
            "defmacro",
            "defmulti",
            "defmethod",
            "defprotocol",
            "defrecord",
            "deftype",
            "definterface",
            "ns",
            "let",
            "let*",
            "fn",
            "fn*",
            "if",
            "when",
            "when-not",
            "when-let",
            "if-let",
            "if-not",
            "cond",
            "case",
            "do",
            "loop",
            "loop*",
            "recur",
            "quote",
            "var",
            "try",
            "catch",
            "finally",
            "throw",
            "new",
            ".",
            "->",
            "->>",
            "as->",
            "doto",
            "comment",
        ];

        let mut callees = Vec::new();
        while let Some(m) = matches.next() {
            for capture in m.captures {
                let name = capture.node.utf8_text(source_bytes).unwrap_or("");
                if SPECIAL_FORMS.contains(&name) {
                    continue;
                }
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

    fn adapter() -> ClojureAdapter {
        ClojureAdapter
    }

    fn pb(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(name)
    }

    #[test]
    fn extracts_defn_and_marks_dash_private() {
        let src = "(defn pub-fn [x] x)\n(defn- priv-fn [x] x)\n";
        let f = adapter().extract(&pb("a.clj"), src).unwrap();
        assert_eq!(f.symbols.len(), 2);
        let pub_sym = f.symbols.iter().find(|s| s.name == "pub-fn").unwrap();
        let priv_sym = f.symbols.iter().find(|s| s.name == "priv-fn").unwrap();
        assert!(pub_sym.is_exported);
        assert!(!priv_sym.is_exported);
        assert!(matches!(pub_sym.kind, SymbolKind::Function));
    }

    #[test]
    fn extracts_def_as_variable() {
        let src = "(def PI 3.14)\n";
        let f = adapter().extract(&pb("a.clj"), src).unwrap();
        assert_eq!(f.symbols.len(), 1);
        assert_eq!(f.symbols[0].name, "PI");
        assert!(matches!(f.symbols[0].kind, SymbolKind::Variable));
    }

    #[test]
    fn extracts_defprotocol_with_methods() {
        let src = "(defprotocol Greeter (welcome [this]) (bye [this msg]))\n";
        let f = adapter().extract(&pb("a.clj"), src).unwrap();
        // 1 interface + 2 methods.
        assert_eq!(f.symbols.len(), 3);
        let proto = f.symbols.iter().find(|s| s.name == "Greeter").unwrap();
        assert!(matches!(proto.kind, SymbolKind::Interface));
        let welcome = f.symbols.iter().find(|s| s.name == "welcome").unwrap();
        assert!(matches!(welcome.kind, SymbolKind::Method));
        assert_eq!(welcome.parent_class.as_deref(), Some("Greeter"));
    }

    #[test]
    fn extracts_defrecord_as_class() {
        let src = "(defrecord Person [name age])\n";
        let f = adapter().extract(&pb("a.clj"), src).unwrap();
        assert_eq!(f.symbols.len(), 1);
        assert_eq!(f.symbols[0].name, "Person");
        assert!(matches!(f.symbols[0].kind, SymbolKind::Class));
    }

    #[test]
    fn extracts_ns_require_into_imports() {
        let src = "(ns my.app (:require [clojure.string :as str] clojure.set))\n";
        let f = adapter().extract(&pb("a.clj"), src).unwrap();
        assert!(
            f.imports
                .iter()
                .any(|i| i.source.contains("clojure.string")),
        );
        assert!(f.imports.iter().any(|i| i.source.contains("clojure.set")));
    }

    #[test]
    fn find_callees_skips_special_forms() {
        let src = "(defn x [a b] (let [c (+ a b)] (if (pos? c) (println c) (str a b))))\n";
        let f = adapter().extract(&pb("a.clj"), src).unwrap();
        let x = &f.symbols[0];
        let callees = adapter()
            .find_callees_in_range(src, &pb("a.clj"), x.range)
            .unwrap();
        // `let`, `if` filtered; `+`, `pos?`, `println`, `str` kept.
        assert!(callees.contains(&"+".to_string()));
        assert!(callees.contains(&"pos?".to_string()));
        assert!(callees.contains(&"println".to_string()));
        assert!(callees.contains(&"str".to_string()));
        assert!(!callees.contains(&"let".to_string()));
        assert!(!callees.contains(&"if".to_string()));
    }

    /// Defmethod is surfaced as a Method whose parent_class is the
    /// multifn name (handy for `list_symbols --kind method`).
    #[test]
    fn extracts_defmethod_as_method() {
        let src = "(defmulti shape :kind)\n(defmethod shape :circle [c] :circle)\n";
        let f = adapter().extract(&pb("a.clj"), src).unwrap();
        let multifn = f
            .symbols
            .iter()
            .find(|s| matches!(s.kind, SymbolKind::Function))
            .expect("defmulti present");
        assert_eq!(multifn.name, "shape");
        let method = f
            .symbols
            .iter()
            .find(|s| matches!(s.kind, SymbolKind::Method))
            .expect("defmethod present");
        assert_eq!(method.name, "shape");
        assert_eq!(method.parent_class.as_deref(), Some("shape"));
    }

    /// Extensions list is what the AdapterRegistry routes on.
    #[test]
    fn extensions_cover_clojure_family() {
        let a = adapter();
        let exts = a.extensions();
        for needed in [".clj", ".cljs", ".cljc", ".edn", ".bb"] {
            assert!(exts.contains(&needed), "missing extension: {needed}");
        }
    }

    /// Namespace-qualified symbols (`clojure.string/blank?`) in
    /// call position must surface only the leaf name. Previously
    /// the fallback returned the full `ns/name` string, polluting
    /// the call graph.
    #[test]
    fn find_callees_strips_namespace_from_qualified_calls() {
        let src = "(defn run [] (clojure.string/blank? \"x\") (str/join \", \" [1 2]))\n";
        let f = adapter().extract(&pb("a.clj"), src).unwrap();
        let run = &f.symbols[0];
        let callees = adapter()
            .find_callees_in_range(src, &pb("a.clj"), run.range)
            .unwrap();
        // Leaf names, not fully qualified.
        assert!(
            callees.iter().any(|c| c == "blank?"),
            "expected leaf 'blank?'; got {callees:?}",
        );
        assert!(
            callees.iter().any(|c| c == "join"),
            "expected leaf 'join'; got {callees:?}",
        );
        // Should NOT contain the full qualified path.
        assert!(
            !callees.iter().any(|c| c.contains('/')),
            "no callee should contain a /: {callees:?}",
        );
    }

    /// Imports are tagged with `ImportKind::Module` (Clojure
    /// namespaces look like single dotted tokens, not the scoped
    /// `Foo::Bar` syntax used by Rust/Java).
    #[test]
    fn imports_are_tagged_module_kind() {
        let src = "(ns app (:require [clojure.string :as str]))\n";
        let f = adapter().extract(&pb("a.clj"), src).unwrap();
        assert!(!f.imports.is_empty());
        for imp in &f.imports {
            assert_eq!(imp.kind, ImportKind::Module);
        }
    }

    /// Exports are populated from `is_exported=true` symbols.
    /// `defn` is exported; `defn-` private.
    #[test]
    fn exports_mirror_is_exported_symbols() {
        let src = "(defn public-thing [] :ok)\n(defn- private-thing [] :no)\n";
        let f = adapter().extract(&pb("a.clj"), src).unwrap();
        assert!(f.exports.iter().any(|n| n == "public-thing"));
        assert!(!f.exports.iter().any(|n| n == "private-thing"));
    }
}
