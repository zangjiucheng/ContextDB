//! Durable operation log, blobs and recovery (SPEC §5 P2, ADR 0006).
//!
//! op_log is an own append-only segment log and the only source of truth.
//! All durable I/O goes through [`Durability`]; [`SimDisk`] drives crash tests.
pub mod doctor;
pub mod durability;
pub mod log;
pub mod migrate;
mod pack;
pub mod projection;
pub mod recall;
pub mod session;
#[doc(hidden)]
pub mod snapshot;
mod store;
pub use durability::{Durability, SimDisk, StdDisk, TailFate};
pub use projection::ProjectionFile;
pub use store::{
    LogReader, OpenOptions, OpenReport, Payload, Prepare, Store, StoreError, TruncatedTail,
    TxnStatus, Unavailable, Write,
};
