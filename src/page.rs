//! Fixed-size pages over a [`crate::storage::Storage`] file.
//!
//! Page 0 is reserved as a header so a database file describes its own format.

use std::fmt;
use std::io;
use std::ops::{Deref, DerefMut};
use std::path::Path;

use crate::storage::Storage;

/// Size of every page, in bytes.
pub const PAGE_SIZE: usize = 4096;

const MAGIC: &[u8; 8] = b"SQLTOYDB";
const FORMAT_VERSION: u32 = 3;

const MAGIC_OFFSET: usize = 0;
const VERSION_OFFSET: usize = 8;
const PAGE_SIZE_OFFSET: usize = 12;

/// Zero-based index of a page within a database file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageId(pub u32);

impl PageId {
    fn offset(self) -> u64 {
        self.0 as u64 * PAGE_SIZE as u64
    }
}

impl fmt::Display for PageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// In-memory contents of one page.
pub struct Page {
    bytes: Box<[u8]>,
}

impl Page {
    /// A blank page, used when allocating or as a scratch buffer.
    pub fn zeroed() -> Page {
        Page {
            bytes: vec![0u8; PAGE_SIZE].into_boxed_slice(),
        }
    }

    /// The page bytes.
    pub fn data(&self) -> &[u8] {
        &self.bytes
    }

    /// Mutable page bytes.
    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }
}

impl Deref for Page {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.data()
    }
}

impl DerefMut for Page {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.data_mut()
    }
}

/// Allocates, reads, and writes fixed-size pages in a database file.
pub struct PageManager {
    storage: Storage,
    pages_read: u64,
}

impl PageManager {
    /// Opens the database at `path`.
    ///
    /// An empty file is initialized with a header page and synced. An existing
    /// file must carry the sqltoy magic, format version 3, and this build's
    /// page size, and its length must be a positive multiple of [`PAGE_SIZE`].
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<PageManager> {
        let mut storage = Storage::open(path)?;
        if storage.is_empty()? {
            initialize_header(&mut storage)?;
        } else {
            validate_existing(&mut storage)?;
        }
        Ok(PageManager {
            storage,
            pages_read: 0,
        })
    }

    /// Successful [`Self::read_page`] calls since this manager was opened.
    ///
    /// A page id that is out of range does not count. The counter is not stored
    /// in the file.
    pub fn pages_read(&self) -> u64 {
        self.pages_read
    }

    /// Number of pages in the file, including the header.
    pub fn page_count(&self) -> io::Result<u32> {
        let len = self.storage.len()?;
        Ok((len / PAGE_SIZE as u64) as u32)
    }

    /// Appends a zero-filled page and returns its id.
    ///
    /// The new page is synced before this returns. Page 0 is the header, so
    /// the first page allocated from a fresh database is id 1.
    pub fn allocate_page(&mut self) -> io::Result<PageId> {
        let id = self.allocate_uncommitted()?;
        self.storage.sync()?;
        Ok(id)
    }

    /// Appends a zero-filled page without syncing.
    ///
    /// The caller must [`Self::sync`] before the new page is durable. Used by
    /// the index, which syncs once per insert or delete.
    pub(crate) fn allocate_uncommitted(&mut self) -> io::Result<PageId> {
        let id = PageId(self.page_count()?);
        self.storage.write_at(id.offset(), Page::zeroed().data())?;
        Ok(id)
    }

    /// Reads the page identified by `id`.
    pub fn read_page(&mut self, id: PageId) -> io::Result<Page> {
        self.ensure_in_range(id)?;
        self.pages_read += 1;
        let bytes = self.storage.read_at(id.offset(), PAGE_SIZE)?;
        let mut page = Page::zeroed();
        page.data_mut().copy_from_slice(&bytes);
        Ok(page)
    }

