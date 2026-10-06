//! SQL execution.
//!
//! Expressions use three-valued logic. `SELECT` runs as a tree of operators
//! chosen by a rule-based planner (scan, index range, join, filter, project,
//! sort, limit). [`format_result`] prints a [`QueryResult`]. `EXPLAIN` prints
//! the tree.

mod eval;
mod executor;
mod format;
mod operator;
mod plan;
mod planner;

pub use eval::{eval, RowContext};
pub use executor::QueryResult;
pub use format::format_result;
