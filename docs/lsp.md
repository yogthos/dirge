# LSP integration

When built with the `lsp` feature (on by default), dirge attaches Language
Server Protocol clients to your project and surfaces compile-time diagnostics
directly in the agent's tool output. After every `write` or `edit`, the LSP
server gets a `didChange`, waits for a fresh diagnostic publish, and any ERRORs
land in the tool result as a `<diagnostics file="...">` block — so the agent
corrects compile errors on the same turn instead of writing broken code and
discovering it later via `cargo check`.

| Tool | Effect |
|------|--------|
| `read`  | Fire-and-forget `didOpen` so the server has the file in memory by the time the agent edits it. No diagnostic block in `read` output. |
| `write` | After write: `didChange` + wait for diagnostics + append errors-block. |
| `edit`  | Same as `write`. |
| `lsp`   | Agent-facing tool that exposes `definition`, `references`, `hover`, `documentSymbol`, `workspaceSymbol`, `implementation`, `prepareCallHierarchy`, `incomingCalls`, `outgoingCalls`. 1-based coordinates. |

## Built-in server set

| Server id | Binary | Extensions |
|-----------|--------|------------|
| `rust` | `rust-analyzer` | `.rs` |
| `typescript` | `typescript-language-server --stdio` | `.ts`, `.tsx`, `.mts`, `.cts`, `.js`, `.jsx`, `.mjs`, `.cjs` |
| `pyright` | `pyright-langserver --stdio` | `.py`, `.pyi` |
| `clojure-lsp` | `clojure-lsp` | `.clj`, `.cljs`, `.cljc`, `.edn`, `.bb` |
| `gopls` | `gopls` | `.go` |
| `jdtls` | `jdtls` | `.java` |
| `clangd` | `clangd` | `.c`, `.cc`, `.cpp`, `.cxx`, `.h`, `.hh`, `.hpp`, `.hxx`, `.m`, `.mm` |
| `ruby-lsp` | `ruby-lsp` | `.rb`, `.rake`, `.gemspec` |
| `bash-language-server` | `bash-language-server start` | `.sh`, `.bash` |

Missing binaries trip the broken-server backoff (1s → 2s → … capped at 10 min)
rather than failing dirge — the rest of the session keeps working. Override the
spawn command per server via the `lsp` config key; see [CONFIG.md](../CONFIG.md).

## Workspace root resolution

Resolution is per-server: rust-analyzer walks past nested member crates to the
workspace `Cargo.toml` declaring `[workspace]`; typescript stops at the nearest
`package.json`/`tsconfig.json` and yields to deno when a `deno.json` is closer;
pyright looks for `pyproject.toml`/`setup.py`/etc.; clojure-lsp looks for
`deps.edn`/`project.clj`/`shadow-cljs.edn`/`bb.edn`/`.clj-kondo`; gopls follows
`go.mod`/`go.work`; jdtls looks for `pom.xml`/`build.gradle`; clangd uses
`compile_commands.json`/`CMakeLists.txt`/`Makefile`/`meson.build`; ruby-lsp
follows `Gemfile`/`Rakefile`; bash-language-server uses the file's parent.

Disable: `--no-lsp` flag or `{ "lsp": false }` in the config. Per-server
overrides (custom command, env, init options) live in the config — see
[CONFIG.md](../CONFIG.md).
