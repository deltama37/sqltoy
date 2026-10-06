//! Snapshot isolation, versions, and crash cleanup.

use std::fs;
use std::io::Write;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use sqltoy::mvcc::split_record;
use sqltoy::{wal_path, Database, QueryResult, RecordFile, SessionId, TableId, Value, PAGE_SIZE};

fn temp_path(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "sqltoy-mvcc-{label}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(wal_path(&path));
    path
}

struct Temp(PathBuf);

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
        let _ = fs::remove_file(wal_path(&self.0));
    }
}

fn open(label: &str) -> (Temp, Database) {
    let path = temp_path(label);
    let db = Database::open(&path).unwrap();
    (Temp(path), db)
}

fn exec(db: &mut Database, session: SessionId, sql: &str) -> Vec<QueryResult> {
    db.execute_in(session, sql)
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
}

fn query(db: &mut Database, session: SessionId, sql: &str) -> Vec<Vec<Value>> {
    match &exec(db, session, sql)[0] {
        QueryResult::Rows { rows, .. } => rows.clone(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn exec_err(db: &mut Database, session: SessionId, sql: &str) -> (std::io::ErrorKind, String) {
    let err = db.execute_in(session, sql).unwrap_err();
    (err.kind(), err.to_string())
}

fn int(value: i64) -> Value {
    Value::Integer(value)
}

fn text(value: &str) -> Value {
    Value::Text(value.to_string())
}

fn users(db: &mut Database) {
    let main = db.default_session();
    exec(
        db,
        main,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)",
    );
}

#[test]
fn fresh_database_starts_at_transaction_one() {
    let (temp, db) = open("fresh");
    assert_eq!(db.next_transaction_id(), 1);
    drop(db);
    let bytes = fs::read(&temp.0).unwrap();
    assert_eq!(u64::from_le_bytes(bytes[16..24].try_into().unwrap()), 1);
    assert_eq!(bytes[24], 0);
    assert!(bytes[25..PAGE_SIZE].iter().all(|byte| *byte == 0));
}

#[test]
fn snapshot_hides_dirty_and_later_commits() {
    let (_temp, mut db) = open("snap");
    users(&mut db);
    let main = db.default_session();
    let other = db.create_session();
    exec(&mut db, main, "BEGIN");
    exec(&mut db, other, "BEGIN");
    exec(&mut db, main, "INSERT INTO users VALUES (1, 'Ann')");
    assert_eq!(
        query(&mut db, main, "SELECT name FROM users"),
        vec![vec![text("Ann")]]
    );
    assert!(query(&mut db, other, "SELECT name FROM users").is_empty());
    exec(&mut db, main, "COMMIT");
    assert!(query(&mut db, other, "SELECT name FROM users").is_empty());
    exec(&mut db, other, "COMMIT");
    assert_eq!(
        query(&mut db, other, "SELECT name FROM users"),
        vec![vec![text("Ann")]]
    );
}

#[test]
fn non_repeatable_reads_and_phantoms_stay_stable() {
    let (_temp, mut db) = open("stable");
    users(&mut db);
    let main = db.default_session();
    exec(&mut db, main, "INSERT INTO users VALUES (1, 'Ann')");
    let reader = db.create_session();
    let writer = db.create_session();
    exec(&mut db, reader, "BEGIN");
    assert_eq!(
        query(&mut db, reader, "SELECT name FROM users WHERE id = 1"),
        vec![vec![text("Ann")]]
    );
    assert_eq!(
        query(&mut db, reader, "SELECT id FROM users"),
        vec![vec![int(1)]]
    );
    exec(
        &mut db,
        writer,
        "UPDATE users SET name = 'Bea' WHERE id = 1; INSERT INTO users VALUES (2, 'Cam')",
    );
    assert_eq!(
        query(&mut db, reader, "SELECT name FROM users WHERE id = 1"),
        vec![vec![text("Ann")]]
    );
    assert_eq!(
        query(&mut db, reader, "SELECT id FROM users ORDER BY id"),
        vec![vec![int(1)]]
    );
    exec(&mut db, reader, "COMMIT");
    assert_eq!(
        query(&mut db, reader, "SELECT id, name FROM users ORDER BY id"),
        vec![vec![int(1), text("Bea")], vec![int(2), text("Cam")]]
    );
}

#[test]
fn rollback_keeps_another_sessions_change_on_the_same_page() {
    let (temp, mut db) = open("same-page");
    users(&mut db);
    let main = db.default_session();
    exec(&mut db, main, "INSERT INTO users VALUES (1, 'a'), (2, 'b')");
    let before = db.scan("users").unwrap();
    assert_eq!(before[0].0.page_id, before[1].0.page_id);

    let writer = db.create_session();
    exec(&mut db, writer, "BEGIN");
    exec(&mut db, writer, "UPDATE users SET name = 'A' WHERE id = 1");
    exec(&mut db, main, "UPDATE users SET name = 'B' WHERE id = 2");
    exec(&mut db, writer, "ROLLBACK");
    assert_eq!(
        query(&mut db, main, "SELECT id, name FROM users ORDER BY id"),
        vec![vec![int(1), text("a")], vec![int(2), text("B")]]
    );
    drop(db);

    let mut db = Database::open(&temp.0).unwrap();
    let main = db.default_session();
    assert_eq!(
        query(&mut db, main, "SELECT id, name FROM users ORDER BY id"),
        vec![vec![int(1), text("a")], vec![int(2), text("B")]]
    );
}

#[test]
fn write_write_and_delete_conflicts_retry_after_rollback() {
    let (_temp, mut db) = open("conflict");
    users(&mut db);
    let main = db.default_session();
    exec(&mut db, main, "INSERT INTO users VALUES (1, 'Ann')");
    let other = db.create_session();
    exec(&mut db, main, "BEGIN");
    exec(&mut db, other, "BEGIN");
    exec(&mut db, main, "UPDATE users SET name = 'A' WHERE id = 1");
    assert_eq!(
        exec_err(&mut db, other, "UPDATE users SET name = 'B' WHERE id = 1"),
        (
            std::io::ErrorKind::Other,
            "serialization failure: row was modified by a concurrent transaction".to_string(),
        )
    );
    exec(&mut db, main, "COMMIT");
    assert_eq!(
        exec_err(&mut db, other, "UPDATE users SET name = 'B' WHERE id = 1"),
        (
            std::io::ErrorKind::Other,
            "serialization failure: row was modified by a concurrent transaction".to_string(),
        )
    );
    exec(&mut db, other, "ROLLBACK");
    exec(&mut db, other, "UPDATE users SET name = 'B' WHERE id = 1");
    assert_eq!(
        query(&mut db, other, "SELECT name FROM users"),
        vec![vec![text("B")]]
    );

    exec(&mut db, main, "BEGIN");
    exec(&mut db, other, "BEGIN");
    exec(&mut db, main, "DELETE FROM users WHERE id = 1");
    assert_eq!(
        exec_err(&mut db, other, "DELETE FROM users WHERE id = 1").1,
        "serialization failure: row was modified by a concurrent transaction"
    );
    exec(&mut db, main, "COMMIT");
    assert_eq!(
        exec_err(&mut db, other, "DELETE FROM users WHERE id = 1").1,
        "serialization failure: row was modified by a concurrent transaction"
    );
    exec(&mut db, other, "ROLLBACK");
    exec(&mut db, other, "DELETE FROM users WHERE id = 1");
    assert!(query(&mut db, other, "SELECT id FROM users").is_empty());
}

#[test]
fn unique_keys_wait_for_the_other_transaction_to_finish() {
    let (_temp, mut db) = open("unique");
    users(&mut db);
    let main = db.default_session();
    exec(
        &mut db,
        main,
        "INSERT INTO users VALUES (1, 'Ann'), (2, 'Bea')",
    );
    let older = db.create_session();
    exec(&mut db, older, "BEGIN");
    let writer = db.create_session();
    exec(&mut db, writer, "BEGIN");
    exec(&mut db, writer, "INSERT INTO users VALUES (5, 'new')");
    assert_eq!(
        exec_err(&mut db, main, "INSERT INTO users VALUES (5, 'other')").1,
        "serialization failure: row was modified by a concurrent transaction"
    );
    exec(&mut db, writer, "DELETE FROM users WHERE id = 1");
    exec(&mut db, writer, "DELETE FROM users WHERE id = 2");
    assert_eq!(
        exec_err(&mut db, main, "INSERT INTO users VALUES (1, 'taken')").1,
        "serialization failure: row was modified by a concurrent transaction"
    );
    exec(&mut db, writer, "COMMIT");
    exec(&mut db, main, "INSERT INTO users VALUES (1, 'again')");
    // `older` still sees id 2, so reusing that key would show it twice.
    assert_eq!(
        exec_err(&mut db, older, "INSERT INTO users VALUES (2, 'twice')").1,
        "serialization failure: row was modified by a concurrent transaction"
    );
    assert_eq!(
        query(&mut db, older, "SELECT name FROM users WHERE id = 1"),
        vec![vec![text("Ann")]]
    );
    assert_eq!(
        query(&mut db, older, "SELECT id FROM users ORDER BY id"),
        vec![vec![int(1)], vec![int(2)]]
    );
    assert_eq!(
        query(&mut db, main, "SELECT name FROM users WHERE id = 1"),
        vec![vec![text("again")]]
    );
    // A key whose delete `main` sees is free for `main` even while `older`
    // can still see the deleted version.
}

#[test]
fn vacuum_is_durable_across_reopen() {
    let (temp, mut db) = open("vacuum-durable");
    users(&mut db);
    let main = db.default_session();
    exec(&mut db, main, "INSERT INTO users VALUES (1, 'v0')");
    for version in 1..=3 {
        exec(
            &mut db,
            main,
            &format!("UPDATE users SET name = 'v{version}' WHERE id = 1"),
        );
    }
    assert_eq!(
        exec(&mut db, main, "VACUUM"),
        vec![QueryResult::Vacuumed(3)]
    );
    drop(db);

    let mut db = Database::open(&temp.0).unwrap();
    assert_eq!(db.stored_version_count("users").unwrap(), 1);
    assert_eq!(db.index_entry_count("users").unwrap(), Some(1));
    let main = db.default_session();
    assert_eq!(
        query(&mut db, main, "SELECT name FROM users"),
        vec![vec![text("v3")]]
    );
}

#[test]
fn write_skew_is_allowed() {
    // Each transaction reads both balances and updates a different row. Neither
    // write conflicts, so both commits stand. sqltoy does not enforce a sum.
    let (_temp, mut db) = open("skew");
    let main = db.default_session();
    exec(
        &mut db,
        main,
        "CREATE TABLE accounts (id INTEGER PRIMARY KEY, balance INTEGER); \
         INSERT INTO accounts VALUES (1, 100), (2, 100)",
    );
    let other = db.create_session();
    exec(&mut db, main, "BEGIN");
    exec(&mut db, other, "BEGIN");
    assert_eq!(
        query(&mut db, main, "SELECT balance FROM accounts ORDER BY id"),
        vec![vec![int(100)], vec![int(100)]]
    );
    assert_eq!(
        query(&mut db, other, "SELECT balance FROM accounts ORDER BY id"),
        vec![vec![int(100)], vec![int(100)]]
    );
    exec(
        &mut db,
        main,
        "UPDATE accounts SET balance = -50 WHERE id = 1",
    );
    exec(
        &mut db,
        other,
        "UPDATE accounts SET balance = -50 WHERE id = 2",
    );
    exec(&mut db, main, "COMMIT");
    exec(&mut db, other, "COMMIT");
    assert_eq!(
        query(&mut db, main, "SELECT balance FROM accounts ORDER BY id"),
        vec![vec![int(-50)], vec![int(-50)]]
    );
}

#[test]
fn updates_leave_versions_until_vacuum() {
    let (_temp, mut db) = open("versions");
    users(&mut db);
    let main = db.default_session();
    exec(&mut db, main, "INSERT INTO users VALUES (1, 'v0')");
    for version in 1..=4 {
        exec(
            &mut db,
            main,
            &format!("UPDATE users SET name = 'v{version}' WHERE id = 1"),
        );
    }
    assert_eq!(db.stored_version_count("users").unwrap(), 5);
    assert_eq!(db.index_entry_count("users").unwrap(), Some(5));
    assert_eq!(
        exec(&mut db, main, "VACUUM"),
        vec![QueryResult::Vacuumed(4)]
    );
    assert_eq!(db.stored_version_count("users").unwrap(), 1);
    assert_eq!(db.index_entry_count("users").unwrap(), Some(1));
    assert_eq!(
        query(&mut db, main, "SELECT name FROM users"),
        vec![vec![text("v4")]]
    );

    let reader = db.create_session();
    exec(&mut db, reader, "BEGIN");
    let third = db.create_session();
    exec(&mut db, third, "UPDATE users SET name = 'v5' WHERE id = 1");
    assert_eq!(
        exec(&mut db, main, "VACUUM"),
        vec![QueryResult::Vacuumed(0)]
    );
    assert_eq!(db.stored_version_count("users").unwrap(), 2);
    assert_eq!(
        query(&mut db, reader, "SELECT name FROM users"),
        vec![vec![text("v4")]]
    );
    exec(&mut db, reader, "COMMIT");
    assert_eq!(
        exec(&mut db, main, "VACUUM"),
        vec![QueryResult::Vacuumed(1)]
    );
    assert_eq!(db.stored_version_count("users").unwrap(), 1);
    assert_eq!(db.index_entry_count("users").unwrap(), Some(1));
    assert_eq!(
        exec_err(&mut db, main, "BEGIN; VACUUM").1,
        "VACUUM cannot run inside a transaction"
    );
}

#[test]
fn create_table_needs_every_other_transaction_idle() {
    let (_temp, mut db) = open("ddl");
    let main = db.default_session();
    let other = db.create_session();
    exec(&mut db, other, "BEGIN");
    assert_eq!(
        exec_err(&mut db, main, "CREATE TABLE posts (id INTEGER)").1,
        "CREATE TABLE requires no other active transactions"
    );
    exec(&mut db, other, "ROLLBACK");
    exec(&mut db, main, "BEGIN");
    exec(&mut db, main, "CREATE TABLE posts (id INTEGER)");
    assert!(db.table("posts").is_some());
    exec(&mut db, main, "ROLLBACK");
    assert!(db.table("posts").is_none());
}

#[test]
fn cleanup_keeps_a_catalog_record_another_session_flushed() {
    // Catalog rows are not versions. Crash cleanup rewrites user-table
    // versions only, so a table created by a transaction that never committed
    // stays after another session's commit flushed its catalog page.
    let (temp, mut db) = open("ddl-flush");
    users(&mut db);
    let main = db.default_session();
    let writer = db.create_session();
    exec(&mut db, writer, "BEGIN");
    exec(
        &mut db,
        writer,
        "CREATE TABLE extra (id INTEGER PRIMARY KEY)",
    );
    exec(&mut db, writer, "INSERT INTO extra VALUES (1)");
    exec(&mut db, main, "INSERT INTO users VALUES (9, 'flushed')");
    drop(db);

    let mut db = Database::open(&temp.0).unwrap();
    let main = db.default_session();
    assert!(db.table("extra").is_some());
    assert!(query(&mut db, main, "SELECT id FROM extra").is_empty());
    assert_eq!(
        query(&mut db, main, "SELECT id FROM users"),
        vec![vec![int(9)]]
    );
    assert_eq!(fs::read(&temp.0).unwrap()[24], 0);
}

#[test]
fn close_session_rolls_the_transaction_back() {
    let (_temp, mut db) = open("close");
    users(&mut db);
    let extra = db.create_session();
    exec(&mut db, extra, "BEGIN");
    exec(&mut db, extra, "INSERT INTO users VALUES (1, 'gone')");
    db.close_session(extra).unwrap();
    let main = db.default_session();
    assert!(query(&mut db, main, "SELECT id FROM users").is_empty());
}

#[test]
fn crash_during_another_sessions_commit_cleans_uncommitted_versions() {
    let path = temp_path("crash");
    let _temp = Temp(path.clone());
    let baseline = run_sql(
        &path,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT); INSERT INTO t VALUES (1, 'keep'), (2, 'stay')",
        None,
    );
    assert!(baseline.status.success(), "{}", stderr(&baseline));

    let script = "\
.session a
BEGIN;
INSERT INTO t VALUES (3, 'ghost');
UPDATE t SET name = 'changed' WHERE id = 1;
DELETE FROM t WHERE id = 2;
.session main
INSERT INTO t VALUES (4, 'live');
";
    let crashed = run_repl(&path, script, Some("before-wal-truncate"));
    assert!(
        crashed.status.signal().is_some(),
        "status {:?}\nstdout {}\nstderr {}",
        crashed.status,
        stdout(&crashed),
        stderr(&crashed)
    );

    let mut db = Database::open(&path).unwrap();
    let main = db.default_session();
    assert_eq!(
        query(&mut db, main, "SELECT id, name FROM t ORDER BY id"),
        vec![
            vec![int(1), text("keep")],
            vec![int(2), text("stay")],
            vec![int(4), text("live")],
        ]
    );
    assert_eq!(db.stored_version_count("t").unwrap(), 3);
    assert_eq!(db.index_entry_count("t").unwrap(), Some(3));
    let next = db.next_transaction_id();
    drop(db);

    let bytes = fs::read(&path).unwrap();
    assert_eq!(bytes[24], 0, "cleanup flag");
    let mut max_xid = 0u64;
    let mut records = RecordFile::open(&path).unwrap();
    for (id, record) in records.scan(TableId(2)).unwrap() {
        let (header, _) = split_record(id, &record).unwrap();
        assert!(header.xmin_committed(), "{header:?}");
        assert!(header.xmax == 0 || header.xmax_committed(), "{header:?}");
        max_xid = max_xid.max(header.xmin).max(header.xmax);
    }
    assert!(next > max_xid, "next {next} max {max_xid}");

    let mut db = Database::open(&path).unwrap();
    let main = db.default_session();
    let before = db.next_transaction_id();
    exec(&mut db, main, "INSERT INTO t VALUES (5, 'after')");
    assert!(db.next_transaction_id() > before);
    assert_eq!(
        query(&mut db, main, "SELECT name FROM t WHERE id = 5"),
        vec![vec![text("after")]]
    );
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

fn run_repl(path: &Path, script: &str, crash: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sqltoy"));
    command
        .arg("repl")
        .arg(path)
        .stdin(std::process::Stdio::piped());
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
    let mut child = command.spawn().expect("spawn repl");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    drop(child.stdin.take());
    child.wait_with_output().expect("repl")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
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
