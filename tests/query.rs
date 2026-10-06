use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_path(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "sqltoy-query-{label}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    path
}

struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn utf8(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("utf-8")
}

#[test]
fn sql_command_prints_a_join_ordered_and_limited() {
    let path = unique_temp_path("cli");
    let _cleanup = TempFile(path.clone());
    let sql = "\
CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT); \
CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, sku TEXT); \
INSERT INTO users VALUES (1, 'Bob'), (2, 'Ann'), (3, 'Cam'); \
INSERT INTO orders VALUES (10, 1, 'pen'), (11, 2, 'cup'), (12, 1, 'mug'); \
SELECT users.name, orders.sku \
FROM users \
LEFT JOIN orders ON users.id = orders.user_id \
ORDER BY users.name, orders.sku \
LIMIT 3";
    let output = Command::new(env!("CARGO_BIN_EXE_sqltoy"))
        .args(["sql", path.to_str().unwrap(), sql])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn sqltoy");
    assert_eq!(output.status.code(), Some(0), "{}", utf8(&output.stderr));
    assert_eq!(utf8(&output.stderr), "");
    assert_eq!(
        utf8(&output.stdout),
        "\
CREATE TABLE
CREATE TABLE
INSERT 3
INSERT 3
 name | sku 
------+-----
 Ann  | cup 
 Bob  | mug 
 Bob  | pen 
(3 rows)
"
    );
}