    /// Overwrites the page identified by `id`.
    ///
    /// The write stays buffered until [`Self::sync`].
    pub fn write_page(&mut self, id: PageId, page: &Page) -> io::Result<()> {
        self.ensure_in_range(id)?;
        self.storage.write_at(id.offset(), page.data())
    }

    /// Makes prior page writes durable.
    pub fn sync(&mut self) -> io::Result<()> {
        self.storage.sync()
    }

    /// Format version recorded in the header page.
    pub fn format_version(&mut self) -> io::Result<u32> {
        let page = self.read_page(PageId(0))?;
        Ok(read_u32_le(
            &page.data()[VERSION_OFFSET..VERSION_OFFSET + 4],
        ))
    }

    fn ensure_in_range(&self, id: PageId) -> io::Result<()> {
        let count = self.page_count()?;
        if id.0 >= count {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("page out of range: {}", id.0),
            ));
        }
        Ok(())
    }
}

fn initialize_header(storage: &mut Storage) -> io::Result<()> {
    let mut page = Page::zeroed();
    page.data_mut()[MAGIC_OFFSET..MAGIC.len()].copy_from_slice(MAGIC);
    page.data_mut()[VERSION_OFFSET..VERSION_OFFSET + 4]
        .copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    page.data_mut()[PAGE_SIZE_OFFSET..PAGE_SIZE_OFFSET + 4]
        .copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
    storage.write_at(PageId(0).offset(), page.data())?;
    storage.sync()
}

fn validate_existing(storage: &mut Storage) -> io::Result<()> {
    let len = storage.len()?;
    if len == 0 || len % PAGE_SIZE as u64 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("file length {len} is not a multiple of page size {PAGE_SIZE}"),
        ));
    }

    let header = storage.read_at(0, PAGE_SIZE)?;
    if header[MAGIC_OFFSET..MAGIC.len()] != MAGIC[..] {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a sqltoy database (bad magic)",
        ));
    }

    let stored_page_size = read_u32_le(&header[PAGE_SIZE_OFFSET..PAGE_SIZE_OFFSET + 4]);
    if stored_page_size != PAGE_SIZE as u32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported page size: {stored_page_size}"),
        ));
    }

    let version = read_u32_le(&header[VERSION_OFFSET..VERSION_OFFSET + 4]);
    if version != FORMAT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported format version: {version}"),
        ));
    }
    Ok(())
}

fn read_u32_le(bytes: &[u8]) -> u32 {
    let mut buf = [0u8; 4];
    buf.copy_from_slice(bytes);
    u32::from_le_bytes(buf)
}

