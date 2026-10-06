//! Crash recovery through the `sqltoy` binary.
//!
//! `SQLTOY_CRASH_AT` aborts the child at one commit point. A second open, in
//! this process, replays the WAL.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqltoy::wal::Wal;
use sqltoy::{wal_path, BTree, BufferPool, Database, Page, PageId, QueryResult, Value, PAGE_SIZE};

const BASELINE: &str = "\
CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT); \
INSERT INTO users VALUES (1, 'Ann'), (2, 'Bob')";

fn change_sql() -> String {
    let values = (3..=260)
        .map(|id| format!("({id}, 'n{id}')"))
        .collect::<Vec<_>>()
        .join(", ");
    let long = "x".repeat(2500);
    format!(
        "BEGIN; INSERT INTO users VALUES {values}; \
         UPDATE users SET name = '{long}' WHERE id = 1; COMMIT"
    )
}

struct Temp {
    path: PathBuf,
}

impl Temp {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "sqltoy-crash-{label}-{}-{nanos}",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(wal_path(&path));
        Temp { path }
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_file(wal_path(&self.path));
    }
}

fn disable_core_dumps() {
    #[repr(C)]
    struct Rlimit {
        rlim_cur: u64,
        rlim_max: u64,
    }
    extern "C" {
        fn setrlimit(resource: i32, rlim: *const Rlimit) -> i32;
    }
    let limit = Rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    unsafe {
        let _ = setrlimit(4, &limit);
    }
}

fn run_sql(path: &Path, sql: &str, crash: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sqltoy"));
    command.arg("sql").arg(path).arg(sql);
    if let Some(point) = crash {
        command.env("SQLTOY_CRASH_AT", point);
    } else {
        command.env_remove("SQLTOY_CRASH_AT");
    }
    unsafe {
        command.pre_exec(|| {
            disable_core_dumps();
            Ok(())
        });
    }
    command.output().expect("spawn sqltoy")
}

fn assert_aborted(output: &Output) {
    assert!(
        output.status.signal().is_some(),
        "expected abort, status {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn copy_db(src: &Path, dst: &Path) {
    fs::copy(src, dst).expect("copy db");
    fs::copy(wal_path(src), wal_path(dst)).expect("copy wal");
}

fn rows_of(path: &Path) -> Vec<Vec<Value>> {
    let mut db = Database::open(path).unwrap();
    let rows = select(&mut db, "users");
    drop(db);
    let len = fs::metadata(path).unwrap().len();
    assert_eq!(len % PAGE_SIZE as u64, 0, "db length {len}");
    rows
}

fn select(db: &mut Database, table: &str) -> Vec<Vec<Value>> {
    let results = db
        .execute(&format!("SELECT id, name FROM {table} ORDER BY id"))
        .unwrap_or_else(|err| panic!("select {table}: {err}"));
    match &results[0] {
        QueryResult::Rows { rows, .. } => rows.clone(),
        other => panic!("unexpected result {other:?}"),
    }
}

fn ids(rows: &[Vec<Value>]) -> Vec<i64> {
    rows.iter()
        .map(|row| match row[0] {
            Value::Integer(id) => id,
            _ => panic!("id column"),
        })
        .collect()
}

fn assert_same_rows(label: &str, got: &[Vec<Value>], expected: &[Vec<Value>]) {
    assert_eq!(ids(got), ids(expected), "{label}");
    assert_eq!(got, expected, "{label}");
}

fn assert_index(path: &Path, table: &str, rows: &[Vec<Value>]) {
    let mut db = Database::open(path).unwrap();
    for row in rows {
        let Value::Integer(id) = row[0] else {
            panic!("id");
        };
        let found = db
            .execute(&format!("SELECT id, name FROM {table} WHERE id = {id}"))
            .unwrap_or_else(|err| panic!("lookup {id}: {err}"));
        match &found[0] {
            QueryResult::Rows { rows: got, .. } => assert_eq!(got, &vec![row.clone()]),
            other => panic!("unexpected result {other:?}"),
        }
    }
    let schema = db.table(table).unwrap().clone();
    let root = schema.index_root.expect("primary key");
    let owner = schema.id;
    drop(db);

    let mut pool = BufferPool::open(path, 8).unwrap();
    let keys = BTree::open(root, owner)
        .scan_all(&mut pool)
        .unwrap_or_else(|err| panic!("index scan: {err}"));
    assert!(
        keys.len() >= rows.len(),
        "index keys vs rows: {} vs {}",
        keys.len(),
        rows.len()
    );
    assert_eq!(ids_from_keys(&keys), ids(rows));
}

fn ids_from_keys(keys: &[(i64, sqltoy::RecordId)]) -> Vec<i64> {
    let mut ids: Vec<i64> = keys.iter().map(|(key, _)| *key).collect();
    ids.sort();
    ids.dedup();
    ids
}

fn assert_wal_header_only(path: &Path) {
    let bytes = fs::read(wal_path(path)).unwrap();
    assert_eq!(bytes.len(), 16, "wal len");
    assert_eq!(&bytes[..8], b"SQLTOYWL");
}

fn assert_further_insert(path: &Path, table: &str) {
    let mut db = Database::open(path).unwrap();
    db.execute(&format!("INSERT INTO {table} VALUES (100000, 'extra')"))
        .unwrap();
    let got = db
        .execute(&format!("SELECT name FROM {table} WHERE id = 100000"))
        .unwrap();
    match &got[0] {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows, &vec![vec![Value::Text("extra".to_string())]]);
        }
        other => panic!("unexpected result {other:?}"),
    }
}

