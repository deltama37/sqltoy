//! Page cache in front of [`crate::page::PageManager`].
//!
//! Callers read and write copies of a page. A hit copies the frame out. A
//! miss reads the file. A write copies into a frame and marks it dirty, and
//! does not read the old page when the frame is cold. Dirty frames are never
//! evicted (no-steal): when every frame is dirty the pool grows past its
//! configured capacity and shrinks back after [`BufferPool::flush`] or
//! [`BufferPool::discard_dirty`]. [`BufferPool::flush`] commits dirty frames
//! to the write-ahead log and then checkpoints them into the database file.
//! [`BufferPool::open`] replays a committed log before serving pages.

use std::collections::BTreeMap;
use std::io::{self, ErrorKind};
use std::path::Path;

#[cfg(test)]
use std::cell::Cell;

use crate::crash::crash_point;
use crate::page::{Page, PageId, PageManager, PAGE_SIZE};
use crate::wal::{Wal, HEADER_LEN};

/// Default number of frames: 256 pages, 1 MiB.
pub const DEFAULT_POOL_PAGES: usize = 256;

/// Counters for one buffer pool since it was opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BufferStats {
    /// Successful [`BufferPool::read_page`] calls, hits and misses together.
    pub logical_reads: u64,
    /// Reads served from a frame.
    pub hits: u64,
    /// Reads that loaded a page from disk.
    pub misses: u64,
    /// Pages written to disk, including flush.
    pub pages_written: u64,
    /// Resident frames dropped to make room or to return to capacity.
    pub evictions: u64,
    /// Peak frame count while the pool was above its configured capacity.
    ///
    /// Stays 0 until a dirty workload forces the pool to grow.
    pub max_frames: u64,
}

/// Prior contents of one page, recorded the first time it is written after a
/// savepoint.
struct PageImage {
    /// `None` when the page was not resident. It was clean on disk, and
    /// rollback drops the frame so the next read loads the file.
    bytes: Option<Page>,
    dirty: bool,
}

struct Savepoint {
    logical_pages: u32,
    images: BTreeMap<u32, PageImage>,
}

#[cfg(test)]
thread_local! {
    static WRITE_FAILPOINT: Cell<Option<u64>> = const { Cell::new(None) };
}

/// Allows `allow` later [`BufferPool::write_page`] calls, then returns an error.
///
/// Record storage and the B+Tree both write through that method, so the
/// counter can fail a statement between two page updates. Zero fails the next
/// write. The counter is per thread.
#[cfg(test)]
pub(crate) fn arm_write_failpoint(allow: u64) {
    WRITE_FAILPOINT.with(|slot| slot.set(Some(allow)));
}

/// Disarms [`arm_write_failpoint`].
#[cfg(test)]
pub(crate) fn clear_write_failpoint() {
    WRITE_FAILPOINT.with(|slot| slot.set(None));
}

#[cfg(test)]
fn failpoint_before_write() -> io::Result<()> {
    WRITE_FAILPOINT.with(|slot| match slot.get() {
        None => Ok(()),
        Some(0) => Err(io::Error::new(ErrorKind::Other, "injected write failure")),
        Some(left) => {
            slot.set(Some(left - 1));
            Ok(())
        }
    })
}

struct Frame {
    id: Option<PageId>,
    page: Page,
    dirty: bool,
    /// Last access tick. Larger is newer. Empty frames are not victims.
    tick: u64,
}

impl Frame {
    fn empty() -> Frame {
        Frame {
            id: None,
            page: Page::zeroed(),
            dirty: false,
            tick: 0,
        }
    }
}

/// LRU cache of pages.
///
/// `logical` page count is the file's page count plus pages allocated in
/// memory and not yet reflected by a shorter file. A newly allocated page is
/// a zeroed dirty frame and is not written until [`Self::flush`]. Dirty
/// frames stay resident. The pool may hold more than `capacity` frames until
/// flush or [`Self::discard_dirty`].
pub struct BufferPool {
    pages: PageManager,
    wal: Wal,
    frames: Vec<Frame>,
    /// Configured frame count. [`Self::frame_count`] may be higher.
    capacity: usize,
    logical_pages: u32,
    tick: u64,
    stats: BufferStats,
    savepoint: Option<Savepoint>,
    #[cfg(test)]
    disk_writes: Vec<u32>,
}