#[cfg(test)]
mod tests {
    use super::{Page, PageId, PageManager, PAGE_SIZE};
    use crate::storage::Storage;
    use std::env::temp_dir;
    use std::fs;
    use std::io::ErrorKind;
    use std::path::PathBuf;
    use std::process;
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
    fn fresh_file_has_one_header_page() {
        let db = TempDb::new("header");
        let mut pages = PageManager::open(db.path()).unwrap();

        assert_eq!(pages.page_count().unwrap(), 1);
        assert_eq!(pages.format_version().unwrap(), 3);

        let header = pages.read_page(PageId(0)).unwrap();
        assert_eq!(&header.data()[..8], b"SQLTOYDB");
        assert_eq!(&header.data()[8..12], &3u32.to_le_bytes());
        assert_eq!(&header.data()[12..16], &(PAGE_SIZE as u32).to_le_bytes());
        assert!(header.data()[16..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn allocate_page_returns_sequential_ids() {
        let db = TempDb::new("alloc");
        let mut pages = PageManager::open(db.path()).unwrap();

        let first = pages.allocate_page().unwrap();
        assert_eq!(first, PageId(1));
        assert_eq!(pages.page_count().unwrap(), 2);

        let second = pages.allocate_page().unwrap();
        assert_eq!(second, PageId(2));
        assert_eq!(pages.page_count().unwrap(), 3);

        let page = pages.read_page(first).unwrap();
        assert!(page.data().iter().all(|byte| *byte == 0));
    }

    #[test]
    fn write_then_read_returns_same_bytes() {
        let db = TempDb::new("roundtrip");
        let mut pages = PageManager::open(db.path()).unwrap();
        let id = pages.allocate_page().unwrap();

        let mut page = Page::zeroed();
        for (i, byte) in page.data_mut().iter_mut().enumerate() {
            *byte = u8::try_from(i % 251).unwrap();
        }
        pages.write_page(id, &page).unwrap();

        let got = pages.read_page(id).unwrap();
        assert_eq!(got.data(), page.data());
    }

    #[test]
    fn out_of_range_page_is_rejected() {
        let db = TempDb::new("range");
        let mut pages = PageManager::open(db.path()).unwrap();

        let err = error_of(pages.read_page(PageId(1)));
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "page out of range: 1");

        let err = pages.write_page(PageId(3), &Page::zeroed()).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "page out of range: 3");
    }

    #[test]
    fn bad_magic_is_rejected() {
        let db = TempDb::new("magic");
        {
            let mut storage = Storage::open(db.path()).unwrap();
            let mut bytes = vec![0u8; PAGE_SIZE];
            bytes[..8].copy_from_slice(b"NOTMAGIC");
            storage.write_at(0, &bytes).unwrap();
            storage.sync().unwrap();
        }

        let err = error_of(PageManager::open(db.path()));
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "not a sqltoy database (bad magic)");
    }

    #[test]
    fn unsupported_page_size_is_rejected() {
        let db = TempDb::new("pagesize");
        {
            let mut storage = Storage::open(db.path()).unwrap();
            let mut bytes = vec![0u8; PAGE_SIZE];
            bytes[..8].copy_from_slice(b"SQLTOYDB");
            // Not version 3. Page size is checked before the version.
            bytes[8..12].copy_from_slice(&2u32.to_le_bytes());
            bytes[12..16].copy_from_slice(&512u32.to_le_bytes());
            storage.write_at(0, &bytes).unwrap();
            storage.sync().unwrap();
        }

        let err = error_of(PageManager::open(db.path()));
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "unsupported page size: 512");
    }

    #[test]
    fn unsupported_format_version_is_rejected() {
        let db = TempDb::new("badver");
        {
            let mut storage = Storage::open(db.path()).unwrap();
            let mut bytes = vec![0u8; PAGE_SIZE];
            bytes[..8].copy_from_slice(b"SQLTOYDB");
            bytes[8..12].copy_from_slice(&2u32.to_le_bytes());
            bytes[12..16].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
            storage.write_at(0, &bytes).unwrap();
            storage.sync().unwrap();
        }

        let err = error_of(PageManager::open(db.path()));
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "unsupported format version: 2");
    }

    #[test]
    fn pages_read_counts_successful_reads() {
        let db = TempDb::new("reads");
        let mut pages = PageManager::open(db.path()).unwrap();
        assert_eq!(pages.pages_read(), 0);
        let id = pages.allocate_page().unwrap();
        assert_eq!(pages.pages_read(), 0);
        pages.read_page(id).unwrap();
        pages.read_page(PageId(0)).unwrap();
        assert_eq!(pages.pages_read(), 2);
        assert!(pages.read_page(PageId(9)).is_err());
        assert_eq!(pages.pages_read(), 2);
    }

    #[test]
    fn file_length_must_be_a_multiple_of_page_size() {
        let db = TempDb::new("badlen");
        {
            let mut storage = Storage::open(db.path()).unwrap();
            storage.write_at(0, b"SQLTOYDB").unwrap();
            storage.sync().unwrap();
        }

        let err = error_of(PageManager::open(db.path()));
        assert_eq!(err.kind(), ErrorKind::InvalidData);
    }

    fn error_of<T>(result: std::io::Result<T>) -> std::io::Error {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(err) => err,
        }
    }
}
