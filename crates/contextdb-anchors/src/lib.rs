//! Trusted file observers (SPEC SD18; ADR 0011). No store or SQLite dependency.
pub mod capture;
pub mod classify;
pub mod descriptor;
pub mod observer;

pub use capture::{Capture, Repo, Unavailable};
pub use classify::{AnchorState, Observation, Prepared, prepare};
pub use descriptor::{Descriptor, ObjectFormat, validate_path};
