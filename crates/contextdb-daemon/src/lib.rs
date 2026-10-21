//! `ctxd`: single-writer daemon, epoch-pinned snapshot readers, sessions,
//! certificates and local IPC (SPEC §5 P3, ADR 0008).
//!
//! Under `cfg(loom)` only the publication handshake is built, for its model
//! test (ADR 0008 §11 G4).
pub mod publish;

#[cfg(not(loom))]
pub mod actor;
#[cfg(not(loom))]
pub mod lease;
#[cfg(not(loom))]
pub use contextdb_proto::proto;
#[cfg(not(loom))]
pub mod server;
#[cfg(not(loom))]
pub mod service;
