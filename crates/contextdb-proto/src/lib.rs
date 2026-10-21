//! The `ctxd` wire protocol, lease discovery and a blocking client, shared by
//! the daemon and its light clients (`ctx-hook`, `ctx-mcp`, `ctx`) without
//! linking the store (ADR 0010 §1).
pub mod client;
pub mod lease;
pub mod proto;

pub use client::Client;
