//! Insert, scan, update, and delete of typed rows.
//!
//! Values are checked against the catalog schema and stored as one record
//! per row. When an updated row no longer fits on its page, it is inserted
//! at a new record id and the old row is deleted.

use std::io::{self, ErrorKind};

use crate::catalog::{Database, TableSchema};
use crate::record::RecordId;
use crate::row::{decode_row, encode_row, Value};

impl Database {
    /// Inserts one row into `table` and returns its record id.
    ///
    /// `values` must match the column count and types. NULL is allowed in
    /// every column. Comparison of `table` ignores ASCII case.
    ///
    /// An unknown table is [`ErrorKind::NotFound`]. A count mismatch, a type
    /// mismatch, or a row longer than the maximum record length is
    /// [`ErrorKind::InvalidInput`].
    pub fn insert(&mut self, table: &str, values: &[Value]) -> io::Result<RecordId> {
        let schema = self.require_table(table)?;
        let bytes = encode_row(&schema, values)?;
        self.records.insert(schema.id, &bytes)
    }

    /// Reads the row identified by `id` in `table`.
    ///
    /// An unknown table or a missing row is [`ErrorKind::NotFound`]. A stored
    /// record that does not match the schema is [`ErrorKind::InvalidData`].
    pub fn get(&mut self, table: &str, id: RecordId) -> io::Result<Vec<Value>> {
        let schema = self.require_table(table)?;
        let bytes = self.records.get(schema.id, id)?;
        decode_row(&schema, id, &bytes)
    }

    /// Rows of `table` with their record ids, in page order then slot order.
    ///
    /// An unknown table is [`ErrorKind::NotFound`]. A stored record that
    /// cannot be decoded with the schema is [`ErrorKind::InvalidData`].
    pub fn scan(&mut self, table: &str) -> io::Result<Vec<(RecordId, Vec<Value>)>> {
        let schema = self.require_table(table)?;
        let rows = self.records.scan(schema.id)?;
        rows.into_iter()
            .map(|(id, bytes)| Ok((id, decode_row(&schema, id, &bytes)?)))
            .collect()
    }

    /// Replaces the row identified by `id`.
    ///
    /// The checks match [`Self::insert`] and run before the record is read.
    /// When the new bytes fit on the same page, the record id is unchanged.
    /// When they do not, the new row is inserted and the old row is deleted
    /// afterward, and the new id is returned. A crash between those writes
    /// can leave both copies.
    ///
    /// An unknown table or a missing row is [`ErrorKind::NotFound`]. A count
    /// mismatch, a type mismatch, or a row longer than the maximum record
    /// length is [`ErrorKind::InvalidInput`].
    pub fn update(&mut self, table: &str, id: RecordId, values: &[Value]) -> io::Result<RecordId> {
        let schema = self.require_table(table)?;
        let bytes = encode_row(&schema, values)?;
        if self.records.try_update(schema.id, id, &bytes)? {
            Ok(id)
        } else {
            let new_id = self.records.insert(schema.id, &bytes)?;
            self.records.delete(schema.id, id)?;
            Ok(new_id)
        }
    }

    /// Deletes the row identified by `id` in `table`.
    ///
    /// An unknown table or a missing row is [`ErrorKind::NotFound`].
    pub fn delete(&mut self, table: &str, id: RecordId) -> io::Result<()> {
        let schema = self.require_table(table)?;
        self.records.delete(schema.id, id)
    }

    fn require_table(&self, name: &str) -> io::Result<TableSchema> {
        self.table(name)
            .cloned()
            .ok_or_else(|| io::Error::new(ErrorKind::NotFound, format!("table not found: {name}")))
    }
}

#[cfg(test)]
mod tests {
    use super::Database;
    use crate::catalog::ColumnType;
    use crate::page::PageId;
    use crate::record::{RecordId, TableId};
    use crate::row::Value;
    use std::env::temp_dir;
    use std::fs;
    use std::io::ErrorKind;
    use std::path::PathBuf;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDb {
        path: PathBuf,
    }

    impl TempDb {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let mut path = temp_dir();
            path.push(format!("sqltoy-{label}-{}-{nanos}", process::id()));
            let _ = fs::remove_file(&path);
            TempDb { path }
        }

