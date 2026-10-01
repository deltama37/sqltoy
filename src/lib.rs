//! sqltoy is a small learning RDBMS written in Rust.
//!
//! The storage layer addresses one local file by byte offset. The page layer
//! manages that file as fixed-size pages, with page 0 reserved as a header.
//! Later layers (records, SQL, and so on) are specified in `docs/adr/`.

pub mod page;
pub mod storage;

pub use page::{Page, PageId, PageManager, PAGE_SIZE};
pub use storage::Storage;
