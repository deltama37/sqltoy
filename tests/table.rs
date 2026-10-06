use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use sqltoy::{ColumnSpec, ColumnType, Database, PageId, RecordId, Value};

#[test]
fn rows_persist_after_reopen() {
    let path = unique_temp_path("persist");
    let _cleanup = TempFile(&path);

    let alice;
    {
        let mut db = Database::open(&path).expect("open");
        db.create_table(
            "users",
            &[
                ColumnSpec::new("id", ColumnType::Integer),
                ColumnSpec::new("name", ColumnType::Text),
                ColumnSpec::new("age", ColumnType::Integer),
            ],
        )
        .expect("create");
        let inserted = db
            .insert(
                "users",
                &[
                    Value::Integer(1),
                    Value::Text("Alice".to_string()),
                    Value::Null,
                ],
            )
            .expect("insert");
        let updated = db
            .update(
                "users",
                inserted,
                &[
                    Value::Integer(1),
                    Value::Text("Alicia".to_string()),
                    Value::Integer(31),
                ],
            )
            .expect("update");
        assert_ne!(updated, inserted);
        let second = db
            .insert(
                "users",
                &[Value::Integer(2), Value::Null, Value::Integer(4)],
            )
            .expect("second");
        db.delete("users", second).expect("delete");
        alice = updated;
    }

    let mut db = Database::open(&path).expect("reopen");
    assert_eq!(db.tables().len(), 1);
    assert_eq!(
        db.scan("USERS").expect("scan"),
        vec![(
            alice,
            vec![
                Value::Integer(1),
                Value::Text("Alicia".to_string()),
                Value::Integer(31),
            ]
        )]
    );
    let missing = db.get(
        "users",
        RecordId {
            page_id: PageId(2),
            slot_id: 0,
        },
    );
    let missing = missing.expect_err("old version");
    assert_eq!(missing.kind(), ErrorKind::NotFound);
}

#[test]
fn rows_of_two_tables_do_not_mix() {
    let path = unique_temp_path("mix");
    let _cleanup = TempFile(&path);

    let mut db = Database::open(&path).expect("open");
    db.create_table(
        "users",
        &[
            ColumnSpec::new("id", ColumnType::Integer),
            ColumnSpec::new("name", ColumnType::Text),
        ],
    )
    .expect("users");
    db.create_table(
        "posts",
        &[
            ColumnSpec::new("id", ColumnType::Integer),
            ColumnSpec::new("title", ColumnType::Text),
        ],
    )
    .expect("posts");
    let user_id = db
        .insert(
            "users",
            &[Value::Integer(1), Value::Text("Ada".to_string())],
        )
        .expect("user");
    let post_id = db
        .insert(
            "posts",
            &[Value::Integer(9), Value::Text("hello".to_string())],
        )
        .expect("post");
    assert_ne!(user_id.page_id, post_id.page_id);

    assert_eq!(db.scan("users").expect("users").len(), 1);
    assert_eq!(db.scan("posts").expect("posts").len(), 1);
    let err = db.get("posts", user_id).expect_err("cross get");
    assert_eq!(err.kind(), ErrorKind::NotFound);
    assert_eq!(err.to_string(), format!("record not found: {user_id}"));
    assert_eq!(
        db.get("users", user_id).expect("still there"),
        vec![Value::Integer(1), Value::Text("Ada".to_string())]
    );
}

#[test]
fn update_moves_when_the_row_does_not_fit() {
    let path = unique_temp_path("move");
    let _cleanup = TempFile(&path);

    let mut db = Database::open(&path).expect("open");
    db.create_table(
        "users",
        &[
            ColumnSpec::new("id", ColumnType::Integer),
            ColumnSpec::new("name", ColumnType::Text),
            ColumnSpec::new("age", ColumnType::Integer),
        ],
    )
    .expect("create");
    let first = db
        .insert(
            "users",
            &[
                Value::Integer(1),
                Value::Text("a".repeat(2000)),
                Value::Integer(1),
            ],
        )
        .expect("first");
    let second = db
        .insert(
            "users",
            &[
                Value::Integer(2),
                Value::Text("b".repeat(2000)),
                Value::Integer(2),
            ],
        )
        .expect("second");
    assert_eq!(first.page_id, second.page_id);

    let kept = db
        .update(
            "users",
            second,
            &[
                Value::Integer(2),
                Value::Text("b".repeat(2000)),
                Value::Integer(3),
            ],
        )
        .expect("new version");
    assert_ne!(kept, second);
    let err = db.get("users", second).expect_err("old version");
    assert_eq!(err.kind(), ErrorKind::NotFound);

    let moved = db
        .update(
            "users",
            first,
            &[
                Value::Integer(1),
                Value::Text("c".repeat(2100)),
                Value::Integer(1),
            ],
        )
        .expect("move");
    assert_ne!(moved, first);
    let err = db.get("users", first).expect_err("old id");
    assert_eq!(err.kind(), ErrorKind::NotFound);
    assert_eq!(err.to_string(), format!("record not found: {first}"));

    let scanned = db.scan("users").expect("scan");
    assert_eq!(scanned.len(), 2);
    assert_eq!(scanned.iter().filter(|(id, _)| *id == moved).count(), 1);
    assert_eq!(scanned.iter().filter(|(id, _)| *id == first).count(), 0);
    assert_eq!(scanned.iter().filter(|(id, _)| *id == kept).count(), 1);
    assert!(scanned
        .iter()
        .any(|(_, values)| values[1] == Value::Text("c".repeat(2100))));
}

#[test]
fn bad_input_and_unknown_table() {
    let path = unique_temp_path("bad");
    let _cleanup = TempFile(&path);

    let mut db = Database::open(&path).expect("open");
    db.create_table(
        "users",
        &[
            ColumnSpec::new("id", ColumnType::Integer),
            ColumnSpec::new("name", ColumnType::Text),
        ],
    )
    .expect("create");

    let err = db
        .insert("missing", &[Value::Integer(1)])
        .expect_err("missing");
    assert_eq!(err.kind(), ErrorKind::NotFound);
    assert_eq!(err.to_string(), "table not found: missing");

    let err = db.insert("users", &[Value::Integer(1)]).expect_err("count");
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    assert_eq!(err.to_string(), "expected 2 values for table users, got 1");

    let err = db
        .insert("users", &[Value::Integer(1), Value::Integer(2)])
        .expect_err("type");
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        "type mismatch for column name: expected TEXT"
    );
    assert!(db.scan("users").expect("empty").is_empty());

    let id = db
        .insert(
            "users",
            &[Value::Integer(1), Value::Text("Ada".to_string())],
        )
        .expect("insert");
    db.delete("users", id).expect("delete");
    assert!(db.scan("users").expect("gone").is_empty());
}

fn unique_temp_path(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut path = std::env::temp_dir();
    path.push(format!("sqltoy-table-{label}-{}-{nanos}", process::id()));
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
