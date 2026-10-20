//! Independent input model and Jacobi/Kahn reference evaluators.
mod eval;
mod input;
mod query;
pub use eval::{jacobi, kahn, summarize};
pub use input::{InputModel, asserted_vector, current_vector, semantic};
pub use query::{cert_verdict, why, why_not};