impl BufferPool {
    /// Opens the database at `path` with `frames` slots.
    ///
    /// `frames` must be at least 1. An empty file is initialized by the page
    /// manager before any frame is filled. The page manager checks the header
    /// page before the WAL is replayed. Committed WAL pages are then written
    /// in log order, the file is sized to the last commit's page count, and
    /// both files are synced. The WAL is truncated to its header whenever it
    /// is longer than that header, including when the extra bytes are only a
    /// torn tail, so a later commit is not appended after them. Replay writes
    /// the same images again, so opening twice leaves the same database.
    pub fn open<P: AsRef<Path>>(path: P, frames: usize) -> io::Result<BufferPool> {
        if frames < 1 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "buffer pool needs at least one frame",
            ));
        }
        let path = path.as_ref();
        let mut pages = PageManager::open(path)?;
        let mut wal = Wal::open(path)?;
        recover(&mut pages, &mut wal)?;
        let logical_pages = pages.page_count()?;
        let mut pool_frames = Vec::with_capacity(frames);
        for _ in 0..frames {
            pool_frames.push(Frame::empty());
        }
        Ok(BufferPool {
            pages,
            wal,
            frames: pool_frames,
            capacity: frames,
            logical_pages,
            tick: 0,
            stats: BufferStats::default(),
            savepoint: None,
            #[cfg(test)]
            disk_writes: Vec::new(),
        })
    }

    /// Pages visible to callers: file pages plus unflushed allocations.
    pub fn page_count(&self) -> io::Result<u32> {
        Ok(self.logical_pages)
    }

    /// Reserves a new zeroed page at the end of the logical file.
    ///
    /// The page is a dirty frame. This does not write that page. If every
    /// frame is full, the least recently used clean frame is evicted. When
    /// no clean frame can be evicted, the pool grows by one frame.
    pub fn allocate_page(&mut self) -> io::Result<PageId> {
        let index = self.acquire_frame()?;
        let id = PageId(self.logical_pages);
        self.logical_pages = self
            .logical_pages
            .checked_add(1)
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "page id space exhausted"))?;
        let tick = self.bump();
        let frame = &mut self.frames[index];
        frame.id = Some(id);
        frame.page.data_mut().fill(0);
        frame.dirty = true;
        frame.tick = tick;
        Ok(id)
    }

    /// Copies out the page identified by `id`.
    ///
    /// A page that is still in a frame is returned from that frame.
    pub fn read_page(&mut self, id: PageId) -> io::Result<Page> {
        self.ensure_in_range(id)?;
        if let Some(index) = self.find(id) {
            self.stats.logical_reads += 1;
            self.stats.hits += 1;
            let tick = self.bump();
            self.frames[index].tick = tick;
            return Ok(copy_page(&self.frames[index].page));
        }
        let index = self.acquire_frame()?;
        let page = self.pages.read_page(id)?;
        self.stats.logical_reads += 1;
        self.stats.misses += 1;
        self.install(index, id, &page, false);
        Ok(page)
    }

    /// Copies `page` into a frame and marks it dirty.
    ///
    /// The file is not read when `id` is not cached, and it is not written
    /// until [`Self::flush`]. The first write of each page after
    /// [`Self::set_savepoint`] records the page's prior bytes and dirty flag.
    pub fn write_page(&mut self, id: PageId, page: &Page) -> io::Result<()> {
        self.ensure_in_range(id)?;
        #[cfg(test)]
        failpoint_before_write()?;
        self.capture_before_modify(id);
        if let Some(index) = self.find(id) {
            self.install(index, id, page, true);
            return Ok(());
        }
        let index = self.acquire_frame()?;
        self.install(index, id, page, true);
        Ok(())
    }

    /// Commits dirty frames, then checkpoints them into the database file.
    ///
    /// When no frame is dirty this returns without reading or writing either
    /// file. Sync runs only for a commit that has pages to make durable.
    /// Extra frames are still dropped until the pool is back to its
    /// configured capacity.
    ///
    /// Otherwise:
    /// 1. Append the dirty pages in ascending page id order and a commit
    ///    record, then sync the WAL. That sync is the commit point. Dirty
    ///    flags are cleared here. The frames hold the committed bytes, and a
    ///    crash before the checkpoint finishes is repaired by replaying the
    ///    WAL on the next open.
    /// 2. Write those pages to the database file and sync it.
    ///    `mid-checkpoint` aborts after the first of those writes.
    ///    `before-wal-truncate` aborts after the database sync.
    /// 3. Truncate the WAL to its header and sync it.
    ///
    /// A checkpoint error after the commit point is returned. The commit
    /// stays in the WAL. Frames are already clean, so they match the
    /// committed images until the pool is dropped.
    pub fn flush(&mut self) -> io::Result<()> {
        let ids = self.dirty_ids_ascending();
        if ids.is_empty() {
            self.shrink();
            return Ok(());
        }
        let mut stored = Vec::with_capacity(ids.len());
        for id in &ids {
            let index = self
                .find(*id)
                .expect("dirty page stays resident until flush");
            stored.push((*id, copy_page(&self.frames[index].page)));
        }
        let borrowed: Vec<(PageId, &Page)> = stored.iter().map(|(id, page)| (*id, page)).collect();
        self.wal.append_commit(&borrowed, self.logical_pages)?;
        for id in &ids {
            let index = self
                .find(*id)
                .expect("dirty page stays resident until flush");
            self.frames[index].dirty = false;
        }
        for (index, (id, page)) in stored.iter().enumerate() {
            self.pages.write_page_extending(*id, page)?;
            #[cfg(test)]
            self.disk_writes.push(id.0);
            self.stats.pages_written += 1;
            if index == 0 {
                crash_point("mid-checkpoint");
            }
        }
        self.pages.sync()?;
        crash_point("before-wal-truncate");
        self.wal.truncate()?;
        self.shrink();
        Ok(())
    }

    /// Drops every dirty frame and forgets pages that were only allocated in
    /// memory.
    ///
    /// Logical page count becomes the on-disk page count. An active savepoint
    /// is cleared. The pool then shrinks toward its configured capacity.
    pub fn discard_dirty(&mut self) -> io::Result<()> {
        self.savepoint = None;
        let disk_pages = self.pages.page_count()?;
        self.frames.retain(|frame| {
            if frame.dirty {
                return false;
            }
            match frame.id {
                Some(id) => id.0 < disk_pages,
                None => true,
            }
        });
        self.logical_pages = disk_pages;
        self.shrink();
        Ok(())
    }

    /// Records a baseline for [`Self::rollback_to_savepoint`].
    ///
    /// Only one savepoint is kept. A second call is
    /// [`ErrorKind::InvalidInput`] (`savepoint already set`).
    pub fn set_savepoint(&mut self) -> io::Result<()> {
        if self.savepoint.is_some() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "savepoint already set",
            ));
        }
        self.savepoint = Some(Savepoint {
            logical_pages: self.logical_pages,
            images: BTreeMap::new(),
        });
        Ok(())
    }

    /// Restores pages to the state at [`Self::set_savepoint`] and keeps it active.
    ///
    /// Recorded images are copied back with their prior dirty flags. Pages
    /// that were not resident are dropped so the next read loads the file.
    /// Frames whose page id is at or above the logical count from the
    /// savepoint are dropped, and the logical count is restored. A missing
    /// savepoint is [`ErrorKind::InvalidInput`] (`no savepoint`).
    pub fn rollback_to_savepoint(&mut self) -> io::Result<()> {
        if self.savepoint.is_none() {
            return Err(io::Error::new(ErrorKind::InvalidInput, "no savepoint"));
        }
        let logical = self
            .savepoint
            .as_ref()
            .expect("savepoint checked")
            .logical_pages;
        let images =
            std::mem::take(&mut self.savepoint.as_mut().expect("savepoint checked").images);
        for (raw, image) in images {
            self.restore_image(PageId(raw), image);
        }
        for frame in &mut self.frames {
            if frame.id.is_some_and(|id| id.0 >= logical) {
                frame.id = None;
                frame.dirty = false;
                frame.tick = 0;
            }
        }
        self.logical_pages = logical;
        Ok(())
    }

    /// Forgets the savepoint without restoring pages.
    ///
    /// A missing savepoint is [`ErrorKind::InvalidInput`] (`no savepoint`).
    pub fn release_savepoint(&mut self) -> io::Result<()> {
        if self.savepoint.take().is_none() {
            return Err(io::Error::new(ErrorKind::InvalidInput, "no savepoint"));
        }
        Ok(())
    }

    /// Frames currently held, including overflow past the configured capacity.
    #[cfg(test)]
    pub(crate) fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Counters since [`Self::open`].
    pub fn stats(&self) -> BufferStats {
        self.stats
    }

    fn dirty_ids_ascending(&self) -> Vec<PageId> {
        let mut ids = Vec::new();
        for frame in &self.frames {
            if frame.dirty {
                ids.push(frame.id.expect("dirty frame has a page id"));
            }
        }
        ids.sort_unstable();
        ids
    }

    fn ensure_in_range(&self, id: PageId) -> io::Result<()> {
        if id.0 >= self.logical_pages {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("page out of range: {}", id.0),
            ));
        }
        Ok(())
    }

    fn find(&self, id: PageId) -> Option<usize> {
        self.frames.iter().position(|frame| frame.id == Some(id))
    }

    fn bump(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    fn install(&mut self, index: usize, id: PageId, page: &Page, dirty: bool) {
        let tick = self.bump();
        let frame = &mut self.frames[index];
        frame.id = Some(id);
        frame.page.data_mut().copy_from_slice(page.data());
        frame.dirty = dirty;
        frame.tick = tick;
    }

    fn acquire_frame(&mut self) -> io::Result<usize> {
        if let Some(index) = self.frames.iter().position(|frame| frame.id.is_none()) {
            return Ok(index);
        }
        if let Some(index) = self.oldest_clean() {
            self.evict_at(index)?;
            return Ok(index);
        }
        self.frames.push(Frame::empty());
        self.note_overflow();
        Ok(self.frames.len() - 1)
    }

    fn oldest_clean(&self) -> Option<usize> {
        self.frames
            .iter()
            .enumerate()
            .filter(|(_, frame)| frame.id.is_some() && !frame.dirty)
            .min_by_key(|(_, frame)| frame.tick)
            .map(|(index, _)| index)
    }

    fn note_overflow(&mut self) {
        let len = self.frames.len();
        if len > self.capacity {
            let len = len as u64;
            if len > self.stats.max_frames {
                self.stats.max_frames = len;
            }
        }
    }

    /// Drops clean frames, least recently used first, until `capacity`.
    fn shrink(&mut self) {
        while self.frames.len() > self.capacity {
            let Some(index) = self
                .frames
                .iter()
                .enumerate()
                .filter(|(_, frame)| !frame.dirty)
                .min_by_key(|(_, frame)| frame.tick)
                .map(|(index, _)| index)
            else {
                break;
            };
            if self.frames[index].id.is_some() {
                self.stats.evictions += 1;
            }
            self.frames.remove(index);
        }
    }

    fn capture_before_modify(&mut self, id: PageId) {
        let Some(logical) = self.savepoint.as_ref().map(|save| save.logical_pages) else {
            return;
        };
        if id.0 >= logical {
            return;
        }
        if self
            .savepoint
            .as_ref()
            .expect("savepoint checked")
            .images
            .contains_key(&id.0)
        {
            return;
        }
        let image = if let Some(index) = self.find(id) {
            PageImage {
                bytes: Some(copy_page(&self.frames[index].page)),
                dirty: self.frames[index].dirty,
            }
        } else {
            PageImage {
                bytes: None,
                dirty: false,
            }
        };
        self.savepoint
            .as_mut()
            .expect("savepoint checked")
            .images
            .insert(id.0, image);
    }

    fn restore_image(&mut self, id: PageId, image: PageImage) {
        let Some(bytes) = image.bytes else {
            if let Some(index) = self.find(id) {
                self.frames[index].id = None;
                self.frames[index].dirty = false;
                self.frames[index].tick = 0;
            }
            return;
        };
        if let Some(index) = self.find(id) {
            self.install(index, id, &bytes, image.dirty);
            return;
        }
        self.frames.push(Frame::empty());
        self.note_overflow();
        let index = self.frames.len() - 1;
        self.install(index, id, &bytes, image.dirty);
    }

    fn evict_at(&mut self, index: usize) -> io::Result<()> {
        self.write_out(index)?;
        self.frames[index].id = None;
        self.stats.evictions += 1;
        Ok(())
    }

    fn write_out(&mut self, index: usize) -> io::Result<()> {
        if !self.frames[index].dirty {
            return Ok(());
        }
        let id = self.frames[index].id.expect("dirty frame has a page id");
        let page = copy_page(&self.frames[index].page);
        self.pages.write_page_extending(id, &page)?;
        #[cfg(test)]
        self.disk_writes.push(id.0);
        self.frames[index].dirty = false;
        self.stats.pages_written += 1;
        Ok(())
    }
}

