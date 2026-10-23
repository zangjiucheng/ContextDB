//! Trusted file observers (SPEC SD18; ADR 0011). No store or SQLite dependency.
pub mod capture;
pub mod classify;
pub mod descriptor;
pub mod observer;
pub mod symbol;

pub use capture::{Capture, Repo, Unavailable};
pub use classify::{
    AnchorObservation, AnchorState, Observation, Prepared, prepare, prepare_anchors,
};
pub use descriptor::{Descriptor, ObjectFormat, validate_legacy_path, validate_path};
pub use symbol::{AnchorDescriptor, Language, SymbolDescriptor};
