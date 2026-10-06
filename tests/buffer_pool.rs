use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use sqltoy::{Database, QueryResult};

#[test]
fn same_workload_matches_across_pool_sizes() {
    let mut results = Vec::new();
    let mut files = Vec::new();
    for frames in [1usize, 2, 3, 8, 256] {
        let (out, bytes) = run_workload(frames);
        results.push(out);
        files.push(bytes);
    }
    for (index, result) in results.iter().enumerate().skip(1) {
        assert_eq!(
            result, &results[0],
            "results differ for frames index {index}"
        );
    }
    for (index, bytes) in files.iter().enumerate().skip(1) {
        assert_eq!(
            bytes,
            &files[0],
            "file bytes differ for frames index {index} ({} vs {} bytes)",
            bytes.len(),
            files[0].len()
        );
    }
    assert_eq!(files[0].len() % 4096, 0);
    assert!(files[0].len() > 4096);
}

#[test]
fn bulk_insert_writes_far_fewer_pages_than_rows() {
    let path = unique_temp_path("bulk");
    let _cleanup = TempFile(path.clone());
    let mut db = Database::open(&path).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    let values = (0..500)
        .map(|id| format!("({id}, 'n{id}')"))
        .collect::<Vec<_>>()
        .join(", ");
    let before = db.buffer_stats().pages_written;
    db.execute(&format!("INSERT INTO t VALUES {values}"))
        .unwrap();
    let written = db.buffer_stats().pages_written - before;
    assert!(written > 0, "insert wrote no pages");
    assert!(
        written < 20,
        "inserting 500 rows wrote {written} pages; a statement should flush once"
    );
}

#[test]
fn repeated_index_lookups_miss_nothing_after_warmup() {
    let path = unique_temp_path("hits");
    let _cleanup = TempFile(path.clone());
    let mut db = Database::open(&path).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd')")
        .unwrap();
    for id in 1..=4 {
        db.execute(&format!("SELECT name FROM t WHERE id = {id}"))
            .unwrap();
    }
    let misses = db.buffer_stats().misses;
    let hits = db.buffer_stats().hits;
    for _ in 0..3 {
        for id in 1..=4 {
            let rows = db
                .execute(&format!("SELECT name FROM t WHERE id = {id}"))
                .unwrap();
            assert!(matches!(rows[0], QueryResult::Rows { .. }));
        }
    }
    assert_eq!(db.buffer_stats().misses, misses);
    assert!(db.buffer_stats().hits > hits);
}

fn run_workload(frames: usize) -> (Vec<QueryResult>, Vec<u8>) {
    let path = unique_temp_path(&format!("work-{frames}"));
    let _cleanup = TempFile(path.clone());
    let mut results = Vec::new();
    {
        let mut db = Database::open_with_frames(&path, frames).unwrap();
        for sql in script() {
            results.extend(db.execute(&sql).unwrap());
        }
    }
    let bytes = fs::read(&path).unwrap();
    {
        let mut db = Database::open_with_frames(&path, frames).unwrap();
        results.extend(
            db.execute(
                "SELECT id, name FROM users ORDER BY id; \
                 SELECT id, user_id, sku FROM orders ORDER BY id",
            )
            .unwrap(),
        );
    }
    assert_eq!(fs::read(&path).unwrap(), bytes);
    (results, bytes)
}

fn script() -> Vec<String> {
    let users = (1..=24)
        .map(|id| format!("({id}, 'user{id}')"))
        .collect::<Vec<_>>()
        .join(", ");
    let orders = (1..=24)
        .map(|id| {
            let user = (id % 24) + 1;
            format!("({id}, {user}, 'sku{id}')")
        })
        .collect::<Vec<_>>()
        .join(", ");
    let long = "m".repeat(3000);
    vec![
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)".into(),
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, sku TEXT)".into(),
        format!("INSERT INTO users VALUES {users}"),
        format!("INSERT INTO orders VALUES {orders}"),
        format!("UPDATE users SET name = '{long}' WHERE id = 2"),
        "UPDATE users SET name = 'Bobby' WHERE id = 2".into(),
        "UPDATE users SET id = id + 100 WHERE id <= 4".into(),
        "DELETE FROM orders WHERE id % 2 = 0".into(),
        "SELECT users.id, users.name, orders.sku \
             FROM users LEFT JOIN orders ON users.id = orders.user_id \
             ORDER BY users.id, orders.id"
            .into(),
        "SELECT name FROM users WHERE id = 102".into(),
        "SELECT name FROM users WHERE id = 5".into(),
        "DELETE FROM users WHERE id = 5".into(),
        "SELECT id, name FROM users ORDER BY id".into(),
        "INSERT INTO orders VALUES (100, 102, 'pen'), (101, 6, 'cup')".into(),
        "SELECT orders.sku FROM users JOIN orders ON users.id = orders.user_id \
             WHERE users.id = 102 ORDER BY orders.id"
            .into(),
    ]
}

fn unique_temp_path(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "sqltoy-buffer-{label}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(sqltoy::wal_path(&path));
    path
}

struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
        let _ = fs::remove_file(sqltoy::wal_path(&self.0));
    }
}
