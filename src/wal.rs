//! Write-ahead log beside a database file.
//!
//! The log path is the database path's file name with `-wal` appended, in the
//! same directory. A commit is a run of page images followed by a commit
//! record. [`Wal::read_committed`] keeps every group that ends in a valid
//! commit record and stops at the first torn or corrupt record. Bytes after
//! that point, and page images with no commit record, are not committed.

use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::crash::crash_point;
use crate::crc32::crc32;
use crate::page::{Page, PageId, PAGE_SIZE};

/// Length of the WAL header.
pub const HEADER_LEN: usize = 16;

const MAGIC: &[u8; 8] = b"SQLTOYWL";
const VERSION: u32 = 1;
const PAGE_RECORD: u8 = 1;
const COMMIT_RECORD: u8 = 2;
const PAGE_RECORD_LEN: usize = 1 + 4 + PAGE_SIZE + 4;
const COMMIT_RECORD_LEN: usize = 1 + 4 + 4 + 4;

/// Pages from every complete commit in the log, in log order.
///
/// [`page_count`](Committed::page_count) is the page count stored in the last
/// of those commits. The same page id may appear more than once; the later
/// image is the one to keep.
pub struct Committed {
    /// `(page id, image)` pairs in the order they were logged.
    pub pages: Vec<(PageId, Page)>,
    /// Database page count from the last complete commit.
    pub page_count: u32,
}

/// One `{db}-wal` file.
pub struct Wal {
    file: File,
}

/// Path of the WAL for `db_path`.
///
/// The directory is unchanged. The file name is the database file name plus
/// `-wal`, so `demo.db` uses `demo.db-wal`.
pub fn wal_path(db_path: impl AsRef<Path>) -> PathBuf {
    let db_path = db_path.as_ref();
    let mut name = db_path.file_name().unwrap_or_default().to_os_string();
    name.push("-wal");
    db_path.with_file_name(name)
}

impl Wal {
    /// Opens the WAL for `db_path`, creating it when it is missing.
    ///
    /// A missing file, or a file shorter than [`HEADER_LEN`], is replaced with
    /// a header and synced. A longer file must start with magic `SQLTOYWL`,
    /// version 1, and page size 4096. Anything else is
    /// [`ErrorKind::InvalidData`] (`invalid WAL header`).
    pub fn open<P: AsRef<Path>>(db_path: P) -> io::Result<Wal> {
        let path = wal_path(db_path);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let len = file.metadata()?.len();
        let mut wal = Wal { file };
        if len < HEADER_LEN as u64 {
            wal.install_header()?;
        } else {
            wal.validate_header()?;
        }
        Ok(wal)
    }

