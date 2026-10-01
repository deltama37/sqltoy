//! sqltoy is a small learning RDBMS written in Rust.
//!
//! The storage layer addresses one local file by byte offset. The page layer
//! manages that file as fixed-size pages, with page 0 reserved as a header.
//! The record layer stores variable-length records in slotted pages and
//! addresses them by record id. The catalog layer records table schemas in
//! the file and restores them when the database is opened. The table layer
//! stores typed rows in those tables, including NULL. Later layers (SQL and
//! so on) are specified in `docs/adr/`.

pub mod catalog;
pub mod page;
pub mod record;
pub mod row;
pub mod slotted_page;
pub mod storage;
pub mod table;

pub use catalog::{Column, ColumnType, Database, TableSchema};
pub use page::{Page, PageId, PageManager, PAGE_SIZE};
pub use record::{RecordFile, RecordId, TableId, MAX_RECORD_SIZE};
pub use row::Value;
pub use storage::Storage;
