//! Variable-length records stored in slotted pages.
//!
//! Each record page belongs to one table. Page 0 stays the header from the
//! page layer. Later pages are record pages allocated and initialized here.
//! Record ids stay stable across deletes and compaction, and are unique in
//! the file.
//!
//! Mutations update a [`crate::buffer::BufferPool`] and do not sync a single
//! page on their own. [`RecordFile::open`] flushes after each insert, update,
//! and delete so the record CLI stays durable across processes. That flush is
//! the WAL commit.
//! [`crate::catalog::Database`] opens with [`RecordFile::open_pooled`], which
//! leaves that flush off, and syncs once at the end of a statement or a
//! public row call.

use std::fmt;
use std::io::{self, ErrorKind};
use std::path::Path;
use std::str::FromStr;

use crate::btree::PAGE_TYPE_BTREE;
use crate::buffer::{BufferPool, BufferStats, DEFAULT_POOL_PAGES};
use crate::page::PageId;
use crate::slotted_page::SlottedPage;

pub use crate::slotted_page::MAX_RECORD_SIZE;

/// Identity of one table.
///
/// `0` is never a valid id. [`TableId::CATALOG`] is the system catalog.
/// User tables start at [`TableId::FIRST_USER`] and count upward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TableId(pub u16);

impl TableId {
    /// Table id of the catalog record set.
    pub const CATALOG: TableId = TableId(1);

    /// First id assigned to a user table.
    pub const FIRST_USER: TableId = TableId(2);
}

impl fmt::Display for TableId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Identity of one record.
///
/// The display and parse format is `page:slot`, for example `1:0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordId {
    pub page_id: PageId,
    pub slot_id: u16,
}

impl fmt::Display for RecordId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.page_id, self.slot_id)
    }
}

impl FromStr for RecordId {
    type Err = io::Error;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let invalid =
            || io::Error::new(ErrorKind::InvalidInput, format!("invalid record id: {raw}"));
        let Some((page, slot)) = raw.split_once(':') else {
            return Err(invalid());
        };
        let page_id = page.parse::<u32>().map_err(|_| invalid())?;
        let slot_id = slot.parse::<u16>().map_err(|_| invalid())?;
        Ok(RecordId {
            page_id: PageId(page_id),
            slot_id,
        })
    }
}

/// Records stored in one database file.
///
/// Each record page belongs to a single table. Record ids are unique in the
/// file and stay stable across deletes and compaction.
pub struct RecordFile {
    pages: BufferPool,
    /// When set, each successful insert, update, or delete flushes the pool.
    autoflush: bool,
}

impl RecordFile {
    /// Opens the database at `path`, creating it when the file is empty.
    ///
    /// Uses [`DEFAULT_POOL_PAGES`] frames. Each insert, update, and delete
    /// flushes before it returns.
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<RecordFile> {
        Self::open_in(path, DEFAULT_POOL_PAGES, true)
    }

    /// Opens `path` with `frames` and does not flush after each mutation.
    ///
    /// [`crate::catalog::Database`] flushes at statement and public-API
    /// boundaries instead, so one statement does not sync once per page.
    pub(crate) fn open_pooled<P: AsRef<Path>>(path: P, frames: usize) -> io::Result<RecordFile> {
        Self::open_in(path, frames, false)
    }

    fn open_in<P: AsRef<Path>>(path: P, frames: usize, autoflush: bool) -> io::Result<RecordFile> {
        Ok(RecordFile {
            pages: BufferPool::open(path, frames)?,
            autoflush,
        })
    }