enum Expect {
    Baseline,
    Full,
    Either,
}

#[test]
fn crash_points_restore_all_or_nothing() {
    let base = Temp::new("base");
    {
        let mut db = Database::open(&base.path).unwrap();
        db.execute(BASELINE).unwrap();
    }
    let baseline = rows_of(&base.path);
    assert_wal_header_only(&base.path);

    let full_db = Temp::new("full");
    copy_db(&base.path, &full_db.path);
    {
        let mut db = Database::open(&full_db.path).unwrap();
        db.execute(&change_sql()).unwrap();
    }
    let full = rows_of(&full_db.path);
    assert_eq!(full.len(), 260);
    assert!(matches!(&full[0][1], Value::Text(text) if text.len() == 2500));
    assert_index(&full_db.path, "users", &full);

    let cases = [
        ("wal-partial", Expect::Baseline),
        ("before-wal-sync", Expect::Either),
        ("after-wal-sync", Expect::Full),
        ("mid-checkpoint", Expect::Full),
        ("before-wal-truncate", Expect::Full),
    ];
    for (point, expect) in cases {
        let crashed = Temp::new(point);
        copy_db(&base.path, &crashed.path);
        let output = run_sql(&crashed.path, &change_sql(), Some(point));
        assert_aborted(&output);
        let got = rows_of(&crashed.path);
        match expect {
            Expect::Baseline => assert_same_rows(point, &got, &baseline),
            Expect::Full => assert_same_rows(point, &got, &full),
            Expect::Either => {
                assert!(
                    got == baseline || got == full,
                    "{point}: got {} rows {:?}, baseline {}, full {}",
                    got.len(),
                    ids(&got),
                    baseline.len(),
                    full.len()
                );
            }
        }
        assert_index(&crashed.path, "users", &got);
        assert_wal_header_only(&crashed.path);
        assert_further_insert(&crashed.path, "users");
    }
}

