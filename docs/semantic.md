# Semantic code tools

When built with `--features "semantic,semantic-ts,semantic-python"` (and the
other `semantic-<lang>` features), dirge gains AST-powered code analysis via
tree-sitter:

| Tool | Description |
|------|-------------|
| `list_symbols` | List functions, classes, methods, interfaces, and type aliases in a file or project. Filter by kind. |
| `get_symbol_body` | Full source of a named symbol via precise byte-range extraction. |
| `find_definition` | Locate where a symbol is defined across the project. |
| `find_callers` | Find all call sites of a function/method via the tree-sitter symbol index (word-boundary semantics, excludes the definition site). |
| `find_callees` | Extract all function/method calls made within a symbol's body (tree-sitter query). |

Supports TypeScript/TSX, Python, Clojure (`.clj`/`.cljs`/`.cljc`/`.edn`/`.bb`),
Go, Ruby (`.rb`/`.rake`/`.gemspec`), Rust, Java, C (`.c`/`.h`), and C++
(`.cpp`/`.cc`/`.cxx`/`.hpp`/`.hh`/`.hxx`). Index is built lazily on first use and
cached by file mtime.

## Export detection per language

| Language | Exports detected from | Maps to dirge SymbolKinds |
|---|---|---|
| TypeScript/TSX | `export` keyword + index re-exports | function/class/interface/method/type alias |
| Python | leading underscore convention; `__dunder__` treated as public | function/class/method |
| Clojure | `defn-` is private; everything else exported | function/variable/class (defrecord/deftype) /interface (defprotocol) /method (defmethod, defprotocol body) |
| Go | uppercase-first-letter convention | function/method (receiver type as `parent_class`) /class (struct) /interface (with methods) /type alias |
| Ruby | not detected (visibility is keyword-scoped) | class/interface (module) /method (instance + `def self.`) /function (top-level) |
| Rust | `pub` / `pub(crate)` / `pub(super)` visibility modifier | function/class (struct/enum) /interface (trait + methods) /method (impl block, attached to receiving type) /type alias /variable (const/static) |
| Java | `public` modifier; package-private + `private` / `protected` stay non-exported | class/interface/method (incl. constructors) /variable (fields) — nested classes recursed |
| C | `static` storage class = non-exported; extern by default | function/class (struct/enum) /type alias (typedef; suppressed when wrapping a named struct to avoid duplicates) |
| C++ | `public:` / `private:` / `protected:` access labels tracked through class bodies | class (class/struct) /method (incl. through templates + namespaces) /function (top-level) — namespaces recursed |

C and C++ both claim `.h`. When extracting a `.h` file, dirge sniffs the
first 32 KiB for C++-only markers (`class `, `namespace `, `template<`, `::`)
and routes the file to the C++ adapter if any match — so a Qt / libstdc++
header with classes is parsed correctly without the user having to rename it.
Pure-C headers fall through to the C adapter as before.

## Adding a language

Adding a new language requires writing a Rust `LanguageAdapter` impl (see
`src/semantic/adapters/clojure.rs` for a 60-line reference covering the full
lifecycle) and gating it behind a new `semantic-<lang>` cargo feature.
Tree-sitter Rust bindings don't load grammars dynamically today, so the
per-language adapters need to ship in the binary — but users who want their own
language can add an adapter in a fork without touching anything outside
`src/semantic/`. For runtime-pluggable language intelligence, register an LSP
server in `config.json` instead (see [lsp.md](lsp.md)) — that's the supported
path for languages dirge doesn't bake in.
