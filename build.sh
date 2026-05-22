#!/usr/bin/env bash
set -euo pipefail

# Enable every feature dirge ships with so the release binary
# has the full surface area. Defaults (`loop`, `git-worktree`,
# `mcp`, `lsp`) come along automatically because we don't pass
# `--no-default-features`; this list explicitly adds:
# - `semantic` + per-language adapters
#     (ts / python / bash / clojure / go / ruby / rust / java / c / c++)
# - `plugin` (Janet runtime)
# - `acp` (Zed/editor agent-protocol server)
FEATURES="${FEATURES:-semantic,semantic-ts,semantic-python,semantic-bash,semantic-clojure,semantic-go,semantic-ruby,semantic-rust,semantic-java,semantic-c,semantic-cpp,mcp,loop,git-worktree,plugin,acp,lsp}"

echo "==> Building dirge with features: $FEATURES"
cargo build --features "$FEATURES" --release

echo "==> Binary: target/release/dirge"
ls -lh target/release/dirge
