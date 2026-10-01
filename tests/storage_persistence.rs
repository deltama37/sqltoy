use std::fs;
use std::path::PathBuf;
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use sqltoy::Storage;

#[test]
fn bytes_persist_after_storage_is_dropped() {
    let path = unique_temp_path();
    let _cleanup = TempFile(&path);

    {
        let mut storage = Storage::open(&path).expect("open");
        storage
            .write_at(16, b"durable")
            .expect("write at non-zero offset");
        storage.sync().expect("sync");
    }

    let mut reopened = Storage::open(&path).expect("reopen");
    let got = reopened.read_at(16, b"durable".len()).expect("read");
    assert_eq!(got, b"durable");
}

fn unique_temp_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut path = std::env::temp_dir();
    path.push(format!("sqltoy-persist-{}-{nanos}", process::id()));
    path
}

struct TempFile<'a>(&'a PathBuf);

impl Drop for TempFile<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0);
    }
}
