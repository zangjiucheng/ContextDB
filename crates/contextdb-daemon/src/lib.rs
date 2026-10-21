//! `ctxd`: single-writer daemon, epoch-pinned snapshot readers, sessions,
//! certificates and local IPC (SPEC §5 P3, ADR 0008).
pub mod actor;
pub mod lease;
pub mod proto;
