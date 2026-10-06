use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use sqltoy::{format_result, Database, QueryResult, Value};

const MILESTONE_SQL: &str = "\
CREATE TABLE users (id INTEGER, name TEXT); \
INSERT INTO users VALUES (1, 'Alice'); \
SELECT * FROM users";

const MILESTONE_OUT: &str =
    "CREATE TABLE\nINSERT 1\n id | name  \n----+-------\n  1 | Alice \n(1 row)\n";

fn unique_temp_path(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut path = std::env::temp_dir();
    path.push(format!("sqltoy-sql-{label}-{}-{nanos}", std::process::id()));
    let _ = fs::remove_file(&path);
    path
}

struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn run(args: &[&str], stdin: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sqltoy"));
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn().expect("spawn sqltoy");
    if let Some(input) = stdin {
        let mut pipe = child.stdin.take().expect("stdin");
        pipe.write_all(input.as_bytes()).expect("write stdin");
    }
    child.wait_with_output().expect("wait sqltoy")
}

fn utf8(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("utf-8")
}

#[test]
fn milestone_sql_runs_through_database_execute() {
    let path = unique_temp_path("lib");
    let _cleanup = TempFile(path.clone());
    let mut db = Database::open(&path).unwrap();
    let results = db.execute(MILESTONE_SQL).unwrap();
    assert_eq!(
        results,
        vec![
            QueryResult::CreatedTable,
            QueryResult::Inserted(1),
            QueryResult::Rows {
                columns: vec!["id".to_string(), "name".to_string()],
                rows: vec![vec![Value::Integer(1), Value::Text("Alice".to_string())]],
            },
        ]
    );
    let mut rendered = String::new();
    for result in &results {
        rendered.push_str(&format_result(result));
        rendered.push('\n');
    }
    assert_eq!(rendered, MILESTONE_OUT);
}

#[test]
fn sql_command_prints_the_milestone_table() {
    let path = unique_temp_path("cli");
    let _cleanup = TempFile(path.clone());
    let output = run(&["sql", path.to_str().unwrap(), MILESTONE_SQL], None);
    assert_eq!(output.status.code(), Some(0), "{}", utf8(&output.stderr));
    assert_eq!(utf8(&output.stdout), MILESTONE_OUT);
    assert_eq!(utf8(&output.stderr), "");
}

#[test]
fn sql_command_prints_earlier_results_then_stops() {
    let path = unique_temp_path("cli-err");
    let _cleanup = TempFile(path.clone());
    let sql = "\
CREATE TABLE users (id INTEGER, name TEXT); \
INSERT INTO users VALUES (1, 'Alice'); \
SELECT * FROM missing; \
INSERT INTO users VALUES (2, 'Bob')";
    let output = run(&["sql", path.to_str().unwrap(), sql], None);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(utf8(&output.stdout), "CREATE TABLE\nINSERT 1\n");
    assert_eq!(utf8(&output.stderr), "error: table not found: missing\n");

    let mut db = Database::open(&path).unwrap();
    let results = db.execute("SELECT * FROM users").unwrap();
    match &results[0] {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(
                rows,
                &vec![vec![Value::Integer(1), Value::Text("Alice".to_string())]]
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn repl_continues_after_an_error_and_quit_stops_input() {
    let path = unique_temp_path("repl");
    let _cleanup = TempFile(path.clone());
    let input = "\
CREATE TABLE users (
  id INTEGER,
  name TEXT
);
INSERT INTO users VALUES (1, 'Alice');
SELECT * FROM missing;
INSERT INTO users VALUES (2, 'Bob');
SELECT id,
name
FROM users;
.quit
INSERT INTO users VALUES (3, 'Cara');
";
    let output = run(&["repl", path.to_str().unwrap()], Some(input));
    assert_eq!(output.status.code(), Some(0), "{}", utf8(&output.stderr));
    let stdout = utf8(&output.stdout);
    assert_eq!(
        stdout,
        "CREATE TABLE\nINSERT 1\nINSERT 1\n id | name  \n----+-------\n  1 | Alice \n  2 | Bob   \n(2 rows)\n"
    );
    assert!(!stdout.contains("sqltoy>"));
    assert!(!stdout.contains("Cara"));
    assert_eq!(utf8(&output.stderr), "error: table not found: missing\n");

    let mut db = Database::open(&path).unwrap();
    let results = db.execute("SELECT id FROM users").unwrap();
    match &results[0] {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(
                rows,
                &vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn repl_runs_a_leftover_statement_at_eof() {
    let path = unique_temp_path("eof");
    let _cleanup = TempFile(path.clone());
    let input = "\
CREATE TABLE t (id INTEGER);
INSERT INTO t VALUES (7)";
    let output = run(&["repl", path.to_str().unwrap()], Some(input));
    assert_eq!(output.status.code(), Some(0), "{}", utf8(&output.stderr));
    assert_eq!(utf8(&output.stdout), "CREATE TABLE\nINSERT 1\n");
    assert_eq!(utf8(&output.stderr), "");

    let selected = run(&["sql", path.to_str().unwrap(), "SELECT id FROM t"], None);
    assert_eq!(selected.status.code(), Some(0));
    assert_eq!(utf8(&selected.stdout), " id \n----\n  7 \n(1 row)\n");
}

#[test]
fn sql_persists_across_processes() {
    let path = unique_temp_path("persist");
    let _cleanup = TempFile(path.clone());
    let created = run(
        &[
            "sql",
            path.to_str().unwrap(),
            "CREATE TABLE users (id INTEGER, name TEXT); INSERT INTO users VALUES (1, 'Alice')",
        ],
        None,
    );
    assert_eq!(created.status.code(), Some(0), "{}", utf8(&created.stderr));
    assert_eq!(utf8(&created.stdout), "CREATE TABLE\nINSERT 1\n");

    let selected = run(
        &["sql", path.to_str().unwrap(), "SELECT * FROM users"],
        None,
    );
    assert_eq!(
        selected.status.code(),
        Some(0),
        "{}",
        utf8(&selected.stderr)
    );
    assert_eq!(
        utf8(&selected.stdout),
        " id | name  \n----+-------\n  1 | Alice \n(1 row)\n"
    );
    assert_eq!(utf8(&selected.stderr), "");
}