    /// Appends page records and one commit record, then syncs.
    ///
    /// `page_count` is the database's logical page count after this commit.
    /// `wal-partial` aborts after the first page record has been written and
    /// not synced. `before-wal-sync` aborts after the commit record is written
    /// and before the sync. `after-wal-sync` aborts after the sync returns.
    pub fn append_commit(&mut self, pages: &[(PageId, &Page)], page_count: u32) -> io::Result<()> {
        let count = u32::try_from(pages.len())
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "too many pages in one commit"))?;
        self.file.seek(SeekFrom::End(0))?;
        for (index, (id, page)) in pages.iter().enumerate() {
            self.file.write_all(&page_record(*id, page))?;
            if index == 0 {
                crash_point("wal-partial");
            }
        }
        self.file.write_all(&commit_record(page_count, count))?;
        crash_point("before-wal-sync");
        self.file.sync_all()?;
        crash_point("after-wal-sync");
        Ok(())
    }

    /// Reads every complete commit, or `Ok(None)` when there is none.
    ///
    /// Reading stops at the end of the file, a record truncated by that end,
    /// a CRC mismatch, an unknown record type, or a commit whose page-record
    /// count does not match the page records since the previous commit. The
    /// partial group is dropped. Earlier complete groups are kept, in order,
    /// and `page_count` comes from the last of them.
    pub fn read_committed(&mut self) -> io::Result<Option<Committed>> {
        let len = self.file.metadata()?.len();
        let mut pos = HEADER_LEN as u64;
        let mut pending = Vec::new();
        let mut pages = Vec::new();
        let mut page_count = None;
        while pos < len {
            let mut kind = [0u8; 1];
            self.read_exact_at(pos, &mut kind)?;
            let record_len = match kind[0] {
                PAGE_RECORD => PAGE_RECORD_LEN,
                COMMIT_RECORD => COMMIT_RECORD_LEN,
                _ => break,
            };
            let record_end = pos.saturating_add(record_len as u64);
            if record_end > len {
                break;
            }
            let mut record = vec![0u8; record_len];
            self.read_exact_at(pos, &mut record)?;
            let body = record_len - 4;
            if crc32(&record[..body]) != read_u32(&record[body..]) {
                break;
            }
            match kind[0] {
                PAGE_RECORD => {
                    let mut page = Page::zeroed();
                    page.data_mut().copy_from_slice(&record[5..5 + PAGE_SIZE]);
                    pending.push((PageId(read_u32(&record[1..5])), page));
                }
                COMMIT_RECORD => {
                    let committed_pages = read_u32(&record[1..5]);
                    let count = read_u32(&record[5..9]);
                    if count as usize != pending.len() {
                        break;
                    }
                    pages.append(&mut pending);
                    page_count = Some(committed_pages);
                }
                _ => break,
            }
            pos = record_end;
        }
        Ok(page_count.map(|page_count| Committed { pages, page_count }))
    }

    /// Truncates the file to the header and syncs it.
    pub fn truncate(&mut self) -> io::Result<()> {
        self.file.set_len(HEADER_LEN as u64)?;
        self.file.sync_all()
    }

    /// Current file length in bytes.
    pub fn len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    /// Whether the file contains no bytes.
    ///
    /// A WAL opened by [`Self::open`] has a header, so this is false.
    pub fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }

    fn install_header(&mut self) -> io::Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header_bytes())?;
        self.file.sync_all()
    }

    fn validate_header(&mut self) -> io::Result<()> {
        let mut buf = [0u8; HEADER_LEN];
        self.read_exact_at(0, &mut buf)?;
        let version = read_u32(&buf[8..12]);
        let page_size = read_u32(&buf[12..16]);
        if &buf[..8] != MAGIC || version != VERSION || page_size != PAGE_SIZE as u32 {
            return Err(io::Error::new(ErrorKind::InvalidData, "invalid WAL header"));
        }
        Ok(())
    }

    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(buf)
    }
}

fn header_bytes() -> [u8; HEADER_LEN] {
    let mut buf = [0u8; HEADER_LEN];
    buf[..8].copy_from_slice(MAGIC);
    buf[8..12].copy_from_slice(&VERSION.to_le_bytes());
    buf[12..16].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
    buf
}

fn page_record(id: PageId, page: &Page) -> Vec<u8> {
    let mut buf = Vec::with_capacity(PAGE_RECORD_LEN);
    buf.push(PAGE_RECORD);
    buf.extend_from_slice(&id.0.to_le_bytes());
    buf.extend_from_slice(page.data());
    let sum = crc32(&buf);
    buf.extend_from_slice(&sum.to_le_bytes());
    buf
}

fn commit_record(page_count: u32, count: u32) -> [u8; COMMIT_RECORD_LEN] {
    let mut buf = [0u8; COMMIT_RECORD_LEN];
    buf[0] = COMMIT_RECORD;
    buf[1..5].copy_from_slice(&page_count.to_le_bytes());
    buf[5..9].copy_from_slice(&count.to_le_bytes());
    let sum = crc32(&buf[..9]);
    buf[9..13].copy_from_slice(&sum.to_le_bytes());
    buf
}

