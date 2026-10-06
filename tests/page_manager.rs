use std::fs;
use std::path::PathBuf;
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use sqltoy::{PageId, PageManager, PAGE_SIZE};

#[test]
fn page_bytes_persist_after_manager_is_dropped() {
    let path = unique_temp_path();
    let _cleanup = TempFile(&path);

    let page_id;
    let count;
    {
        let mut pages = PageManager::open(&path).expect("open");
        page_id = pages.allocate_page().expect("allocate");
        let mut page = pages.read_page(page_id).expect("read");
        let payload = b"page-durable";
        page.data_mut()[..payload.len()].copy_from_slice(payload);
        pages.write_page(page_id, &page).expect("write");
        pages.sync().expect("sync");
        count = pages.page_count().expect("count");
    }

    let mut pages = PageManager::open(&path).expect("reopen validates header");
    assert_eq!(pages.page_count().expect("count after reopen"), count);
    assert_eq!(page_id, PageId(1));
    let page = pages.read_page(page_id).expect("read after reopen");
    let mut expected = vec![0u8; PAGE_SIZE];
    expected[..b"page-durable".len()].copy_from_slice(b"page-durable");
    assert_eq!(page.data(), expected.as_slice());
}

fn unique_temp_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut path = std::env::temp_dir();
    path.push(format!("sqltoy-page-{}-{nanos}", process::id()));
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(sqltoy::wal_path(&path));
    path
}

struct TempFile<'a>(&'a PathBuf);

impl Drop for TempFile<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0);
        let _ = fs::remove_file(sqltoy::wal_path(self.0));
    }
}