        fn path(&self) -> &PathBuf {
            &self.path
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    fn open_users() -> (TempDb, Database) {
        let db = TempDb::new("rows");
        let mut database = Database::open(db.path()).unwrap();
        database
            .create_table(
                "users",
                &[
                    ("id", ColumnType::Integer),
                    ("name", ColumnType::Text),
                    ("age", ColumnType::Integer),
                ],
            )
            .unwrap();
        (db, database)
    }

    fn row(id: i64, name: Option<&str>, age: Option<i64>) -> Vec<Value> {
        vec![
            Value::Integer(id),
            match name {
                Some(name) => Value::Text(name.to_string()),
                None => Value::Null,
            },
            match age {
                Some(age) => Value::Integer(age),
                None => Value::Null,
            },
        ]
    }

    fn show(id: RecordId, values: &[Value]) -> String {
        let body = values
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        format!("{id} ({body})")
    }

    #[test]
    fn insert_scan_update_and_delete_roundtrip() {
        let (_db, mut database) = open_users();
        let alice = database
            .insert("users", &row(1, Some("Alice"), Some(30)))
            .unwrap();
        let bob = database
            .insert("USERS", &row(2, Some("Bob"), None))
            .unwrap();
        assert_eq!(alice.to_string(), "2:0");
        assert_eq!(bob.to_string(), "2:1");
        assert_eq!(
            database.get("users", alice).unwrap(),
            row(1, Some("Alice"), Some(30))
        );

        let scanned = database.scan("Users").unwrap();
        assert_eq!(
            scanned
                .iter()
                .map(|(id, values)| show(*id, values))
                .collect::<Vec<_>>(),
            vec![
                "2:0 (1, 'Alice', 30)".to_string(),
                "2:1 (2, 'Bob', NULL)".to_string(),
            ]
        );

        let updated = database
            .update("users", alice, &row(1, Some("Alicia"), Some(31)))
            .unwrap();
        assert_eq!(updated, alice);
        assert_eq!(
            database.get("users", alice).unwrap(),
            row(1, Some("Alicia"), Some(31))
        );

        let shrunk = database
            .update("users", alice, &row(1, Some("Al"), Some(31)))
            .unwrap();
        assert_eq!(shrunk, alice);

        database.delete("users", bob).unwrap();
        let err = database.get("users", bob).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(err.to_string(), "record not found: 2:1");
        assert_eq!(
            database.scan("users").unwrap(),
            vec![(alice, row(1, Some("Al"), Some(31)))]
        );

        let err = database.delete("users", bob).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(err.to_string(), "record not found: 2:1");
    }

    #[test]
    fn nulls_persist_across_reopen() {
        let db = TempDb::new("nulls");
        let alice;
        {
            let mut database = Database::open(db.path()).unwrap();
            database
                .create_table(
                    "users",
                    &[
                        ("id", ColumnType::Integer),
                        ("name", ColumnType::Text),
                        ("age", ColumnType::Integer),
                    ],
                )
                .unwrap();
            alice = database.insert("users", &row(1, None, Some(30))).unwrap();
            database
                .insert(
                    "users",
                    &[Value::Null, Value::Text("it's".to_string()), Value::Null],
                )
                .unwrap();
        }

        let mut database = Database::open(db.path()).unwrap();
        assert_eq!(database.tables().len(), 1);
        assert_eq!(database.table("users").unwrap().id, TableId(2));
        assert_eq!(
            database.get("users", alice).unwrap(),
            row(1, None, Some(30))
        );
        let scanned = database.scan("users").unwrap();
        assert_eq!(scanned.len(), 2);
        assert_eq!(scanned[0], (alice, row(1, None, Some(30))));
        assert_eq!(
            scanned[1].1,
            vec![Value::Null, Value::Text("it's".to_string()), Value::Null,]
        );
        assert_eq!(
            show(scanned[1].0, &scanned[1].1),
            "2:1 (NULL, 'it''s', NULL)"
        );
    }

    #[test]
    fn rows_of_two_tables_do_not_mix() {
        let db = TempDb::new("mix");
        let user_id;
        let post_id;
        {
            let mut database = Database::open(db.path()).unwrap();
            database
                .create_table(
                    "users",
                    &[("id", ColumnType::Integer), ("name", ColumnType::Text)],
                )
                .unwrap();
            database
                .create_table(
                    "posts",
                    &[("id", ColumnType::Integer), ("title", ColumnType::Text)],
                )
                .unwrap();
            user_id = database
                .insert(
                    "users",
                    &[Value::Integer(1), Value::Text("Ada".to_string())],
                )
                .unwrap();
            post_id = database
                .insert(
                    "posts",
                    &[Value::Integer(9), Value::Text("hello".to_string())],
                )
                .unwrap();
            assert_eq!(user_id.page_id, PageId(2));
            assert_eq!(post_id.page_id, PageId(3));

            let err = database.get("posts", user_id).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::NotFound);
            assert_eq!(err.to_string(), format!("record not found: {user_id}"));
            let err = database
                .update(
                    "posts",
                    user_id,
                    &[Value::Integer(9), Value::Text("nope".to_string())],
                )
                .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::NotFound);
            assert_eq!(err.to_string(), format!("record not found: {user_id}"));
            let err = database.delete("posts", user_id).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::NotFound);
            assert_eq!(
                database.get("users", user_id).unwrap(),
                vec![Value::Integer(1), Value::Text("Ada".to_string())]
            );
        }

        let mut database = Database::open(db.path()).unwrap();
        assert_eq!(
            database.scan("users").unwrap(),
            vec![(
                user_id,
                vec![Value::Integer(1), Value::Text("Ada".to_string())]
            )]
        );
        assert_eq!(
            database.scan("posts").unwrap(),
            vec![(
                post_id,
                vec![Value::Integer(9), Value::Text("hello".to_string())]
            )]
        );
        assert_eq!(database.tables().len(), 2);
    }

    #[test]
    fn update_in_place_keeps_the_record_id() {
        let (_db, mut database) = open_users();
        let id = database
            .insert("users", &row(1, Some("Al"), Some(1)))
            .unwrap();
        let grown = database
            .update("users", id, &row(1, Some("Alicia"), Some(2)))
            .unwrap();
        assert_eq!(grown, id);
        assert_eq!(grown.page_id, PageId(2));
        assert_eq!(
            database.get("users", id).unwrap(),
            row(1, Some("Alicia"), Some(2))
        );
        assert_eq!(database.scan("users").unwrap().len(), 1);
    }

    #[test]
    fn update_that_does_not_fit_moves_the_row() {
        let (_db, mut database) = open_users();
        // Two long rows fill the first user page. A longer replacement does
        // not fit there, so the table layer inserts it and deletes the old one.
        let first = database
            .insert("users", &row(1, Some(&"a".repeat(2000)), Some(1)))
            .unwrap();
        let second = database
            .insert("users", &row(2, Some(&"b".repeat(2000)), Some(2)))
            .unwrap();
        assert_eq!(
            first,
            RecordId {
                page_id: PageId(2),
                slot_id: 0
            }
        );
        assert_eq!(second.page_id, PageId(2));

        let kept = database
            .update("users", second, &row(2, Some(&"b".repeat(2000)), Some(3)))
            .unwrap();
        assert_eq!(kept, second);

        let moved = database
            .update("users", first, &row(1, Some(&"c".repeat(2100)), Some(1)))
            .unwrap();
        assert_ne!(moved, first);
        assert_eq!(moved.page_id, PageId(3));
        assert_eq!(moved.slot_id, 0);

        let err = database.get("users", first).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(err.to_string(), format!("record not found: {first}"));

        let scanned = database.scan("users").unwrap();
        assert_eq!(scanned.len(), 2);
        assert_eq!(scanned.iter().filter(|(id, _)| *id == first).count(), 0);
        assert_eq!(scanned.iter().filter(|(id, _)| *id == moved).count(), 1);
        assert_eq!(scanned[0].0, second);
        assert_eq!(scanned[0].1, row(2, Some(&"b".repeat(2000)), Some(3)));
        assert_eq!(scanned[1].0, moved);
        assert_eq!(scanned[1].1, row(1, Some(&"c".repeat(2100)), Some(1)));
    }

    #[test]
    fn rejects_bad_values_and_unknown_tables() {
        let (_db, mut database) = open_users();
        let err = database
            .insert("nope", &row(1, Some("a"), Some(1)))
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(err.to_string(), "table not found: nope");

        let err = database.insert("USERS", &[Value::Integer(1)]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "expected 3 values for table users, got 1");

        let err = database
            .insert(
                "users",
                &[
                    Value::Text("x".to_string()),
                    Value::Text("y".to_string()),
                    Value::Integer(1),
                ],
            )
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            err.to_string(),
            "type mismatch for column id: expected INTEGER"
        );

        let err = database
            .insert("users", &row(1, Some(&"x".repeat(5000)), Some(1)))
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "row too large for table users");
        assert!(database.scan("users").unwrap().is_empty());

        let id = database
            .insert("users", &row(1, Some("Ada"), Some(1)))
            .unwrap();
        let missing = RecordId {
            page_id: PageId(2),
            slot_id: 5,
        };
        let err = database.update("gone", missing, &[]).unwrap_err();
        assert_eq!(err.to_string(), "table not found: gone");
        let err = database.update("users", missing, &[]).unwrap_err();
        assert_eq!(err.to_string(), "expected 3 values for table users, got 0");
        let err = database
            .update(
                "users",
                missing,
                &[Value::Text("x".to_string()), Value::Null, Value::Null],
            )
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "type mismatch for column id: expected INTEGER"
        );
        let err = database
            .update("users", missing, &row(2, Some("nope"), Some(2)))
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(err.to_string(), "record not found: 2:5");
        assert_eq!(
            database.scan("users").unwrap(),
            vec![(id, row(1, Some("Ada"), Some(1)))]
        );

        let err = database
            .update(
                "users",
                id,
                &[Value::Integer(1), Value::Integer(2), Value::Null],
            )
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "type mismatch for column name: expected TEXT"
        );
        assert_eq!(
            database.get("users", id).unwrap(),
            row(1, Some("Ada"), Some(1))
        );

        let err = database.delete("missing", id).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(err.to_string(), "table not found: missing");
        let err = database.scan("missing").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(err.to_string(), "table not found: missing");
        let err = database.get("missing", id).unwrap_err();
        assert_eq!(err.to_string(), "table not found: missing");
    }

    #[test]
    fn corrupt_row_is_invalid_data() {
        let (_db, mut database) = open_users();
        database.records.insert(TableId(2), b"nope").unwrap();
        let err = database.scan("users").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "invalid row: 2:0");
        let err = database
            .get(
                "users",
                RecordId {
                    page_id: PageId(2),
                    slot_id: 0,
                },
            )
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "invalid row: 2:0");
    }
}
