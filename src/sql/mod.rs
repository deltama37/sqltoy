//! SQL lexer, parser, and abstract syntax tree.
//!
//! [`parse`] accepts `CREATE TABLE`, `INSERT`, `SELECT`, `UPDATE`, and
//! `DELETE`, including expressions and `WHERE`. Execution is a later step.
//! Statement [`Display`](std::fmt::Display) text parses back to the same tree.

mod ast;
mod lexer;
mod parser;

pub use ast::{
    format_statements, Assignment, BinaryOp, ColumnDef, ColumnRef, CreateTable, Delete, Expr,
    Insert, Literal, Select, SelectItem, Statement, UnaryOp, Update,
};
pub use parser::parse;
