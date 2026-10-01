//! Variable-length records stored in slotted pages.
//!
//! A database file is one record set. Page 0 stays the header from the page
//! layer. Every later page is a record page allocated and initialized here.
//! Record ids stay stable across deletes and compaction.

use std::fmt;
use std::io::{self, ErrorKind};
use std::path::Path;
use std::str::FromStr;

use crate::page::{PageId, PageManager};
use crate::slotted_page::SlottedPage;

pub use crate::slotted_page::MAX_RECORD_SIZE;

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

/// Record set stored in one database file.
pub struct RecordFile {
    pages: PageManager,
}

impl RecordFile {
    /// Opens the database at `path`, creating it when the file is empty.
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<RecordFile> {
        Ok(RecordFile {
            pages: PageManager::open(path)?,
        })
    }

    /// Inserts `record` on the first page that can hold it.
    ///
    /// Scans from page 1. When no existing page has room, a new record page is
    /// allocated and initialized. A record longer than [`MAX_RECORD_SIZE`] is
    /// [`ErrorKind::InvalidInput`].
    pub fn insert(&mut self, record: &[u8]) -> io::Result<RecordId> {
        if record.len() > MAX_RECORD_SIZE {
            return Err(record_too_large(record.len()));
        }
        let count = self.pages.page_count()?;
        for raw_id in 1..count {
            let page_id = PageId(raw_id);
            let page = self.pages.read_page(page_id)?;
            let mut slotted = SlottedPage::from_page(page_id, page)?;
            if let Some(slot_id) = slotted.insert(record) {
                self.store(page_id, &slotted)?;
                return Ok(RecordId { page_id, slot_id });
            }
        }
        let page_id = self.pages.allocate_page()?;
        let mut slotted = SlottedPage::init();
        let Some(slot_id) = slotted.insert(record) else {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "initialized record page has no room",
            ));
        };
        self.store(page_id, &slotted)?;
        Ok(RecordId { page_id, slot_id })
    }

    /// Reads the live record identified by `id`.
    ///
    /// An out-of-range page, page 0, an out-of-range slot, or a tombstone is
    /// [`ErrorKind::NotFound`].
    pub fn get(&mut self, id: RecordId) -> io::Result<Vec<u8>> {
        let page = self.read_record_page(id)?;
        match page.get(id.slot_id) {
            Some(bytes) => Ok(bytes.to_vec()),
            None => Err(not_found(id)),
        }
    }

    /// Replaces the record identified by `id`. The id does not change.
    ///
    /// A missing record is [`ErrorKind::NotFound`]. A value longer than
    /// [`MAX_RECORD_SIZE`] is [`ErrorKind::InvalidInput`]. A value that does
    /// not fit on the same page is [`ErrorKind::InvalidInput`] and leaves the
    /// page unchanged.
    pub fn update(&mut self, id: RecordId, record: &[u8]) -> io::Result<()> {
        let mut page = self.read_record_page(id)?;
        if page.get(id.slot_id).is_none() {
            return Err(not_found(id));
        }
        if record.len() > MAX_RECORD_SIZE {
            return Err(record_too_large(record.len()));
        }
        match page.update(id.slot_id, record) {
            Ok(true) => {}
            Ok(false) => {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    format!("record does not fit in page: {id}"),
                ));
            }
            Err(err) if err.kind() == ErrorKind::NotFound => return Err(not_found(id)),
            Err(err) => return Err(err),
        }
        self.store(id.page_id, &page)
    }

    /// Tombstones the record identified by `id`.
    ///
    /// A missing record is [`ErrorKind::NotFound`].
    pub fn delete(&mut self, id: RecordId) -> io::Result<()> {
        let mut page = self.read_record_page(id)?;
        page.delete(id.slot_id).map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                not_found(id)
            } else {
                err
            }
        })?;
        self.store(id.page_id, &page)
    }

    /// Live records in page order, then slot order.
    pub fn scan(&mut self) -> io::Result<Vec<(RecordId, Vec<u8>)>> {
        let count = self.pages.page_count()?;
        let mut records = Vec::new();
        for raw_id in 1..count {
            let page_id = PageId(raw_id);
            let page = self.pages.read_page(page_id)?;
            let slotted = SlottedPage::from_page(page_id, page)?;
            for (slot_id, bytes) in slotted.iter_live() {
                records.push((RecordId { page_id, slot_id }, bytes.to_vec()));
            }
        }
        Ok(records)
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
        SlottedPage::from_page(id.page_id, page)
    }

    fn store(&mut self, page_id: PageId, slotted: &SlottedPage) -> io::Result<()> {
        self.pages.write_page(page_id, slotted.page())?;
        self.pages.sync()
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
    use super::{RecordFile, RecordId, MAX_RECORD_SIZE};
    use crate::page::{PageId, PageManager};
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
            TempDb { path }
        }

        fn path(&self) -> &PathBuf {
            &self.path
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
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
        assert!(file.scan().unwrap().is_empty());

        let alice = file.insert(b"Alice").unwrap();
        let bob = file.insert(b"Bob").unwrap();
        assert_eq!(alice.to_string(), "1:0");
        assert_eq!(bob.to_string(), "1:1");
        assert_eq!(file.get(alice).unwrap(), b"Alice");

        file.update(alice, b"Alicia").unwrap();
        assert_eq!(file.get(alice).unwrap(), b"Alicia");
        file.delete(bob).unwrap();

        let err = file.get(bob).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(err.to_string(), format!("record not found: {bob}"));
        assert_eq!(file.scan().unwrap(), vec![(alice, b"Alicia".to_vec())]);

        let carol = file.insert(b"").unwrap();
        assert_eq!(carol, bob);
        assert_eq!(file.get(carol).unwrap(), b"");
        let scanned = file.scan().unwrap();
        assert_eq!(
            scanned,
            vec![(alice, b"Alicia".to_vec()), (carol, Vec::new())]
        );
    }

    #[test]
    fn missing_records_and_oversized_values() {
        let db = TempDb::new("missing");
        let mut file = RecordFile::open(db.path()).unwrap();

        let header = RecordId {
            page_id: PageId(0),
            slot_id: 0,
        };
        let err = file.get(header).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(err.to_string(), "record not found: 0:0");

        let missing = RecordId {
            page_id: PageId(1),
            slot_id: 0,
        };
        let err = file.get(missing).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(err.to_string(), "record not found: 1:0");
        let err = file.delete(missing).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        let err = file.update(missing, b"nope").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);

        let err = file.insert(&[0u8; MAX_RECORD_SIZE + 1]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            err.to_string(),
            format!(
                "record too large: {} bytes (max {MAX_RECORD_SIZE})",
                MAX_RECORD_SIZE + 1
            )
        );
        assert!(file.scan().unwrap().is_empty());

        let id = file.insert(&[7u8; MAX_RECORD_SIZE]).unwrap();
        assert_eq!(id.to_string(), "1:0");
        assert_eq!(file.get(id).unwrap(), vec![7u8; MAX_RECORD_SIZE]);

        let err = file.update(id, &[0u8; MAX_RECORD_SIZE + 1]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            err.to_string(),
            format!(
                "record too large: {} bytes (max {MAX_RECORD_SIZE})",
                MAX_RECORD_SIZE + 1
            )
        );
        assert_eq!(file.get(id).unwrap(), vec![7u8; MAX_RECORD_SIZE]);

        let other = RecordId {
            page_id: PageId(1),
            slot_id: 1,
        };
        let err = file.update(other, &[8u8; MAX_RECORD_SIZE + 1]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(err.to_string(), "record not found: 1:1");
        assert_eq!(file.get(id).unwrap(), vec![7u8; MAX_RECORD_SIZE]);
    }

    #[test]
    fn update_that_does_not_fit_leaves_the_page() {
        let db = TempDb::new("nofit");
        let mut file = RecordFile::open(db.path()).unwrap();
        let first = file.insert(&[1u8; 2000]).unwrap();
        let second = file.insert(&[2u8; 2000]).unwrap();
        let err = file.update(first, &[3u8; 3000]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            err.to_string(),
            format!("record does not fit in page: {first}")
        );
        assert_eq!(file.get(first).unwrap(), vec![1u8; 2000]);
        assert_eq!(file.get(second).unwrap(), vec![2u8; 2000]);
    }

    #[test]
    fn uninitialized_page_is_rejected() {
        let db = TempDb::new("uninit");
        {
            let mut pages = PageManager::open(db.path()).unwrap();
            assert_eq!(pages.allocate_page().unwrap(), PageId(1));
        }
        let mut file = RecordFile::open(db.path()).unwrap();
        let err = file.insert(b"hi").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "not a record page: 1");

        let err = file
            .get(RecordId {
                page_id: PageId(1),
                slot_id: 0,
            })
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "not a record page: 1");
    }

    #[test]
    fn format_version_stays_one() {
        let db = TempDb::new("version");
        {
            let mut file = RecordFile::open(db.path()).unwrap();
            file.insert(b"row").unwrap();
            file.update(
                RecordId {
                    page_id: PageId(1),
                    slot_id: 0,
                },
                b"row2",
            )
            .unwrap();
        }
        let mut pages = PageManager::open(db.path()).unwrap();
        assert_eq!(pages.format_version().unwrap(), 1);
        let header = pages.read_page(PageId(0)).unwrap();
        assert_eq!(&header.data()[..8], b"SQLTOYDB");
    }
}