#[test]
fn explicit_transaction_commit_is_atomic_across_crash_points() {
    let sql = "\
BEGIN; \
INSERT INTO users VALUES (3, 'Cam'); \
INSERT INTO users VALUES (4, 'Dee'); \
UPDATE users SET name = 'Ann2' WHERE id = 1; \
COMMIT";
    let base = Temp::new("txn-base");
    {
        let mut db = Database::open(&base.path).unwrap();
        db.execute(BASELINE).unwrap();
    }
    let baseline = rows_of(&base.path);
    let done = Temp::new("txn-done");
    copy_db(&base.path, &done.path);
    {
        let mut db = Database::open(&done.path).unwrap();
        db.execute(sql).unwrap();
    }
    let committed = rows_of(&done.path);
    assert_eq!(ids(&committed), vec![1, 2, 3, 4]);
    assert!(matches!(&committed[0][1], Value::Text(name) if name == "Ann2"));

    let visible = Temp::new("txn-sync");
    copy_db(&base.path, &visible.path);
    assert_aborted(&run_sql(&visible.path, sql, Some("after-wal-sync")));
    let got = rows_of(&visible.path);
    assert_same_rows("after-wal-sync", &got, &committed);
    assert_index(&visible.path, "users", &got);
    assert_wal_header_only(&visible.path);
    assert_further_insert(&visible.path, "users");

    let hidden = Temp::new("txn-partial");
    copy_db(&base.path, &hidden.path);
    assert_aborted(&run_sql(&hidden.path, sql, Some("wal-partial")));
    let got = rows_of(&hidden.path);
    assert_same_rows("wal-partial", &got, &baseline);
    assert_index(&hidden.path, "users", &got);
    assert_wal_header_only(&hidden.path);
    assert_further_insert(&hidden.path, "users");
}

#[test]
fn dropping_an_open_transaction_leaves_the_wal_header_only() {
    let db = Temp::new("drop-txn");
    {
        let mut database = Database::open(&db.path).unwrap();
        database.execute(BASELINE).unwrap();
        database
            .execute("BEGIN; INSERT INTO users VALUES (99, 'ghost')")
            .unwrap();
        assert!(database.in_transaction());
    }
    assert_wal_header_only(&db.path);
    let rows = rows_of(&db.path);
    assert_eq!(ids(&rows), vec![1, 2]);
}