fn read_u32(bytes: &[u8]) -> u32 {
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&bytes[..4]);
    u32::from_le_bytes(buf)
}

#[cfg(test)]
mod tests {
    use super::{wal_path, Wal, HEADER_LEN, PAGE_RECORD_LEN};
    use crate::crc32::crc32;
    use crate::page::{Page, PageId, PAGE_SIZE};
    use std::env::temp_dir;
    use std::fs;
    use std::io::{self, ErrorKind, Seek, SeekFrom, Write};
    use std::path::{Path, PathBuf};
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
            path.push(format!("sqltoy-wal-{label}-{}-{nanos}", process::id()));
            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(wal_path(&path));
            TempDb { path }
        }

        fn path(&self) -> &PathBuf {
            &self.path
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
            let _ = fs::remove_file(wal_path(&self.path));
        }
    }

    fn error_of<T>(result: io::Result<T>) -> io::Error {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(err) => err,
        }
    }

    fn marked(byte: u8) -> Page {
        let mut page = Page::zeroed();
        page.data_mut()[0] = byte;
        page.data_mut()[PAGE_SIZE - 1] = byte;
        page
    }

    fn first(page: &Page) -> u8 {
        page.data()[0]
    }

    #[test]
    fn wal_path_appends_suffix_to_the_file_name() {
        assert_eq!(
            wal_path(Path::new("/tmp/toy.db")),
            PathBuf::from("/tmp/toy.db-wal")
        );
    }

    #[test]
    fn one_commit_roundtrips() {
        let db = TempDb::new("one");
        let mut wal = Wal::open(db.path()).unwrap();
        assert!(wal.read_committed().unwrap().is_none());
        let page = marked(0xAB);
        wal.append_commit(&[(PageId(1), &page)], 2).unwrap();
        let committed = wal.read_committed().unwrap().unwrap();
        assert_eq!(committed.page_count, 2);
        assert_eq!(committed.pages.len(), 1);
        assert_eq!(committed.pages[0].0, PageId(1));
        assert_eq!(committed.pages[0].1.data(), page.data());
        wal.truncate().unwrap();
        assert_eq!(wal.len().unwrap(), HEADER_LEN as u64);
        assert!(wal.read_committed().unwrap().is_none());
        drop(wal);
        let bytes = fs::read(wal_path(db.path())).unwrap();
        assert_eq!(&bytes[..8], b"SQLTOYWL");
        assert_eq!(bytes.len(), HEADER_LEN);
    }

    #[test]
    fn several_commits_keep_log_order_and_the_last_page_count() {
        let db = TempDb::new("many");
        let mut wal = Wal::open(db.path()).unwrap();
        let first_page = marked(1);
        wal.append_commit(&[(PageId(1), &first_page)], 2).unwrap();
        let second = marked(2);
        let third = marked(3);
        wal.append_commit(&[(PageId(1), &second), (PageId(3), &third)], 4)
            .unwrap();
        let committed = wal.read_committed().unwrap().unwrap();
        assert_eq!(committed.page_count, 4);
        assert_eq!(committed.pages.len(), 3);
        assert_eq!(committed.pages[0].0, PageId(1));
        assert_eq!(first(&committed.pages[0].1), 1);
        assert_eq!(committed.pages[1].0, PageId(1));
        assert_eq!(first(&committed.pages[1].1), 2);
        assert_eq!(committed.pages[2].0, PageId(3));
        assert_eq!(first(&committed.pages[2].1), 3);
        assert_eq!(committed.pages[2].1.data()[PAGE_SIZE - 1], 3);
    }

    #[test]
    fn torn_tail_of_the_last_commit_keeps_only_earlier_commits() {
        let db = TempDb::new("torn");
        let mut wal = Wal::open(db.path()).unwrap();
        let kept = marked(4);
        wal.append_commit(&[(PageId(2), &kept)], 3).unwrap();
        let cut = wal.len().unwrap();
        let extra = marked(5);
        let more = marked(6);
        wal.append_commit(&[(PageId(4), &extra), (PageId(5), &more)], 6)
            .unwrap();
        let full = wal.len().unwrap();
        let bytes = fs::read(wal_path(db.path())).unwrap();
        for len in (cut..full).rev() {
            wal.file.set_len(len).unwrap();
            let committed = wal.read_committed().unwrap().expect("earlier commit");
            assert_eq!(committed.page_count, 3, "len {len}");
            assert_eq!(committed.pages.len(), 1, "len {len}");
            assert_eq!(first(&committed.pages[0].1), 4, "len {len}");
        }
        wal.file.set_len(0).unwrap();
        wal.file.seek(SeekFrom::Start(0)).unwrap();
        wal.file.write_all(&bytes).unwrap();
        let committed = wal.read_committed().unwrap().unwrap();
        assert_eq!(committed.page_count, 6);
        assert_eq!(committed.pages.len(), 3);
        assert_eq!(first(&committed.pages[2].1), 6);
    }

    #[test]
    fn single_bit_flips_in_the_last_commit_drop_only_that_commit() {
        let db = TempDb::new("flip");
        let mut wal = Wal::open(db.path()).unwrap();
        let kept = marked(7);
        wal.append_commit(&[(PageId(1), &kept)], 2).unwrap();
        let cut = wal.len().unwrap();
        let dropped = marked(8);
        wal.append_commit(&[(PageId(1), &dropped)], 2).unwrap();
        let full = wal.len().unwrap();
        let bytes = fs::read(wal_path(db.path())).unwrap();
        assert_eq!(bytes.len() as u64, full);
        let start = usize::try_from(cut).unwrap();
        for (index, byte) in bytes.iter().enumerate().skip(start) {
            for bit in 0..8u8 {
                wal.file.seek(SeekFrom::Start(index as u64)).unwrap();
                wal.file.write_all(&[*byte ^ (1 << bit)]).unwrap();
                let committed = wal.read_committed().unwrap().expect("earlier commit");
                assert_eq!(committed.page_count, 2, "byte {index} bit {bit}");
                assert_eq!(committed.pages.len(), 1, "byte {index} bit {bit}");
                assert_eq!(first(&committed.pages[0].1), 7, "byte {index} bit {bit}");
                wal.file.seek(SeekFrom::Start(index as u64)).unwrap();
                wal.file.write_all(&[*byte]).unwrap();
            }
        }
    }

    #[test]
    fn unknown_record_type_stops() {
        let db = TempDb::new("kind");
        let mut wal = Wal::open(db.path()).unwrap();
        let kept = marked(9);
        wal.append_commit(&[(PageId(1), &kept)], 2).unwrap();
        let end = wal.len().unwrap();
        wal.file.seek(SeekFrom::End(0)).unwrap();
        wal.file.write_all(&[0x7F]).unwrap();
        // A later well-formed commit must not be read after the unknown type.
        let later = marked(10);
        let mut tail = Vec::new();
        tail.extend_from_slice(&page_record_bytes(PageId(1), &later));
        tail.extend_from_slice(&commit_record_bytes(2, 1));
        wal.file.write_all(&tail).unwrap();
        let committed = wal.read_committed().unwrap().unwrap();
        assert_eq!(committed.page_count, 2);
        assert_eq!(committed.pages.len(), 1);
        assert_eq!(first(&committed.pages[0].1), 9);
        assert!(wal.len().unwrap() > end);
    }

    #[test]
    fn count_mismatch_stops() {
        let db = TempDb::new("count");
        let mut bytes = header_bytes().to_vec();
        push_page(&mut bytes, 1, 1);
        push_commit(&mut bytes, 2, 1);
        let good = bytes.len();
        push_page(&mut bytes, 2, 2);
        push_commit(&mut bytes, 3, 0);
        push_page(&mut bytes, 3, 3);
        push_commit(&mut bytes, 4, 1);
        fs::write(wal_path(db.path()), &bytes).unwrap();
        let mut wal = Wal::open(db.path()).unwrap();
        let committed = wal.read_committed().unwrap().unwrap();
        assert_eq!(committed.page_count, 2);
        assert_eq!(committed.pages.len(), 1);
        assert_eq!(first(&committed.pages[0].1), 1);
        assert!(bytes.len() > good);
    }

    #[test]
    fn bad_header_is_rejected_and_short_header_is_recreated() {
        let db = TempDb::new("header");
        let path = wal_path(db.path());
        fs::write(&path, header_bytes()).unwrap();
        {
            let mut bytes = header_bytes();
            bytes[0] = b'X';
            fs::write(&path, bytes).unwrap();
            let err = error_of(Wal::open(db.path()));
            assert_eq!(err.kind(), ErrorKind::InvalidData);
            assert_eq!(err.to_string(), "invalid WAL header");
        }
        {
            let mut bytes = header_bytes();
            bytes[8..12].copy_from_slice(&2u32.to_le_bytes());
            fs::write(&path, bytes).unwrap();
            let err = error_of(Wal::open(db.path()));
            assert_eq!(err.to_string(), "invalid WAL header");
        }
        {
            let mut bytes = header_bytes();
            bytes[12..16].copy_from_slice(&512u32.to_le_bytes());
            fs::write(&path, bytes).unwrap();
            let err = error_of(Wal::open(db.path()));
            assert_eq!(err.to_string(), "invalid WAL header");
        }
        fs::write(&path, b"SQLTOY").unwrap();
        let mut wal = Wal::open(db.path()).unwrap();
        assert!(wal.read_committed().unwrap().is_none());
        assert_eq!(wal.len().unwrap(), HEADER_LEN as u64);
        drop(wal);
        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes, header_bytes());

        let missing = TempDb::new("missing");
        assert!(!wal_path(missing.path()).exists());
        let wal = Wal::open(missing.path()).unwrap();
        assert_eq!(wal.len().unwrap(), HEADER_LEN as u64);
        drop(wal);
        assert_eq!(fs::read(wal_path(missing.path())).unwrap(), header_bytes());
    }

    fn header_bytes() -> [u8; HEADER_LEN] {
        super::header_bytes()
    }

    fn page_record_bytes(id: PageId, page: &Page) -> Vec<u8> {
        super::page_record(id, page)
    }

    fn commit_record_bytes(page_count: u32, count: u32) -> [u8; 13] {
        super::commit_record(page_count, count)
    }

    fn push_page(buf: &mut Vec<u8>, id: u32, byte: u8) {
        let start = buf.len();
        buf.push(1);
        buf.extend_from_slice(&id.to_le_bytes());
        buf.resize(start + 5 + PAGE_SIZE, byte);
        buf[start + 5] = byte;
        let sum = crc32(&buf[start..]);
        buf.extend_from_slice(&sum.to_le_bytes());
        assert_eq!(buf.len() - start, PAGE_RECORD_LEN);
    }

    fn push_commit(buf: &mut Vec<u8>, page_count: u32, count: u32) {
        buf.extend_from_slice(&commit_record_bytes(page_count, count));
    }

    #[test]
    fn pending_pages_without_a_commit_are_dropped() {
        let db = TempDb::new("pending");
        let mut wal = Wal::open(db.path()).unwrap();
        let kept = marked(1);
        wal.append_commit(&[(PageId(1), &kept)], 2).unwrap();
        let page = marked(2);
        wal.file.seek(SeekFrom::End(0)).unwrap();
        wal.file
            .write_all(&page_record_bytes(PageId(2), &page))
            .unwrap();
        let committed = wal.read_committed().unwrap().unwrap();
        assert_eq!(committed.pages.len(), 1);
        assert_eq!(first(&committed.pages[0].1), 1);
    }
}
