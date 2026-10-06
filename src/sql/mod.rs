//! SQL lexer, parser, and abstract syntax tree.
//!
//! [`parse`] accepts `CREATE TABLE`, `INSERT`, `SELECT`, `UPDATE`,
//! `DELETE`, `VACUUM`, `BEGIN`, `COMMIT`, and `ROLLBACK`, including expressions,
//! `WHERE`, `PRIMARY KEY`, `JOIN`, `ORDER BY`, and `LIMIT` / `OFFSET`.
//! [`crate::Database::execute`] runs those statements. Statement
//! [`Display`](std::fmt::Display) text parses back to the same tree.

mod ast;
mod lexer;
mod parser;

pub use ast::{
    format_statements, Assignment, BinaryOp, ColumnDef, ColumnRef, CreateTable, Delete, Expr,
    FromItem, Insert, Join, JoinKind, Literal, OrderItem, Select, SelectItem, Statement, TableRef,
    UnaryOp, Update,
};
pub use parser::parse;
