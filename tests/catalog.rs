use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use sqltoy::{ColumnSpec, ColumnType, Database, RecordFile, TableId};

#[test]
fn schemas_persist_after_reopen() {
    let path = unique_temp_path("persist");
    let _cleanup = TempFile(&path);

    let created;
    {
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
        created = db.tables().to_vec();
        assert_eq!(created[0].id, TableId(2));
        assert_eq!(created[1].id, TableId(3));
    }

    let db = Database::open(&path).expect("reopen");
    assert_eq!(db.tables(), created.as_slice());
    assert_eq!(db.table("USERS").expect("lookup").name, "users");
    assert_eq!(db.table("Posts").expect("lookup").columns[1].name, "title");
}

#[test]
fn user_rows_are_not_catalog_entries() {
    let path = unique_temp_path("rows");
    let _cleanup = TempFile(&path);

    {
        let mut db = Database::open(&path).expect("open");
        db.create_table("users", &[ColumnSpec::new("id", ColumnType::Integer)])
            .expect("create");
    }
    {
        let mut records = RecordFile::open(&path).expect("records");
        records.insert(TableId(2), b"row-bytes").expect("user row");
    }

    let db = Database::open(&path).expect("reopen");
    assert_eq!(db.tables().len(), 1);
    assert_eq!(db.table("users").expect("users").columns.len(), 1);
}

#[test]
fn corrupt_catalog_record_fails_open() {
    let path = unique_temp_path("corrupt");
    let _cleanup = TempFile(&path);

    {
        let mut records = RecordFile::open(&path).expect("records");
        records
            .insert(TableId::CATALOG, b"not-a-schema")
            .expect("bogus");
    }

    let err = error_of(Database::open(&path));
    assert_eq!(err.kind(), ErrorKind::InvalidData);
    assert_eq!(err.to_string(), "invalid catalog record: 1:0");
}

fn error_of<T>(result: std::io::Result<T>) -> std::io::Error {
    match result {
        Ok(_) => panic!("expected an error"),
        Err(err) => err,
    }
}

fn unique_temp_path(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut path = std::env::temp_dir();
    path.push(format!("sqltoy-catalog-{label}-{}-{nanos}", process::id()));
    let _ = fs::remove_file(&path);
    path
}

struct TempFile<'a>(&'a PathBuf);

impl Drop for TempFile<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0);
    }
}
