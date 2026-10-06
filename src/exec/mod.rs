//! SQL execution.
//!
//! Expressions use three-valued logic. Statements call the table operations.
//! [`format_result`] prints a [`QueryResult`].

mod eval;
mod executor;
mod format;

pub use eval::{eval, RowContext};
pub use executor::QueryResult;
pub use format::format_result;
