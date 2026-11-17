//! Independent input model and Jacobi/Kahn reference evaluators.
mod access;
pub use access::AccessModel;
mod branches;
pub use branches::{
    BranchOracle, Canonical, Conflict, MergeOutcome, Obj, canonical, conflict_payload,
};
mod cert;
mod eval;
pub use cert::{
    CertParams, Gaps, History, cert_issue_oracle, cert_validate_oracle, cert_validate_oracle_gaps,
};
mod input;
mod query;
pub use eval::{jacobi, kahn, summarize, with_replication_lag};
pub use input::{InputModel, asserted_vector, current_vector, semantic};
pub use query::{cert_verdict, why, why_not};
pub mod team;
