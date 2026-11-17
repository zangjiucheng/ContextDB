//! P10 team exchange (ADR 0018 r0.6, SPEC v1.10.0 SD21).
//!
//! This crate holds what every edge must agree on byte for byte: the team
//! document, the portable team ops (`CXTB`), the strict commit and trailer
//! parser, and in-process `sshsig` verification. Acceptance is frozen per
//! `accept_version`; nothing here reads a local store.

pub mod b64;
pub mod catchup;
pub mod checkpoint;
pub mod commit;
pub mod doc;
pub mod enc;
pub mod git;
pub mod gitobj;
pub mod ops;
pub mod replica;
pub mod sshsig;
pub mod state;
pub mod store;
