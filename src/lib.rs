//! sqltoy is a small learning RDBMS written in Rust.
//!
//! The storage layer addresses one local file by byte offset. The page layer
//! manages that file as fixed-size pages, with page 0 reserved as a header.
//! The buffer pool caches those pages in frames. A commit appends the dirty
//! pages to a write-ahead log next to the database file, syncs that log, then
//! checkpoints the pages into the database and truncates the log. Opening a
//! database replays any committed log records that were not checkpointed.
//! Dirty pages are not evicted. `BEGIN`, `COMMIT`, and `ROLLBACK`
//! group statements on one session; a statement outside a transaction commits
//! itself, and a statement that fails is undone. Several sessions can each
//! hold a transaction. Rows are versions with snapshot-isolation visibility,
//! and rollback undoes that session's versions without dropping another
//! session's dirty pages. `VACUUM` removes versions that every active
//! snapshot has already stopped seeing. The record layer stores
//! variable-length records in slotted pages and addresses them by record id.
//! The catalog layer records table schemas in
//! the file and restores them when the database is opened. The table layer
//! stores typed rows in those tables, including NULL. The SQL parser turns
//! `CREATE TABLE`, `INSERT`, `SELECT`, `UPDATE`, `DELETE`, `VACUUM`, and
//! `EXPLAIN` text into an AST. The executor evaluates expressions, including
//! `WHERE`, and runs those statements. A primary key is an `INTEGER` column
//! indexed by a B+Tree. The planner uses that index for an equality, a
//! range, or a nested-loop join on the key, and for `ORDER BY` of the key
//! ascending. `EXPLAIN` prints the operator tree. `EXPLAIN ANALYZE` runs a
//! `SELECT` and reports each operator's row count and the pages read.
//! Design notes are in `docs/adr/`.

pub mod btree;
pub mod buffer;
pub mod catalog;
pub mod crash;
pub mod crc32;
pub mod exec;
pub mod mvcc;
pub mod page;
pub mod record;
pub mod row;
pub mod slotted_page;
pub mod sql;
pub mod storage;
pub mod table;
pub mod wal;

pub use btree::BTree;
pub use buffer::{BufferPool, BufferStats, DEFAULT_POOL_PAGES};
pub use catalog::{Column, ColumnSpec, ColumnType, Database, SessionId, TableSchema};
pub use exec::{format_result, QueryResult};
pub use page::{Page, PageId, PageManager, PAGE_SIZE};
pub use record::{RecordFile, RecordId, TableId, MAX_RECORD_SIZE};
pub use row::Value;
pub use storage::Storage;
pub use wal::wal_path;
