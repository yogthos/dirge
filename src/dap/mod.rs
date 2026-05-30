//! DAP (Debug Adapter Protocol) integration. Feature-gated behind
//! `#[cfg(feature = "dap")]` — all public types in this module are
//! invisible when the feature is off.

pub mod client;
pub mod config;
mod framing;
pub mod session;
pub mod types;
