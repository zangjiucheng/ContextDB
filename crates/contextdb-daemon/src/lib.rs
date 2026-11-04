//! `ctxd`: single-writer daemon, epoch-pinned snapshot readers, sessions,
//! certificates and local IPC (SPEC §5 P3, ADR 0008).
//!
//! Under `cfg(loom)` only the publication handshake is built, for its model
//! test (ADR 0008 §11 G4).
pub mod publish;

#[cfg(not(loom))]
pub mod actor;
#[cfg(not(loom))]
pub mod candidates;
/// Retrieval projection health and the recall bounded wait (P8, ADR 0016 §2).
pub mod index_health;
#[cfg(not(loom))]
pub mod jobs;
#[cfg(not(loom))]
pub mod lease;
/// Recall rank fusion (P8, ADR 0016 §3.2).
pub mod ranking;
#[cfg(not(loom))]
pub use contextdb_proto::proto;
#[cfg(not(loom))]
pub mod server;
#[cfg(not(loom))]
pub mod service;
#[cfg(not(loom))]
pub mod validators;

#[cfg(not(loom))]
mod anchors;
#[cfg(not(loom))]
mod changes;
#[cfg(not(loom))]
mod conflicts;
