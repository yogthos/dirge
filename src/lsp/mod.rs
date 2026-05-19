//! Language Server Protocol support.
//!
//! Phase 1 lands the read-only pieces: a registry of supported servers and a
//! workspace-root finder. Subsequent phases add JSON-RPC plumbing
//! ([`client`], P2), file lifecycle + diagnostics (P3), an orchestrator
//! ([`manager`], P4), the agent-facing `lsp` tool (P5), and write/edit
//! integration (P6).

// Symbols in this module are consumed starting in Phase 4. Until then the
// dead-code warnings would clutter every build — silenced module-wide.
#![allow(dead_code)]

pub mod client;
pub mod init;
pub mod jsonrpc;
pub mod language;
pub mod manager;
pub mod rpc;
pub mod server;
pub mod spawn;
pub mod uri;
