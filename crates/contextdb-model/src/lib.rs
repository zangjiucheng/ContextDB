//! Dependency-free transaction vocabulary and versioned interchange records.
mod contract;
/// Revision content is immutable: there is no payload replacement operation.
/// ```compile_fail
/// let op = contextdb_model::Op::UpdatePayload { rev: contextdb_model::RevisionId(0), content: [0;32] };
/// ```
/// Rule conclusions are revisions, never logical heads.
/// ```compile_fail
/// let op = contextdb_model::Op::AddRule { conclusion: contextdb_model::LogicalId(0), incidences: vec![], enabled: true };
/// ```
pub struct Contract;
mod cert;
mod hook;
mod status;
mod wire;
pub use cert::*;
pub use contract::*;
pub use hook::*;
pub use status::*;
pub use wire::*;
