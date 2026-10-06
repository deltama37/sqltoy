//! Fixed-size page cache in front of [`crate::page::PageManager`].
//!
//! Callers read and write copies of a page. A hit copies the frame out. A
//! miss reads the file. A write copies into a frame and marks it dirty, and
//! does not read the old page when the frame is cold. Dirty frames are
//! written on eviction without syncing. [`BufferPool::flush`] writes every
//! dirty frame in page-id order and then syncs.

use std::io::{self, ErrorKind};
use std::path::Path;

use crate::page::{Page, PageId, PageManager};

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
    /// Pages written to disk, including eviction and flush.
    pub pages_written: u64,
    /// Frames dropped to make room, dirty or clean.
    pub evictions: u64,
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

/// LRU cache of fixed-size pages.
///
/// `logical` page count is the file's page count plus pages allocated in
/// memory and not yet reflected by a shorter file. A newly allocated page is
/// a zeroed dirty frame and is not written until it is evicted or flushed.
pub struct BufferPool {
    pages: PageManager,
    frames: Vec<Frame>,
    logical_pages: u32,
    tick: u64,
    stats: BufferStats,
    #[cfg(test)]
    disk_writes: Vec<u32>,
}

impl BufferPool {
    /// Opens the database at `path` with `frames` slots.
    ///
    /// `frames` must be at least 1. An empty file is initialized by the page
    /// manager before any frame is filled.
    pub fn open<P: AsRef<Path>>(path: P, frames: usize) -> io::Result<BufferPool> {
        if frames < 1 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "buffer pool needs at least one frame",
            ));
        }
        let pages = PageManager::open(path)?;
        let logical_pages = pages.page_count()?;
        let mut pool_frames = Vec::with_capacity(frames);
        for _ in 0..frames {
            pool_frames.push(Frame::empty());
        }
        Ok(BufferPool {
            pages,
            frames: pool_frames,
            logical_pages,
            tick: 0,
            stats: BufferStats::default(),
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
    /// frame is full, the least recently used frame is evicted first, and
    /// that eviction writes when the victim is dirty.
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
    /// A page that is still in a frame is returned from that frame, even when
    /// an earlier eviction extended the file with a hole of zeros at `id`.
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
    /// until eviction or [`Self::flush`].
    pub fn write_page(&mut self, id: PageId, page: &Page) -> io::Result<()> {
        self.ensure_in_range(id)?;
        if let Some(index) = self.find(id) {
            self.install(index, id, page, true);
            return Ok(());
        }
        let index = self.acquire_frame()?;
        self.install(index, id, page, true);
        Ok(())
    }

    /// Writes every dirty frame in ascending page id order, then syncs.
    ///
    /// Dirty flags are cleared. Frames stay cached. Sync runs even when
    /// nothing is dirty, so writes from an earlier eviction become durable.
    pub fn flush(&mut self) -> io::Result<()> {
        let ids = self.dirty_ids_ascending();
        for id in ids {
            let index = self
                .find(id)
                .expect("dirty page stays resident until flush");
            self.write_out(index)?;
        }
        self.pages.sync()
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
        let index = self
            .frames
            .iter()
            .enumerate()
            .filter(|(_, frame)| frame.id.is_some())
            .min_by_key(|(_, frame)| frame.tick)
            .map(|(index, _)| index)
            .expect("pool has a frame to evict");
        self.evict_at(index)?;
        Ok(index)
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
    fn dirty_eviction_writes_without_sync_and_a_later_read_sees_it() {
        let db = TempDb::new("evict");
        let mut pool = BufferPool::open(db.path(), 1).unwrap();
        let first = pool.allocate_page().unwrap();
        pool.write_page(first, &marked(0xAB)).unwrap();
        let second = pool.allocate_page().unwrap();
        assert_eq!(pool.stats().evictions, 1);
        assert_eq!(pool.stats().pages_written, 1);
        assert_eq!(pool.read_page(first).unwrap().data()[0], 0xAB);
        assert!(pool.stats().evictions >= 2);
        assert_eq!(pool.read_page(second).unwrap().data()[0], 0);
        drop(pool);

        let mut pool = BufferPool::open(db.path(), 1).unwrap();
        assert_eq!(pool.read_page(first).unwrap().data()[PAGE_SIZE - 1], 0xAB);
        assert_eq!(pool.page_count().unwrap(), 3);
    }

    #[test]
    fn eviction_of_a_high_page_does_not_hide_a_resident_lower_page() {
        let db = TempDb::new("hole");
        let mut pool = BufferPool::open(db.path(), 2).unwrap();
        let mut ids = Vec::new();
        for byte in 1..=3 {
            let id = pool.allocate_page().unwrap();
            pool.write_page(id, &marked(byte)).unwrap();
            ids.push(id);
        }
        // Pages 2 and 3 are cached. Touch page 2, then allocate page 4, which
        // evicts page 3 past the file end and leaves a hole where page 2 sits.
        pool.read_page(ids[1]).unwrap();
        let fourth = pool.allocate_page().unwrap();
        assert_eq!(fourth, PageId(4));
        let raw = fs::read(db.path()).unwrap();
        assert_eq!(raw.len(), 4 * PAGE_SIZE);
        assert!(raw[2 * PAGE_SIZE..3 * PAGE_SIZE]
            .iter()
            .all(|byte| *byte == 0));
        assert_eq!(raw[3 * PAGE_SIZE], 3);
        assert_eq!(pool.read_page(ids[1]).unwrap().data()[0], 2);

        let start = pool.disk_writes.len();
        assert_eq!(pool.dirty_ids_ascending(), vec![PageId(2), PageId(4)]);
        pool.flush().unwrap();
        assert_eq!(&pool.disk_writes[start..], &[2, 4]);
        drop(pool);

        let mut pages = PageManager::open(db.path()).unwrap();
        assert_eq!(pages.page_count().unwrap(), 5);
        assert_eq!(pages.read_page(PageId(1)).unwrap().data()[0], 1);
        assert_eq!(pages.read_page(PageId(2)).unwrap().data()[0], 2);
        assert_eq!(pages.read_page(PageId(3)).unwrap().data()[0], 3);
        assert_eq!(pages.read_page(PageId(4)).unwrap().data()[0], 0);
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
}
