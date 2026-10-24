//! Independent input model and Jacobi/Kahn reference evaluators.
mod branches;
pub use branches::{
    BranchOracle, Canonical, Conflict, MergeOutcome, Obj, canonical, conflict_payload,
};
mod cert;
mod eval;
pub use cert::{CertParams, History, cert_issue_oracle, cert_validate_oracle};
mod input;
mod query;
pub use eval::{jacobi, kahn, summarize};
pub use input::{InputModel, asserted_vector, current_vector, semantic};
pub use query::{cert_verdict, why, why_not};