    /// Inserts `record` on the first page owned by `table` that can hold it.
    ///
    /// Scans from page 1 and skips pages owned by other tables and B+Tree
    /// nodes. When no owned page has room, a new record page is allocated and
    /// initialized for `table`. A record longer than [`MAX_RECORD_SIZE`] is
    /// [`ErrorKind::InvalidInput`]. [`TableId`] `0` is [`ErrorKind::InvalidInput`].
    pub fn insert(&mut self, table: TableId, record: &[u8]) -> io::Result<RecordId> {
        require_table(table)?;
        if record.len() > MAX_RECORD_SIZE {
            return Err(record_too_large(record.len()));
        }
        let count = self.pages.page_count()?;
        for raw_id in 1..count {
            let page_id = PageId(raw_id);
            let Some(mut slotted) = self.read_record_page_or_skip(page_id)? else {
                continue;
            };
            if slotted.owner() != table {
                continue;
            }
            if let Some(slot_id) = slotted.insert(record) {
                self.store_and_flush(page_id, &slotted)?;
                return Ok(RecordId { page_id, slot_id });
            }
        }
        let page_id = self.pages.allocate_page()?;
        let mut slotted = SlottedPage::init(table);
        let Some(slot_id) = slotted.insert(record) else {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "initialized record page has no room",
            ));
        };
        self.store_and_flush(page_id, &slotted)?;
        Ok(RecordId { page_id, slot_id })
    }

    /// Reads the live record identified by `id` on `table`.
    ///
    /// An out-of-range page, page 0, a B+Tree node, a page owned by another
    /// table, an out-of-range slot, or a tombstone is [`ErrorKind::NotFound`].
    /// [`TableId`] `0` is [`ErrorKind::InvalidInput`].
    pub fn get(&mut self, table: TableId, id: RecordId) -> io::Result<Vec<u8>> {
        require_table(table)?;
        let page = self.read_owned_page(table, id)?;
        match page.get(id.slot_id) {
            Some(bytes) => Ok(bytes.to_vec()),
            None => Err(not_found(id)),
        }
    }

    /// Replaces the record identified by `id` on `table` when the bytes fit.
    ///
    /// Returns `Ok(true)` after the new bytes are stored. Returns `Ok(false)`
    /// when they do not fit on the same page; the page is left unchanged and
    /// the id is not moved. A missing record, including one stored on a page
    /// owned by another table, is [`ErrorKind::NotFound`]. A value longer than
    /// [`MAX_RECORD_SIZE`] is [`ErrorKind::InvalidInput`]. [`TableId`] `0` is
    /// [`ErrorKind::InvalidInput`].
    pub fn try_update(&mut self, table: TableId, id: RecordId, record: &[u8]) -> io::Result<bool> {
        require_table(table)?;
        let mut page = self.read_owned_page(table, id)?;
        if page.get(id.slot_id).is_none() {
            return Err(not_found(id));
        }
        if record.len() > MAX_RECORD_SIZE {
            return Err(record_too_large(record.len()));
        }
        match page.update(id.slot_id, record) {
            Ok(true) => {
                self.store_and_flush(id.page_id, &page)?;
                Ok(true)
            }
            Ok(false) => Ok(false),
            Err(err) if err.kind() == ErrorKind::NotFound => Err(not_found(id)),
            Err(err) => Err(err),
        }
    }

    /// Replaces the record identified by `id` on `table`. The id does not change.
    ///
    /// A missing record, including one stored on a page owned by another
    /// table, is [`ErrorKind::NotFound`]. A value longer than
    /// [`MAX_RECORD_SIZE`] is [`ErrorKind::InvalidInput`]. A value that does
    /// not fit on the same page is [`ErrorKind::InvalidInput`] and leaves the
    /// page unchanged. [`TableId`] `0` is [`ErrorKind::InvalidInput`].
    ///
    /// This is [`Self::try_update`], with `Ok(false)` mapped to the fit error.
    pub fn update(&mut self, table: TableId, id: RecordId, record: &[u8]) -> io::Result<()> {
        if self.try_update(table, id, record)? {
            Ok(())
        } else {
            Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("record does not fit in page: {id}"),
            ))
        }
    }

    /// Tombstones the record identified by `id` on `table`.
    ///
    /// A missing record, including one stored on a page owned by another
    /// table, is [`ErrorKind::NotFound`]. [`TableId`] `0` is
    /// [`ErrorKind::InvalidInput`].
    pub fn delete(&mut self, table: TableId, id: RecordId) -> io::Result<()> {
        require_table(table)?;
        let mut page = self.read_owned_page(table, id)?;
        page.delete(id.slot_id).map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                not_found(id)
            } else {
                err
            }
        })?;
        self.store_and_flush(id.page_id, &page)
    }

    /// Live records owned by `table`, in page order, then slot order.
    ///
    /// [`TableId`] `0` is [`ErrorKind::InvalidInput`].
    pub fn scan(&mut self, table: TableId) -> io::Result<Vec<(RecordId, Vec<u8>)>> {
        require_table(table)?;
        let mut records = Vec::new();
        let mut page_id = 1u32;
        loop {
            match self.scan_page(table, PageId(page_id))? {
                None => return Ok(records),
                Some(page) => {
                    records.extend(page);
                    page_id += 1;
                }
            }
        }
    }

    /// Live records on one page owned by `table`.
    ///
    /// `Ok(None)` when `page_id` is past the last page. `Ok(Some(vec))` when
    /// that page was considered: the vec is empty when the page is a B+Tree
    /// node or belongs to another table. Page 0 is empty and is not read.
    /// No other page is read. [`TableId`] `0` is [`ErrorKind::InvalidInput`].
    #[allow(clippy::type_complexity)]
    pub fn scan_page(
        &mut self,
        table: TableId,
        page_id: PageId,
    ) -> io::Result<Option<Vec<(RecordId, Vec<u8>)>>> {
        require_table(table)?;
        if page_id == PageId(0) {
            return Ok(Some(Vec::new()));
        }
        let count = self.pages.page_count()?;
        if page_id.0 >= count {
            return Ok(None);
        }
        let Some(slotted) = self.read_record_page_or_skip(page_id)? else {
            return Ok(Some(Vec::new()));
        };
        if slotted.owner() != table {
            return Ok(Some(Vec::new()));
        }
        let records = slotted
            .iter_live()
            .map(|(slot_id, bytes)| (RecordId { page_id, slot_id }, bytes.to_vec()))
            .collect();
        Ok(Some(records))
    }

    fn read_owned_page(&mut self, table: TableId, id: RecordId) -> io::Result<SlottedPage> {
        let page = self.read_record_page(id)?;
        if page.owner() != table {
            return Err(not_found(id));
        }
        Ok(page)
    }

    fn read_record_page(&mut self, id: RecordId) -> io::Result<SlottedPage> {
        if id.page_id == PageId(0) {
            return Err(not_found(id));
        }
        let count = self.pages.page_count()?;
        if id.page_id.0 >= count {
            return Err(not_found(id));
        }
        let page = self.pages.read_page(id.page_id)?;
        if page.data().first().copied() == Some(PAGE_TYPE_BTREE) {
            return Err(not_found(id));
        }
        SlottedPage::from_page(id.page_id, page)
    }

    /// `Ok(None)` for a B+Tree node. Any other non-record page is an error.
    fn read_record_page_or_skip(&mut self, page_id: PageId) -> io::Result<Option<SlottedPage>> {
        let page = self.pages.read_page(page_id)?;
        if page.data().first().copied() == Some(PAGE_TYPE_BTREE) {
            return Ok(None);
        }
        Ok(Some(SlottedPage::from_page(page_id, page)?))
    }

    /// Logical page reads since this file was opened.
    ///
    /// A hit and a miss both count. The counter is not stored in the file.
    pub fn pages_read(&self) -> u64 {
        self.pages.stats().logical_reads
    }

    /// Buffer-pool counters since this file was opened.
    pub(crate) fn buffer_stats(&self) -> BufferStats {
        self.pages.stats()
    }

    /// Commits dirty pages through the WAL and checkpoints the file.
    pub fn flush(&mut self) -> io::Result<()> {
        self.pages.flush()
    }

    pub(crate) fn set_savepoint(&mut self) -> io::Result<()> {
        self.pages.set_savepoint()
    }

    pub(crate) fn rollback_to_savepoint(&mut self) -> io::Result<()> {
        self.pages.rollback_to_savepoint()
    }

    pub(crate) fn release_savepoint(&mut self) -> io::Result<()> {
        self.pages.release_savepoint()
    }

    /// Drops dirty frames and unflushed allocations.
    pub(crate) fn discard_dirty(&mut self) -> io::Result<()> {
        self.pages.discard_dirty()
    }

    /// Frames held by the pool, including overflow past the configured capacity.
    #[cfg(test)]
    pub(crate) fn frame_count(&self) -> usize {
        self.pages.frame_count()
    }

    /// Buffer pool, so the index can share this file.
    pub(crate) fn pages_mut(&mut self) -> &mut BufferPool {
        &mut self.pages
    }

    fn store_and_flush(&mut self, page_id: PageId, slotted: &SlottedPage) -> io::Result<()> {
        self.pages.write_page(page_id, slotted.page())?;
        if self.autoflush {
            self.pages.flush()?;
        }
        Ok(())
    }
}

