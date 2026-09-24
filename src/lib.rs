//! sqltoy is a small learning RDBMS written in Rust.
//!
//! Storage is implemented first: a single local file addressed by byte offset.
//! Later layers (pages, records, SQL, and so on) are specified in `docs/adr/`.

pub mod storage;

pub use storage::Storage;
