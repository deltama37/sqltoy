use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use sqltoy::{PageId, PageManager, RecordFile};

#[test]
fn records_persist_after_reopen() {
    let path = unique_temp_path("persist");
    let _cleanup = TempFile(&path);

    let alice;
    let bob;
    {
        let mut records = RecordFile::open(&path).expect("open");
        alice = records.insert(b"Alice").expect("insert alice");
        bob = records.insert(b"Bob").expect("insert bob");
        records.update(alice, b"Alicia").expect("update");
        records.delete(bob).expect("delete");
    }

    let mut records = RecordFile::open(&path).expect("reopen");
    assert_eq!(records.get(alice).expect("get"), b"Alicia");
    let missing = records.get(bob).expect_err("deleted");
    assert_eq!(missing.kind(), ErrorKind::NotFound);
    assert_eq!(missing.to_string(), format!("record not found: {bob}"));
    assert_eq!(
        records.scan().expect("scan"),
        vec![(alice, b"Alicia".to_vec())]
    );
    drop(records);

    let mut pages = PageManager::open(&path).expect("pages");
    assert_eq!(pages.format_version().expect("version"), 1);
}

#[test]
fn records_spill_across_pages() {
    let path = unique_temp_path("spill");
    let _cleanup = TempFile(&path);

    let payloads: Vec<Vec<u8>> = (0u8..10).map(|i| vec![i; 1000]).collect();
    let ids;
    {
        let mut records = RecordFile::open(&path).expect("open");
        ids = payloads
            .iter()
            .map(|bytes| records.insert(bytes).expect("insert"))
            .collect::<Vec<_>>();
        assert!(ids.iter().any(|id| id.page_id.0 > 1));
        assert!(ids.iter().any(|id| id.page_id == PageId(1)));
    }

    let mut records = RecordFile::open(&path).expect("reopen");
    for (id, payload) in ids.iter().zip(&payloads) {
        assert_eq!(records.get(*id).expect("get"), *payload);
    }
    let scanned = records.scan().expect("scan");
    assert_eq!(scanned.len(), payloads.len());
    for (got, (expected_id, expected_bytes)) in scanned.iter().zip(ids.iter().zip(&payloads)) {
        assert_eq!(got.0, *expected_id);
        assert_eq!(got.1, *expected_bytes);
    }
}

#[test]
fn deleted_space_is_reused() {
    let path = unique_temp_path("reuse");
    let _cleanup = TempFile(&path);

    let mut records = RecordFile::open(&path).expect("open");
    let mut ids = Vec::new();
    for i in 0u8..4 {
        ids.push(records.insert(&[i; 1000]).expect("insert"));
    }
    assert!(ids.iter().all(|id| id.page_id == PageId(1)));

    records.delete(ids[0]).expect("delete first");
    records.delete(ids[2]).expect("delete third");
    let reused = records.insert(&[9u8; 1000]).expect("reuse");
    assert_eq!(reused.page_id, PageId(1));
    assert_eq!(reused.slot_id, ids[0].slot_id);
    assert_eq!(records.get(reused).expect("reused"), vec![9u8; 1000]);
    assert_eq!(records.get(ids[1]).expect("kept"), vec![1u8; 1000]);
    assert_eq!(records.get(ids[3]).expect("kept"), vec![3u8; 1000]);
    let missing = records.get(ids[2]).expect_err("still deleted");
    assert_eq!(missing.kind(), ErrorKind::NotFound);
    drop(records);

    let pages = PageManager::open(&path).expect("pages");
    assert_eq!(pages.page_count().expect("count"), 2);
}

fn unique_temp_path(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut path = std::env::temp_dir();
    path.push(format!("sqltoy-record-{label}-{}-{nanos}", process::id()));
    let _ = fs::remove_file(&path);
    path
}

struct TempFile<'a>(&'a PathBuf);

impl Drop for TempFile<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0);
    }
}