fn recover(pages: &mut PageManager, wal: &mut Wal) -> io::Result<()> {
    if let Some(committed) = wal.read_committed()? {
        for (id, page) in &committed.pages {
            pages.write_page_extending(*id, page)?;
        }
        let len = u64::from(committed.page_count) * PAGE_SIZE as u64;
        pages.set_len(len)?;
        pages.sync()?;
    }
    // A torn tail must not stay in front of the next commit.
    if wal.len()? > HEADER_LEN as u64 {
        wal.truncate()?;
    }
    Ok(())
}

fn copy_page(page: &Page) -> Page {
    let mut out = Page::zeroed();
    out.data_mut().copy_from_slice(page.data());
    out
}

#[cfg(test)]
mod tests {
    use super::{BufferPool, BufferStats};
    use crate::page::{Page, PageId, PageManager, PAGE_SIZE};
    use std::env::temp_dir;
    use std::fs;
    use std::io::{self, ErrorKind};
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
            path.push(format!("sqltoy-buf-{label}-{}-{nanos}", process::id()));
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

    #[test]
    fn zero_frames_is_rejected() {
        let db = TempDb::new("zero");
        let err = error_of(BufferPool::open(db.path(), 0));
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "buffer pool needs at least one frame");
        assert!(!db.path().exists());
    }

    #[test]
    fn hits_and_misses_follow_reads_and_writes_do_not_read() {
        let db = TempDb::new("stats");
        let mut pool = BufferPool::open(db.path(), 2).unwrap();
        let first = pool.allocate_page().unwrap();
        let second = pool.allocate_page().unwrap();
        pool.write_page(first, &marked(1)).unwrap();
        pool.write_page(second, &marked(2)).unwrap();
        pool.flush().unwrap();
        drop(pool);

        let mut pool = BufferPool::open(db.path(), 2).unwrap();
        assert_eq!(pool.stats(), BufferStats::default());
        assert_eq!(pool.read_page(first).unwrap().data()[0], 1);
        assert_eq!(pool.read_page(first).unwrap().data()[0], 1);
        assert_eq!(pool.read_page(second).unwrap().data()[PAGE_SIZE - 1], 2);
        assert_eq!(
            pool.stats(),
            BufferStats {
                logical_reads: 3,
                hits: 1,
                misses: 2,
                ..BufferStats::default()
            }
        );

        let misses = pool.stats().misses;
        let reads = pool.stats().logical_reads;
        pool.write_page(first, &marked(9)).unwrap();
        assert_eq!(pool.stats().misses, misses);
        assert_eq!(pool.stats().logical_reads, reads);
        assert_eq!(pool.read_page(first).unwrap().data()[0], 9);
        assert_eq!(pool.stats().hits, 2);
        assert_eq!(pool.stats().misses, misses);
    }

    #[test]
    fn write_of_a_cold_page_does_not_read_or_reach_disk_until_flush() {
        let db = TempDb::new("coldwrite");
        let mut pool = BufferPool::open(db.path(), 2).unwrap();
        let id = pool.allocate_page().unwrap();
        pool.write_page(id, &marked(4)).unwrap();
        pool.flush().unwrap();
        drop(pool);

        let mut pool = BufferPool::open(db.path(), 2).unwrap();
        pool.write_page(id, &marked(5)).unwrap();
        assert_eq!(pool.stats().misses, 0);
        assert_eq!(pool.stats().logical_reads, 0);
        assert_eq!(pool.stats().pages_written, 0);
        drop(pool);

        let mut pool = BufferPool::open(db.path(), 2).unwrap();
        assert_eq!(pool.read_page(id).unwrap().data()[0], 4);
        pool.write_page(id, &marked(6)).unwrap();
        pool.flush().unwrap();
        drop(pool);

        let mut pool = BufferPool::open(db.path(), 1).unwrap();
        assert_eq!(pool.read_page(id).unwrap().data()[0], 6);
    }

    #[test]
    fn lru_with_two_frames_evicts_the_oldest_clean_page() {
        let db = TempDb::new("lru2");
        let mut setup = BufferPool::open(db.path(), 4).unwrap();
        let mut ids = Vec::new();
        for byte in 1..=3 {
            let id = setup.allocate_page().unwrap();
            setup.write_page(id, &marked(byte)).unwrap();
            ids.push(id);
        }
        setup.flush().unwrap();
        drop(setup);

        let mut pool = BufferPool::open(db.path(), 2).unwrap();
        pool.read_page(ids[0]).unwrap();
        pool.read_page(ids[1]).unwrap();
        pool.read_page(ids[0]).unwrap();
        pool.read_page(ids[2]).unwrap();
        assert_eq!(pool.stats().evictions, 1);
        assert_eq!(pool.stats().pages_written, 0);
        assert_eq!(pool.stats().misses, 3);
        assert_eq!(pool.read_page(ids[0]).unwrap().data()[0], 1);
        assert_eq!(pool.stats().misses, 3);
        assert_eq!(pool.read_page(ids[1]).unwrap().data()[0], 2);
        assert_eq!(pool.stats().misses, 4);
    }

    #[test]
    fn lru_with_three_frames_keeps_the_recent_pages() {
        let db = TempDb::new("lru3");
        let mut setup = BufferPool::open(db.path(), 8).unwrap();
        let mut ids = Vec::new();
        for byte in 1..=4 {
            let id = setup.allocate_page().unwrap();
            setup.write_page(id, &marked(byte)).unwrap();
            ids.push(id);
        }
        setup.flush().unwrap();
        drop(setup);

        let mut pool = BufferPool::open(db.path(), 3).unwrap();
        pool.read_page(ids[0]).unwrap();
        pool.read_page(ids[1]).unwrap();
        pool.read_page(ids[2]).unwrap();
        pool.read_page(ids[0]).unwrap();
        pool.read_page(ids[3]).unwrap();
        assert_eq!(pool.stats().evictions, 1);
        assert_eq!(pool.stats().pages_written, 0);
        assert_eq!(pool.read_page(ids[0]).unwrap().data()[0], 1);
        assert_eq!(pool.read_page(ids[2]).unwrap().data()[0], 3);
        assert_eq!(pool.stats().misses, 4);
        let misses = pool.stats().misses;
        assert_eq!(pool.read_page(ids[1]).unwrap().data()[0], 2);
        assert_eq!(pool.stats().misses, misses + 1);
    }

    #[test]
    fn dirty_pages_are_not_evicted_and_do_not_reach_disk_without_flush() {
        let db = TempDb::new("evict");
        let mut pool = BufferPool::open(db.path(), 1).unwrap();
        let first = pool.allocate_page().unwrap();
        pool.write_page(first, &marked(0xAB)).unwrap();
        let second = pool.allocate_page().unwrap();
        assert_eq!(pool.frame_count(), 2);
        assert_eq!(pool.stats().evictions, 0);
        assert_eq!(pool.stats().pages_written, 0);
        assert_eq!(pool.stats().max_frames, 2);
        assert_eq!(fs::metadata(db.path()).unwrap().len(), PAGE_SIZE as u64);
        assert_eq!(pool.read_page(first).unwrap().data()[0], 0xAB);
        assert_eq!(pool.read_page(second).unwrap().data()[0], 0);
        assert_eq!(pool.stats().misses, 0);
        drop(pool);

        let mut pool = BufferPool::open(db.path(), 1).unwrap();
        assert_eq!(pool.page_count().unwrap(), 1);
        let err = error_of(pool.read_page(first));
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "page out of range: 1");
    }

    #[test]
    fn overflow_does_not_write_a_hole_ahead_of_a_dirty_page() {
        let db = TempDb::new("hole");
        let mut pool = BufferPool::open(db.path(), 2).unwrap();
        let mut ids = Vec::new();
        for byte in 1..=3 {
            let id = pool.allocate_page().unwrap();
            pool.write_page(id, &marked(byte)).unwrap();
            ids.push(id);
        }
        assert!(pool.frame_count() > 2);
        assert_eq!(pool.stats().pages_written, 0);
        assert_eq!(pool.stats().evictions, 0);
        assert_eq!(fs::metadata(db.path()).unwrap().len(), PAGE_SIZE as u64);
        assert_eq!(pool.read_page(ids[1]).unwrap().data()[0], 2);
        let start = pool.disk_writes.len();
        assert_eq!(
            pool.dirty_ids_ascending(),
            vec![PageId(1), PageId(2), PageId(3)]
        );
        pool.flush().unwrap();
        assert_eq!(&pool.disk_writes[start..], &[1, 2, 3]);
        assert!(pool.frame_count() <= 2);
        drop(pool);

        let mut pages = PageManager::open(db.path()).unwrap();
        assert_eq!(pages.page_count().unwrap(), 4);
        assert_eq!(pages.read_page(PageId(1)).unwrap().data()[0], 1);
        assert_eq!(pages.read_page(PageId(2)).unwrap().data()[0], 2);
        assert_eq!(pages.read_page(PageId(3)).unwrap().data()[0], 3);
    }

    #[test]
    fn flush_persists_in_id_order_across_reopen() {
        let db = TempDb::new("flush");
        let mut pool = BufferPool::open(db.path(), 4).unwrap();
        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(pool.allocate_page().unwrap());
        }
        pool.write_page(ids[2], &marked(3)).unwrap();
        pool.write_page(ids[0], &marked(1)).unwrap();
        pool.write_page(ids[1], &marked(2)).unwrap();
        assert_eq!(pool.dirty_ids_ascending(), ids);
        assert_eq!(pool.stats().pages_written, 0);
        pool.flush().unwrap();
        assert_eq!(pool.disk_writes, vec![1, 2, 3]);
        assert_eq!(pool.stats().pages_written, 3);
        assert!(pool.dirty_ids_ascending().is_empty());
        pool.write_page(ids[1], &marked(8)).unwrap();
        pool.flush().unwrap();
        assert_eq!(pool.disk_writes, vec![1, 2, 3, 2]);
        drop(pool);

        let mut pool = BufferPool::open(db.path(), 2).unwrap();
        assert_eq!(pool.page_count().unwrap(), 4);
        assert_eq!(pool.read_page(ids[0]).unwrap().data()[0], 1);
        assert_eq!(pool.read_page(ids[1]).unwrap().data()[PAGE_SIZE - 1], 8);
        assert_eq!(pool.read_page(ids[2]).unwrap().data()[0], 3);
    }

    #[test]
    fn allocate_does_not_extend_the_file_until_flush() {
        let db = TempDb::new("alloc");
        let mut pool = BufferPool::open(db.path(), 4).unwrap();
        let before = fs::metadata(db.path()).unwrap().len();
        assert_eq!(before, PAGE_SIZE as u64);
        let id = pool.allocate_page().unwrap();
        assert_eq!(id, PageId(1));
        assert_eq!(pool.page_count().unwrap(), 2);
        assert_eq!(fs::metadata(db.path()).unwrap().len(), before);
        assert_eq!(pool.stats().pages_written, 0);
        assert_eq!(pool.read_page(id).unwrap().data(), Page::zeroed().data());
        assert_eq!(pool.stats().misses, 0);
        pool.flush().unwrap();
        assert_eq!(
            fs::metadata(db.path()).unwrap().len(),
            before + PAGE_SIZE as u64
        );
        assert_eq!(pool.stats().pages_written, 1);
        drop(pool);

        let mut pool = BufferPool::open(db.path(), 1).unwrap();
        assert_eq!(pool.page_count().unwrap(), 2);
        assert!(pool
            .read_page(id)
            .unwrap()
            .data()
            .iter()
            .all(|byte| *byte == 0));
    }

    #[test]
    fn one_frame_runs_a_mixed_workload() {
        let db = TempDb::new("one");
        let mut pool = BufferPool::open(db.path(), 1).unwrap();
        let mut ids = Vec::new();
        for byte in 0..8u8 {
            let id = pool.allocate_page().unwrap();
            let mut page = marked(byte);
            page.data_mut()[100] = byte.wrapping_mul(3);
            pool.write_page(id, &page).unwrap();
            ids.push((id, byte));
        }
        for (id, byte) in &ids {
            let page = pool.read_page(*id).unwrap();
            assert_eq!(page.data()[0], *byte);
            assert_eq!(page.data()[100], byte.wrapping_mul(3));
        }
        pool.write_page(ids[3].0, &marked(40)).unwrap();
        pool.flush().unwrap();
        assert!(pool.stats().evictions > 0);
        assert!(pool.stats().pages_written >= 8);
        drop(pool);

        let mut pool = BufferPool::open(db.path(), 1).unwrap();
        assert_eq!(pool.page_count().unwrap(), 9);
        for (id, byte) in &ids {
            let expected = if *byte == 3 { 40 } else { *byte };
            assert_eq!(pool.read_page(*id).unwrap().data()[0], expected);
        }
    }

    #[test]
    fn out_of_range_reads_and_writes_do_not_change_stats() {
        let db = TempDb::new("range");
        let mut pool = BufferPool::open(db.path(), 1).unwrap();
        let err = error_of(pool.read_page(PageId(1)));
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "page out of range: 1");
        let err = error_of(pool.write_page(PageId(4), &Page::zeroed()));
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "page out of range: 4");
        assert_eq!(pool.page_count().unwrap(), 1);
        assert_eq!(pool.stats(), BufferStats::default());
        assert_eq!(pool.stats().evictions, 0);
    }

    fn dirty_of(pool: &BufferPool, id: PageId) -> Option<bool> {
        pool.frames
            .iter()
            .find(|frame| frame.id == Some(id))
            .map(|frame| frame.dirty)
    }

    #[test]
    fn savepoint_restores_bytes_and_dirty_flags() {
        let db = TempDb::new("sp-dirty");
        let mut pool = BufferPool::open(db.path(), 2).unwrap();
        let id = pool.allocate_page().unwrap();
        pool.write_page(id, &marked(1)).unwrap();
        assert_eq!(dirty_of(&pool, id), Some(true));
        pool.set_savepoint().unwrap();
        pool.write_page(id, &marked(2)).unwrap();
        pool.write_page(id, &marked(3)).unwrap();
        assert_eq!(pool.read_page(id).unwrap().data()[0], 3);
        pool.rollback_to_savepoint().unwrap();
        assert_eq!(pool.read_page(id).unwrap().data()[0], 1);
        assert_eq!(pool.read_page(id).unwrap().data()[PAGE_SIZE - 1], 1);
        assert_eq!(dirty_of(&pool, id), Some(true));
        let written = pool.stats().pages_written;
        pool.flush().unwrap();
        assert_eq!(pool.stats().pages_written, written + 1);
        drop(pool);

        let mut pool = BufferPool::open(db.path(), 1).unwrap();
        assert_eq!(pool.read_page(id).unwrap().data()[0], 1);

        pool.write_page(id, &marked(4)).unwrap();
        pool.flush().unwrap();
        pool.set_savepoint().unwrap();
        pool.write_page(id, &marked(5)).unwrap();
        assert_eq!(dirty_of(&pool, id), Some(true));
        pool.rollback_to_savepoint().unwrap();
        assert_eq!(pool.read_page(id).unwrap().data()[0], 4);
        assert_eq!(dirty_of(&pool, id), Some(false));
        let written = pool.stats().pages_written;
        pool.flush().unwrap();
        assert_eq!(pool.stats().pages_written, written);
        pool.release_savepoint().unwrap();
    }

    #[test]
    fn savepoint_drops_pages_allocated_after_it() {
        let db = TempDb::new("sp-alloc");
        let mut pool = BufferPool::open(db.path(), 2).unwrap();
        let kept = pool.allocate_page().unwrap();
        pool.write_page(kept, &marked(7)).unwrap();
        let before = fs::read(db.path()).unwrap();
        let count = pool.page_count().unwrap();
        pool.set_savepoint().unwrap();
        let created = pool.allocate_page().unwrap();
        pool.write_page(created, &marked(8)).unwrap();
        pool.write_page(kept, &marked(9)).unwrap();
        assert_eq!(pool.page_count().unwrap(), count + 1);
        pool.rollback_to_savepoint().unwrap();
        assert_eq!(pool.page_count().unwrap(), count);
        assert_eq!(pool.read_page(kept).unwrap().data()[0], 7);
        assert_eq!(dirty_of(&pool, kept), Some(true));
        let err = error_of(pool.read_page(created));
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(fs::read(db.path()).unwrap(), before);
        pool.release_savepoint().unwrap();
        pool.flush().unwrap();
        drop(pool);

        let mut pool = BufferPool::open(db.path(), 1).unwrap();
        assert_eq!(pool.page_count().unwrap(), count);
        assert_eq!(pool.read_page(kept).unwrap().data()[0], 7);
    }

    #[test]
    fn savepoint_rollback_survives_eviction_of_clean_frames() {
        let db = TempDb::new("sp-evict");
        let mut setup = BufferPool::open(db.path(), 4).unwrap();
        let mut ids = Vec::new();
        for byte in 1..=3 {
            let id = setup.allocate_page().unwrap();
            setup.write_page(id, &marked(byte)).unwrap();
            ids.push(id);
        }
        setup.flush().unwrap();
        drop(setup);

        let mut pool = BufferPool::open(db.path(), 2).unwrap();
        pool.read_page(ids[0]).unwrap();
        pool.read_page(ids[1]).unwrap();
        pool.set_savepoint().unwrap();
        pool.write_page(ids[0], &marked(9)).unwrap();
        assert_eq!(dirty_of(&pool, ids[0]), Some(true));
        let evictions = pool.stats().evictions;
        pool.read_page(ids[2]).unwrap();
        assert_eq!(pool.stats().evictions, evictions + 1);
        assert!(dirty_of(&pool, ids[1]).is_none());
        pool.write_page(ids[2], &marked(8)).unwrap();
        pool.rollback_to_savepoint().unwrap();
        assert_eq!(pool.read_page(ids[0]).unwrap().data()[0], 1);
        assert_eq!(dirty_of(&pool, ids[0]), Some(false));
        assert_eq!(pool.read_page(ids[2]).unwrap().data()[0], 3);
        assert_eq!(dirty_of(&pool, ids[2]), Some(false));
        assert_eq!(pool.read_page(ids[1]).unwrap().data()[0], 2);
        let written = pool.stats().pages_written;
        pool.flush().unwrap();
        assert_eq!(pool.stats().pages_written, written);
    }

    #[test]
    fn savepoint_of_a_cold_page_reloads_from_disk() {
        let db = TempDb::new("sp-cold");
        let mut setup = BufferPool::open(db.path(), 2).unwrap();
        let id = setup.allocate_page().unwrap();
        setup.write_page(id, &marked(4)).unwrap();
        let other = setup.allocate_page().unwrap();
        setup.write_page(other, &marked(1)).unwrap();
        setup.flush().unwrap();
        drop(setup);

        let mut pool = BufferPool::open(db.path(), 1).unwrap();
        pool.read_page(other).unwrap();
        assert!(dirty_of(&pool, id).is_none());
        pool.set_savepoint().unwrap();
        pool.write_page(id, &marked(9)).unwrap();
        assert_eq!(pool.read_page(id).unwrap().data()[0], 9);
        pool.rollback_to_savepoint().unwrap();
        assert!(dirty_of(&pool, id).is_none());
        assert_eq!(pool.read_page(id).unwrap().data()[0], 4);
        assert_eq!(pool.stats().pages_written, 0);
    }

    #[test]
    fn overflow_discard_and_flush_shrink_back_to_capacity() {
        let db = TempDb::new("overflow");
        let mut pool = BufferPool::open(db.path(), 2).unwrap();
        let before = fs::read(db.path()).unwrap();
        for byte in 1..=20 {
            let id = pool.allocate_page().unwrap();
            pool.write_page(id, &marked(byte)).unwrap();
        }
        assert!(pool.frame_count() >= 20);
        assert!(pool.stats().max_frames >= 20);
        assert_eq!(pool.stats().pages_written, 0);
        assert_eq!(fs::read(db.path()).unwrap(), before);
        pool.discard_dirty().unwrap();
        assert_eq!(pool.page_count().unwrap(), 1);
        assert!(pool.frame_count() <= 2);
        assert_eq!(fs::read(db.path()).unwrap(), before);
        assert!(pool.savepoint.is_none());

        for byte in 1..=20 {
            let id = pool.allocate_page().unwrap();
            pool.write_page(id, &marked(byte)).unwrap();
        }
        pool.flush().unwrap();
        assert!(pool.frame_count() <= 2);
        assert_eq!(pool.page_count().unwrap(), 21);
        drop(pool);

        let mut pool = BufferPool::open(db.path(), 2).unwrap();
        assert_eq!(pool.page_count().unwrap(), 21);
        assert_eq!(pool.read_page(PageId(1)).unwrap().data()[0], 1);
        assert_eq!(pool.read_page(PageId(20)).unwrap().data()[0], 20);
    }

    #[test]
    fn savepoint_errors_and_release_keeps_the_write() {
        let db = TempDb::new("sp-err");
        let mut pool = BufferPool::open(db.path(), 1).unwrap();
        let err = error_of(pool.rollback_to_savepoint());
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "no savepoint");
        let err = error_of(pool.release_savepoint());
        assert_eq!(err.to_string(), "no savepoint");
        let id = pool.allocate_page().unwrap();
        pool.set_savepoint().unwrap();
        let err = error_of(pool.set_savepoint());
        assert_eq!(err.to_string(), "savepoint already set");
        pool.write_page(id, &marked(6)).unwrap();
        pool.release_savepoint().unwrap();
        assert_eq!(pool.read_page(id).unwrap().data()[0], 6);
        assert_eq!(dirty_of(&pool, id), Some(true));
        let err = error_of(pool.rollback_to_savepoint());
        assert_eq!(err.to_string(), "no savepoint");
    }

    #[test]
    fn clean_flush_does_not_touch_the_wal_or_the_database() {
        let db = TempDb::new("cleanwal");
        let mut pool = BufferPool::open(db.path(), 1).unwrap();
        let id = pool.allocate_page().unwrap();
        pool.write_page(id, &marked(1)).unwrap();
        pool.flush().unwrap();
        let db_bytes = fs::read(db.path()).unwrap();
        let wal_bytes = fs::read(crate::wal::wal_path(db.path())).unwrap();
        assert_eq!(wal_bytes.len(), crate::wal::HEADER_LEN);
        pool.flush().unwrap();
        assert_eq!(fs::read(db.path()).unwrap(), db_bytes);
        assert_eq!(
            fs::read(crate::wal::wal_path(db.path())).unwrap(),
            wal_bytes
        );
    }

    #[test]
    fn recovery_replays_truncates_and_is_idempotent() {
        let db = TempDb::new("replay");
        let mut pool = BufferPool::open(db.path(), 4).unwrap();
        let id = pool.allocate_page().unwrap();
        pool.write_page(id, &marked(1)).unwrap();
        pool.flush().unwrap();
        drop(pool);

        let updated = marked(2);
        let created = marked(3);
        {
            let mut wal = crate::wal::Wal::open(db.path()).unwrap();
            wal.append_commit(&[(PageId(1), &updated), (PageId(2), &created)], 3)
                .unwrap();
        }
        let mut pool = BufferPool::open(db.path(), 2).unwrap();
        assert_eq!(pool.page_count().unwrap(), 3);
        assert_eq!(pool.read_page(PageId(1)).unwrap().data()[0], 2);
        assert_eq!(pool.read_page(PageId(2)).unwrap().data()[0], 3);
        assert_eq!(
            fs::metadata(crate::wal::wal_path(db.path())).unwrap().len(),
            crate::wal::HEADER_LEN as u64
        );
        drop(pool);

        let mut pool = BufferPool::open(db.path(), 2).unwrap();
        assert_eq!(pool.read_page(PageId(1)).unwrap().data()[0], 2);
        assert_eq!(pool.read_page(PageId(2)).unwrap().data()[PAGE_SIZE - 1], 3);
        let bytes = fs::read(db.path()).unwrap();
        drop(pool);
        let pool = BufferPool::open(db.path(), 1).unwrap();
        assert_eq!(fs::read(db.path()).unwrap(), bytes);
        drop(pool);
    }

    #[test]
    fn recovery_truncates_the_file_to_the_commit_page_count() {
        let db = TempDb::new("shrinkfile");
        let mut pool = BufferPool::open(db.path(), 4).unwrap();
        for byte in 1..=3 {
            let id = pool.allocate_page().unwrap();
            pool.write_page(id, &marked(byte)).unwrap();
        }
        pool.flush().unwrap();
        assert_eq!(pool.page_count().unwrap(), 4);
        drop(pool);

        let kept = marked(9);
        {
            let mut wal = crate::wal::Wal::open(db.path()).unwrap();
            wal.append_commit(&[(PageId(1), &kept)], 2).unwrap();
        }
        let mut pool = BufferPool::open(db.path(), 2).unwrap();
        assert_eq!(pool.page_count().unwrap(), 2);
        assert_eq!(pool.read_page(PageId(1)).unwrap().data()[0], 9);
        let err = error_of(pool.read_page(PageId(2)));
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn recovery_discards_a_torn_tail_so_the_next_commit_is_visible() {
        let db = TempDb::new("tornwal");
        let mut pool = BufferPool::open(db.path(), 2).unwrap();
        let id = pool.allocate_page().unwrap();
        pool.write_page(id, &marked(1)).unwrap();
        pool.flush().unwrap();
        drop(pool);

        {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(crate::wal::wal_path(db.path()))
                .unwrap();
            use std::io::Write;
            file.write_all(b"torn-tail").unwrap();
        }
        let mut pool = BufferPool::open(db.path(), 2).unwrap();
        assert_eq!(pool.read_page(id).unwrap().data()[0], 1);
        assert_eq!(
            fs::metadata(crate::wal::wal_path(db.path())).unwrap().len(),
            crate::wal::HEADER_LEN as u64
        );
        drop(pool);

        let next = marked(4);
        {
            let mut wal = crate::wal::Wal::open(db.path()).unwrap();
            wal.append_commit(&[(id, &next)], 2).unwrap();
        }
        let mut pool = BufferPool::open(db.path(), 1).unwrap();
        assert_eq!(pool.read_page(id).unwrap().data()[0], 4);
        assert_eq!(pool.read_page(id).unwrap().data()[PAGE_SIZE - 1], 4);
        assert_eq!(
            fs::metadata(crate::wal::wal_path(db.path())).unwrap().len(),
            crate::wal::HEADER_LEN as u64
        );
    }
}