#[test]
fn killing_a_repl_with_an_open_transaction_discards_it() {
    let db = Temp::new("kill-repl");
    {
        let mut database = Database::open(&db.path).unwrap();
        database.execute(BASELINE).unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_sqltoy"));
    child
        .arg("repl")
        .arg(&db.path)
        .env_remove("SQLTOY_CRASH_AT")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    unsafe {
        child.pre_exec(|| {
            disable_core_dumps();
            Ok(())
        });
    }
    let mut child = child.spawn().expect("spawn repl");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    stdin
        .write_all(b"BEGIN;\nINSERT INTO users VALUES (99, 'ghost');\n")
        .unwrap();
    stdin.flush().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut acc = String::new();
        let mut byte = [0u8; 1];
        loop {
            match stdout.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {
                    acc.push(byte[0] as char);
                    if acc.contains("INSERT 1") {
                        let _ = tx.send(acc);
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx.recv_timeout(Duration::from_secs(15))
        .expect("repl did not print INSERT 1");
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success());
    let rows = rows_of(&db.path);
    assert_eq!(ids(&rows), vec![1, 2]);
    assert_wal_header_only(&db.path);
}

#[test]
fn create_table_follows_the_commit_point() {
    let sql = "CREATE TABLE events (id INTEGER PRIMARY KEY, name TEXT)";
    let present = Temp::new("create-sync");
    assert_aborted(&run_sql(&present.path, sql, Some("after-wal-sync")));
    {
        let mut db = Database::open(&present.path).unwrap();
        assert!(db.table("events").is_some());
        assert!(select(&mut db, "events").is_empty());
    }
    assert_index(&present.path, "events", &[]);
    assert_wal_header_only(&present.path);
    assert_further_insert(&present.path, "events");

    let absent = Temp::new("create-partial");
    assert_aborted(&run_sql(&absent.path, sql, Some("wal-partial")));
    {
        let mut db = Database::open(&absent.path).unwrap();
        assert!(db.table("events").is_none());
        let err = db.execute("SELECT * FROM events").unwrap_err();
        assert_eq!(err.to_string(), "table not found: events");
        db.execute(
            "CREATE TABLE events (id INTEGER PRIMARY KEY, name TEXT); \
             INSERT INTO events VALUES (1, 'a'); \
             SELECT name FROM events",
        )
        .unwrap();
    }
    assert_wal_header_only(&absent.path);
}

#[test]
fn garbage_appended_to_the_wal_is_ignored() {
    let db = Temp::new("garbage");
    {
        let mut database = Database::open(&db.path).unwrap();
        database.execute(BASELINE).unwrap();
    }
    let baseline = rows_of(&db.path);
    {
        let mut file = OpenOptions::new()
            .append(true)
            .open(wal_path(&db.path))
            .unwrap();
        file.write_all(b"GARBAGE-TAIL").unwrap();
    }
    assert!(fs::metadata(wal_path(&db.path)).unwrap().len() > 16);
    assert_same_rows("garbage", &rows_of(&db.path), &baseline);
    assert_wal_header_only(&db.path);
    assert_further_insert(&db.path, "users");
}

#[test]
fn a_truncated_second_commit_recovers_only_the_first() {
    let db = Temp::new("two-commits");
    {
        let mut database = Database::open(&db.path).unwrap();
        database.execute(BASELINE).unwrap();
    }
    let state0 = fs::read(&db.path).unwrap();
    {
        let mut database = Database::open(&db.path).unwrap();
        database
            .execute("INSERT INTO users VALUES (3, 'Cam')")
            .unwrap();
    }
    let state1 = rows_of(&db.path);
    let pages1 = read_pages(&db.path);
    {
        let mut database = Database::open(&db.path).unwrap();
        database
            .execute("INSERT INTO users VALUES (4, 'Dee')")
            .unwrap();
    }
    let pages2 = read_pages(&db.path);
    fs::write(&db.path, &state0).unwrap();
    let mid = {
        let mut wal = Wal::open(&db.path).unwrap();
        let refs1: Vec<(PageId, &Page)> = pages1.iter().map(|(id, page)| (*id, page)).collect();
        wal.append_commit(&refs1, pages1.len() as u32).unwrap();
        let mid = wal.len().unwrap();
        let refs2: Vec<(PageId, &Page)> = pages2.iter().map(|(id, page)| (*id, page)).collect();
        wal.append_commit(&refs2, pages2.len() as u32).unwrap();
        mid
    };
    OpenOptions::new()
        .write(true)
        .open(wal_path(&db.path))
        .unwrap()
        .set_len(mid + 20)
        .unwrap();
    assert_same_rows("first commit", &rows_of(&db.path), &state1);
    assert_index(&db.path, "users", &state1);
    assert_wal_header_only(&db.path);
}

#[test]
fn recovery_of_a_synced_wal_is_idempotent() {
    let base = Temp::new("idem-base");
    {
        let mut database = Database::open(&base.path).unwrap();
        database.execute(BASELINE).unwrap();
    }
    let crashed = Temp::new("idem");
    copy_db(&base.path, &crashed.path);
    assert_aborted(&run_sql(
        &crashed.path,
        "INSERT INTO users VALUES (3, 'Cam')",
        Some("after-wal-sync"),
    ));
    assert!(fs::metadata(wal_path(&crashed.path)).unwrap().len() > 16);
    let saved = Temp::new("idem-saved");
    copy_db(&crashed.path, &saved.path);

    let first = rows_of(&crashed.path);
    assert_eq!(ids(&first), vec![1, 2, 3]);
    assert_wal_header_only(&crashed.path);

    copy_db(&saved.path, &crashed.path);
    assert!(fs::metadata(wal_path(&crashed.path)).unwrap().len() > 16);
    let second = rows_of(&crashed.path);
    assert_same_rows("second recovery", &second, &first);
    let third = rows_of(&crashed.path);
    assert_same_rows("third open", &third, &first);
    assert_wal_header_only(&crashed.path);
    assert_index(&crashed.path, "users", &first);
}

fn read_pages(path: &Path) -> Vec<(PageId, Page)> {
    let bytes = fs::read(path).unwrap();
    assert_eq!(bytes.len() % PAGE_SIZE, 0);
    bytes
        .chunks(PAGE_SIZE)
        .enumerate()
        .map(|(index, chunk)| {
            let mut page = Page::zeroed();
            page.data_mut().copy_from_slice(chunk);
            (PageId(index as u32), page)
        })
        .collect()
}
