//! SQL execution.
//!
//! Expressions use three-valued logic. `SELECT` runs as a tree of operators
//! (scan, join, filter, project, sort, limit). [`format_result`] prints a
//! [`QueryResult`].

mod eval;
mod executor;
mod format;
mod operator;
mod plan;

pub use eval::{eval, RowContext};
pub use executor::QueryResult;
pub use format::format_result;
