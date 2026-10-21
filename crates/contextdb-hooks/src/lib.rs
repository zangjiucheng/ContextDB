//! `ctx-hook` and the client-side outbox (SPEC §5 P4, ADR 0010).
//!
//! The library is pure where it can be: harness adapters turn hook stdin into
//! a [`adapter::HookEvent`], renderers turn a decision into exactly the JSON
//! each pinned harness accepts, and the outbox keeps facts until the daemon
//! has made them durable. The binary wires these to `ctxd`.
pub mod adapter;
pub mod hook;
/// The outbox lives in `contextdb-proto` so `ctxd`'s sweeper can share it.
pub use contextdb_proto::outbox;
pub mod patch;
pub mod paths;
pub mod render;