fn require_table(table: TableId) -> io::Result<()> {
    if table == TableId(0) {
        Err(io::Error::new(
            ErrorKind::InvalidInput,
            "invalid table id: 0",
        ))
    } else {
        Ok(())
    }
}

fn not_found(id: RecordId) -> io::Error {
    io::Error::new(ErrorKind::NotFound, format!("record not found: {id}"))
}

fn record_too_large(len: usize) -> io::Error {
    io::Error::new(
        ErrorKind::InvalidInput,
        format!("record too large: {len} bytes (max {MAX_RECORD_SIZE})"),
    )
}

#[cfg(test)]
mod tests {
    use super::{RecordFile, RecordId, TableId, MAX_RECORD_SIZE};
    use crate::page::{PageId, PageManager};
    use crate::slotted_page::SlottedPage;
    use std::env::temp_dir;
    use std::fs;
    use std::io::ErrorKind;
    use std::path::PathBuf;
    use std::process;
    use std::str::FromStr;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDb {
        path: PathBuf,
    }

    impl TempDb {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let mut path = temp_dir();
            path.push(format!("sqltoy-{label}-{}-{nanos}", process::id()));
            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(crate::wal::wal_path(&path));
            TempDb { path }
        }

        fn path(&self) -> &PathBuf {
            &self.path
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
            let _ = fs::remove_file(crate::wal::wal_path(&self.path));
        }
    }

    const TABLE: TableId = TableId(2);

    #[test]
    fn table_id_display() {
        assert_eq!(TableId::CATALOG.to_string(), "1");
        assert_eq!(TableId::FIRST_USER.to_string(), "2");
        assert_eq!(TableId(0).to_string(), "0");
    }

    #[test]
    fn record_id_display_and_parse() {
        let id = RecordId {
            page_id: PageId(12),
            slot_id: 3,
        };
        assert_eq!(id.to_string(), "12:3");
        assert_eq!(RecordId::from_str("12:3").unwrap(), id);
        assert_eq!("1:0".parse::<RecordId>().unwrap().slot_id, 0);
        assert_eq!(
            "4294967295:65535".parse::<RecordId>().unwrap(),
            RecordId {
                page_id: PageId(u32::MAX),
                slot_id: u16::MAX,
            }
        );

        for raw in [
            "", "1", "1:", ":1", "a:1", "1:b", "1:2:3", "-1:0", "1:65536",
        ] {
            let err = raw.parse::<RecordId>().unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidInput);
            assert_eq!(err.to_string(), format!("invalid record id: {raw}"));
        }
    }

    #[test]
    fn insert_get_update_delete_and_scan() {
        let db = TempDb::new("records");
        let mut file = RecordFile::open(db.path()).unwrap();
        assert!(file.scan(TABLE).unwrap().is_empty());

        let alice = file.insert(TABLE, b"Alice").unwrap();
        let bob = file.insert(TABLE, b"Bob").unwrap();
        assert_eq!(alice.to_string(), "1:0");
        assert_eq!(bob.to_string(), "1:1");
        assert_eq!(file.get(TABLE, alice).unwrap(), b"Alice");

        file.update(TABLE, alice, b"Alicia").unwrap();
        assert_eq!(file.get(TABLE, alice).unwrap(), b"Alicia");
        file.delete(TABLE, bob).unwrap();

        let err = file.get(TABLE, bob).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(err.to_string(), format!("record not found: {bob}"));
        assert_eq!(file.scan(TABLE).unwrap(), vec![(alice, b"Alicia".to_vec())]);

        let carol = file.insert(TABLE, b"").unwrap();
        assert_eq!(carol, bob);
        assert_eq!(file.get(TABLE, carol).unwrap(), b"");
        let scanned = file.scan(TABLE).unwrap();
        assert_eq!(
            scanned,
            vec![(alice, b"Alicia".to_vec()), (carol, Vec::new())]
        );
    }

    #[test]
    fn scan_page_reads_only_the_requested_page() {
        let db = TempDb::new("scan-page");
        let mut file = RecordFile::open(db.path()).unwrap();
        let wide = vec![1u8; 3000];
        let first = file.insert(TABLE, &wide).unwrap();
        let second = file.insert(TABLE, &wide).unwrap();
        assert_ne!(first.page_id, second.page_id);

        let before = file.pages_read();
        let page = file.scan_page(TABLE, first.page_id).unwrap().unwrap();
        assert_eq!(page, vec![(first, wide.clone())]);
        assert_eq!(file.pages_read() - before, 1);
        assert!(file
            .scan_page(TABLE, PageId(0))
            .unwrap()
            .unwrap()
            .is_empty());
        assert_eq!(file.pages_read() - before, 1);
        assert!(file
            .scan_page(TABLE, PageId(second.page_id.0 + 5))
            .unwrap()
            .is_none());
        assert_eq!(file.pages_read() - before, 1);

        let other = TableId(3);
        let foreign = file.insert(other, b"x").unwrap();
        let skipped = file.scan_page(TABLE, foreign.page_id).unwrap().unwrap();
        assert!(skipped.is_empty());
    }

    #[test]
    fn missing_records_and_oversized_values() {
        let db = TempDb::new("missing");
        let mut file = RecordFile::open(db.path()).unwrap();

        let header = RecordId {
            page_id: PageId(0),
            slot_id: 0,
        };
        let err = file.get(TABLE, header).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(err.to_string(), "record not found: 0:0");

        let missing = RecordId {
            page_id: PageId(1),
            slot_id: 0,
        };
        let err = file.get(TABLE, missing).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(err.to_string(), "record not found: 1:0");
        let err = file.delete(TABLE, missing).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        let err = file.update(TABLE, missing, b"nope").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);

        let err = file.insert(TABLE, &[0u8; MAX_RECORD_SIZE + 1]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            err.to_string(),
            format!(
                "record too large: {} bytes (max {MAX_RECORD_SIZE})",
                MAX_RECORD_SIZE + 1
            )
        );
        assert!(file.scan(TABLE).unwrap().is_empty());

        let id = file.insert(TABLE, &[7u8; MAX_RECORD_SIZE]).unwrap();
        assert_eq!(id.to_string(), "1:0");
        assert_eq!(file.get(TABLE, id).unwrap(), vec![7u8; MAX_RECORD_SIZE]);

        let err = file
            .update(TABLE, id, &[0u8; MAX_RECORD_SIZE + 1])
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            err.to_string(),
            format!(
                "record too large: {} bytes (max {MAX_RECORD_SIZE})",
                MAX_RECORD_SIZE + 1
            )
        );
        assert_eq!(file.get(TABLE, id).unwrap(), vec![7u8; MAX_RECORD_SIZE]);

        let other = RecordId {
            page_id: PageId(1),
            slot_id: 1,
        };
        let err = file
            .update(TABLE, other, &[8u8; MAX_RECORD_SIZE + 1])
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(err.to_string(), "record not found: 1:1");
        assert_eq!(file.get(TABLE, id).unwrap(), vec![7u8; MAX_RECORD_SIZE]);
    }

    #[test]
    fn try_update_returns_false_when_the_record_does_not_fit() {
        let db = TempDb::new("tryupd");
        let mut file = RecordFile::open(db.path()).unwrap();
        let first = file.insert(TABLE, &[1u8; 2000]).unwrap();
        let second = file.insert(TABLE, &[2u8; 2000]).unwrap();
        assert!(!file.try_update(TABLE, first, &[3u8; 3000]).unwrap());
        assert_eq!(file.get(TABLE, first).unwrap(), vec![1u8; 2000]);
        assert_eq!(file.get(TABLE, second).unwrap(), vec![2u8; 2000]);
        assert!(file.try_update(TABLE, first, &[4u8; 100]).unwrap());
        assert_eq!(file.get(TABLE, first).unwrap(), vec![4u8; 100]);

        let missing = RecordId {
            page_id: PageId(1),
            slot_id: 9,
        };
        let err = file.try_update(TABLE, missing, b"nope").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(err.to_string(), "record not found: 1:9");
        assert_eq!(file.get(TABLE, first).unwrap(), vec![4u8; 100]);
    }

    #[test]
    fn update_that_does_not_fit_leaves_the_page() {
        let db = TempDb::new("nofit");
        let mut file = RecordFile::open(db.path()).unwrap();
        let first = file.insert(TABLE, &[1u8; 2000]).unwrap();
        let second = file.insert(TABLE, &[2u8; 2000]).unwrap();
        let err = file.update(TABLE, first, &[3u8; 3000]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            err.to_string(),
            format!("record does not fit in page: {first}")
        );
        assert_eq!(file.get(TABLE, first).unwrap(), vec![1u8; 2000]);
        assert_eq!(file.get(TABLE, second).unwrap(), vec![2u8; 2000]);
    }

    #[test]
    fn uninitialized_page_is_rejected() {
        let db = TempDb::new("uninit");
        {
            let mut pages = PageManager::open(db.path()).unwrap();
            assert_eq!(pages.allocate_page().unwrap(), PageId(1));
        }
        let mut file = RecordFile::open(db.path()).unwrap();
        let err = file.insert(TABLE, b"hi").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "not a record page: 1");

        let err = file
            .get(
                TABLE,
                RecordId {
                    page_id: PageId(1),
                    slot_id: 0,
                },
            )
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "not a record page: 1");
    }

    #[test]
    fn format_version_is_three() {
        let db = TempDb::new("version");
        {
            let mut file = RecordFile::open(db.path()).unwrap();
            file.insert(TABLE, b"row").unwrap();
            file.update(
                TABLE,
                RecordId {
                    page_id: PageId(1),
                    slot_id: 0,
                },
                b"row2",
            )
            .unwrap();
        }
        let mut pages = PageManager::open(db.path()).unwrap();
        assert_eq!(pages.format_version().unwrap(), 3);
        let header = pages.read_page(PageId(0)).unwrap();
        assert_eq!(&header.data()[..8], b"SQLTOYDB");
    }

    #[test]
    fn tables_never_share_a_page() {
        let db = TempDb::new("tables");
        let users = TableId(2);
        let posts = TableId(3);
        let mut file = RecordFile::open(db.path()).unwrap();

        let mut user_ids = Vec::new();
        for _ in 0..4 {
            user_ids.push(file.insert(users, &[1u8; 1000]).unwrap());
        }
        assert!(user_ids.iter().all(|id| id.page_id == PageId(1)));

        let post = file.insert(posts, &[2u8; 1000]).unwrap();
        assert_eq!(post.page_id, PageId(2));

        let spilled = file.insert(users, &[3u8; 1000]).unwrap();
        assert_eq!(spilled.page_id, PageId(3));
        let again = file.insert(posts, b"again").unwrap();
        assert_eq!(again.page_id, PageId(2));

        let scanned = file.scan(users).unwrap();
        assert_eq!(scanned.len(), 5);
        assert!(scanned.iter().all(|(id, _)| id.page_id != post.page_id));
        assert_eq!(
            file.scan(posts).unwrap(),
            vec![(post, vec![2u8; 1000]), (again, b"again".to_vec())]
        );
        assert!(file.scan(TableId(4)).unwrap().is_empty());

        let err = file.get(posts, user_ids[0]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(
            err.to_string(),
            format!("record not found: {}", user_ids[0])
        );
        let err = file.update(posts, user_ids[0], b"nope").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(
            err.to_string(),
            format!("record not found: {}", user_ids[0])
        );
        let err = file.delete(posts, user_ids[0]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(file.get(users, user_ids[0]).unwrap(), vec![1u8; 1000]);

        drop(file);
        let mut pages = PageManager::open(db.path()).unwrap();
        let page = pages.read_page(PageId(1)).unwrap();
        assert_eq!(
            SlottedPage::from_page(PageId(1), page).unwrap().owner(),
            users
        );
        let page = pages.read_page(PageId(2)).unwrap();
        assert_eq!(
            SlottedPage::from_page(PageId(2), page).unwrap().owner(),
            posts
        );
        let page = pages.read_page(PageId(3)).unwrap();
        assert_eq!(
            SlottedPage::from_page(PageId(3), page).unwrap().owner(),
            users
        );
    }

    #[test]
    fn table_id_zero_is_rejected() {
        let db = TempDb::new("table0");
        let mut file = RecordFile::open(db.path()).unwrap();
        let id = RecordId {
            page_id: PageId(1),
            slot_id: 0,
        };
        let err = file.insert(TableId(0), b"x").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "invalid table id: 0");
        let err = file.get(TableId(0), id).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "invalid table id: 0");
        let err = file.update(TableId(0), id, b"x").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "invalid table id: 0");
        let err = file.delete(TableId(0), id).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "invalid table id: 0");
        let err = file.scan(TableId(0)).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "invalid table id: 0");

        let stored = file.insert(TableId(2), b"ok").unwrap();
        assert_eq!(stored.to_string(), "1:0");
    }

    #[test]
    fn btree_pages_are_skipped_and_are_not_records() {
        use crate::btree::BTree;

        let db = TempDb::new("btreeskip");
        let mut file = RecordFile::open(db.path()).unwrap();
        let root = BTree::create(file.pages_mut(), TABLE).unwrap();
        let id = file.insert(TABLE, b"row").unwrap();
        assert_ne!(id.page_id, root);
        assert_eq!(file.scan(TABLE).unwrap(), vec![(id, b"row".to_vec())]);

        let index_rid = RecordId {
            page_id: root,
            slot_id: 0,
        };
        for op in ["get", "update", "delete"] {
            let err = match op {
                "get" => file.get(TABLE, index_rid).unwrap_err(),
                "update" => file.update(TABLE, index_rid, b"nope").unwrap_err(),
                "delete" => file.delete(TABLE, index_rid).unwrap_err(),
                _ => unreachable!(),
            };
            assert_eq!(err.kind(), ErrorKind::NotFound, "{op}");
            assert_eq!(err.to_string(), format!("record not found: {index_rid}"));
        }
        assert_eq!(file.get(TABLE, id).unwrap(), b"row");
        assert!(file.pages_read() > 0);
    }
}
