use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use sqltoy::{Database, QueryResult, Value};

#[test]
fn primary_key_lookup_survives_reopen_and_skips_the_scan() {
    let path = unique_temp_path("index");
    let _cleanup = TempFile(path.clone());

    {
        let mut db = Database::open(&path).expect("open");
        db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)")
            .expect("create");
        for start in (1..=3000).step_by(100) {
            let values = (start..start + 100)
                .map(|id| format!("({id}, 'user{id}')"))
                .collect::<Vec<_>>()
                .join(", ");
            db.execute(&format!("INSERT INTO users VALUES {values}"))
                .expect("insert");
        }
        let height = db.index_height("users").expect("height").expect("pk");
        assert!(height >= 2, "height {height}");
        assert_lookup(&mut db, height);
    }

    let mut db = Database::open(&path).expect("reopen");
    let height = db.index_height("users").expect("height").expect("pk");
    assert!(height >= 2, "height {height}");
    assert_lookup(&mut db, height);
}

fn assert_lookup(db: &mut Database, height: u32) {
    let before = db.pages_read();
    let results = db
        .execute("SELECT * FROM users WHERE id = 1234")
        .expect("lookup");
    let lookup = db.pages_read() - before;
    assert_eq!(
        results,
        vec![QueryResult::Rows {
            columns: vec!["id".to_string(), "name".to_string()],
            rows: vec![vec![
                Value::Integer(1234),
                Value::Text("user1234".to_string())
            ]],
        }]
    );
    assert!(
        lookup <= u64::from(height) + 2,
        "lookup read {lookup} pages at height {height}"
    );

    let before = db.pages_read();
    db.execute("SELECT * FROM users").expect("scan");
    let scan = db.pages_read() - before;
    assert!(scan > lookup * 5, "scan read {scan}, lookup read {lookup}");
}

fn unique_temp_path(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "sqltoy-index-{label}-{}-{nanos}",
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
