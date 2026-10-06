//! Insert, scan, update, and delete of typed rows.
//!
//! Values are checked against the catalog schema and stored as one record
//! per row. When an updated row no longer fits on its page, it is inserted
//! at a new record id and the old row is deleted. A primary key is kept in
//! the table's B+Tree: it cannot be NULL, and it is unique.

use std::collections::BTreeSet;
use std::io::{self, ErrorKind};

use crate::btree::BTree;
use crate::catalog::{Database, TableSchema};
use crate::record::{RecordId, TableId};
use crate::row::{decode_row, encode_row, Value};

impl Database {
    /// Inserts one row into `table` and returns its record id.
    ///
    /// `values` must match the column count and types. NULL is allowed in
    /// every column except a primary key. Comparison of `table` ignores ASCII
    /// case. A duplicate primary key is rejected before the row is written.
    ///
    /// An unknown table is [`ErrorKind::NotFound`]. A count mismatch, a type
    /// mismatch, a NULL primary key, a duplicate primary key, or a row longer
    /// than the maximum record length is [`ErrorKind::InvalidInput`].
    ///
    /// Outside a transaction the buffer pool is flushed before this returns.
    /// A failed call leaves the table unchanged.
    pub fn insert(&mut self, table: &str, values: &[Value]) -> io::Result<RecordId> {
        self.in_statement(|db| db.insert_unflushed(table, values))
    }

    fn insert_unflushed(&mut self, table: &str, values: &[Value]) -> io::Result<RecordId> {
        let mut ids = self.insert_all_unflushed(table, &[values.to_vec()])?;
        Ok(ids.remove(0))
    }

    /// Inserts every row, or none of them.
    ///
    /// Every row is encoded and, when the table has a primary key, checked
    /// against the index and against the other rows before the first write.
    /// The error cases match [`Self::insert`]. Outside a transaction the
    /// buffer pool is flushed before this returns. A failed call leaves the
    /// table unchanged.
    pub fn insert_all(&mut self, table: &str, rows: &[Vec<Value>]) -> io::Result<Vec<RecordId>> {
        self.in_statement(|db| db.insert_all_unflushed(table, rows))
    }

    /// Inserts every row without flushing.
    ///
    /// [`Self::execute_statement`] flushes once after the statement.
    pub(crate) fn insert_all_unflushed(
        &mut self,
        table: &str,
        rows: &[Vec<Value>],
    ) -> io::Result<Vec<RecordId>> {
        let schema = self.require_table(table)?;
        let mut encoded = Vec::with_capacity(rows.len());
        let mut keys = Vec::with_capacity(rows.len());
        for values in rows {
            encoded.push(encode_row(&schema, values)?);
            keys.push(primary_key_value(&schema, values)?);
        }
        let mut seen = BTreeSet::new();
        for key in keys.iter().flatten() {
            if !seen.insert(*key) || self.index_contains(&schema, *key)? {
                return Err(duplicate_key(*key));
            }
        }
        let mut ids = Vec::with_capacity(encoded.len());
        for (bytes, key) in encoded.iter().zip(&keys) {
            let id = self.records.insert(schema.id, bytes)?;
            if let Some(key) = key {
                self.index_insert(&schema, *key, id)?;
            }
            ids.push(id);
        }
        Ok(ids)
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
    /// The checks match [`Self::insert`] and run before the row is replaced.
    /// When the new bytes fit on the same page, the record id is unchanged.
    /// When they do not, the new row is inserted and the old row is deleted
    /// afterward, and the new id is returned. A crash between those writes
    /// can leave both copies.
    ///
    /// If the primary key or the record id changes, the old index entry is
    /// removed and the new key is inserted. A duplicate of another row's key
    /// is rejected before either write.
    ///
    /// An unknown table or a missing row is [`ErrorKind::NotFound`]. A count
    /// mismatch, a type mismatch, a NULL primary key, a duplicate primary
    /// key, or a row longer than the maximum record length is
    /// [`ErrorKind::InvalidInput`].
    ///
    /// Outside a transaction the buffer pool is flushed before this returns.
    /// A failed call leaves the row unchanged.
    pub fn update(&mut self, table: &str, id: RecordId, values: &[Value]) -> io::Result<RecordId> {
        self.in_statement(|db| db.update_unflushed(table, id, values))
    }

    fn update_unflushed(
        &mut self,
        table: &str,
        id: RecordId,
        values: &[Value],
    ) -> io::Result<RecordId> {
        let schema = self.require_table(table)?;
        let bytes = encode_row(&schema, values)?;
        let new_key = primary_key_value(&schema, values)?;
        let old_key = if let Some(index) = schema.primary_key {
            Some(require_primary_key(
                &schema,
                &self.read_row(&schema, id)?,
                index,
            )?)
        } else {
            None
        };
        if let (Some(new_key), Some(old_key)) = (new_key, old_key) {
            if new_key != old_key && self.index_contains(&schema, new_key)? {
                return Err(duplicate_key(new_key));
            }
        }
        let new_id = self.write_row(schema.id, id, &bytes)?;
        if let (Some(new_key), Some(old_key)) = (new_key, old_key) {
            if new_id != id || new_key != old_key {
                self.index_delete(&schema, old_key)?;
                self.index_insert(&schema, new_key, new_id)?;
            }
        }
        Ok(new_id)
    }

    /// Applies `pending` updates, or none of them.
    ///
    /// Every new row is encoded first. When the table has a primary key, the
    /// keys of rows that are not in `pending`, plus the new keys, must be
    /// unique. Old keys of the updated rows are then removed from the index,
    /// and each row is written and inserted under its new key. That order
    /// lets `id = id + 1` succeed. A duplicate or NULL key writes nothing.
    ///
    /// Returns the record id of each row after the write, in `pending` order.
    /// Outside a transaction the buffer pool is flushed before this returns.
    /// A failed call leaves every row unchanged.
    pub fn apply_update(
        &mut self,
        table: &str,
        pending: &[(RecordId, Vec<Value>)],
    ) -> io::Result<Vec<RecordId>> {
        self.in_statement(|db| db.apply_update_unflushed(table, pending))
    }

    /// Applies `pending` without flushing.
    ///
    /// [`Self::execute_statement`] flushes once after the statement.
    pub(crate) fn apply_update_unflushed(
        &mut self,
        table: &str,
        pending: &[(RecordId, Vec<Value>)],
    ) -> io::Result<Vec<RecordId>> {
        let schema = self.require_table(table)?;
        if pending.is_empty() {
            return Ok(Vec::new());
        }
        let mut encoded = Vec::with_capacity(pending.len());
        for (_, values) in pending {
            encoded.push(encode_row(&schema, values)?);
        }
        if schema.primary_key.is_none() {
            let mut ids = Vec::with_capacity(pending.len());
            for (index, (id, _)) in pending.iter().enumerate() {
                ids.push(self.write_row(schema.id, *id, &encoded[index])?);
            }
            return Ok(ids);
        }

        let index = schema.primary_key.expect("primary key checked above");
        let mut new_keys = Vec::with_capacity(pending.len());
        for (_, values) in pending {
            new_keys.push(require_primary_key(&schema, values, index)?);
        }
        let mut old_keys = Vec::with_capacity(pending.len());
        for (id, _) in pending {
            let old = self.read_row(&schema, *id)?;
            old_keys.push(require_primary_key(&schema, &old, index)?);
        }
        self.ensure_updated_keys_unique(&schema, &old_keys, &new_keys)?;

        for key in &old_keys {
            self.index_delete(&schema, *key)?;
        }
        let mut ids = Vec::with_capacity(pending.len());
        for (index, (id, _)) in pending.iter().enumerate() {
            let new_id = self.write_row(schema.id, *id, &encoded[index])?;
            self.index_insert(&schema, new_keys[index], new_id)?;
            ids.push(new_id);
        }
        Ok(ids)
    }

    /// Levels in `table`'s primary-key index.
    ///
    /// `Ok(None)` when the table has no primary key. A root leaf has height 1.
    pub fn index_height(&mut self, table: &str) -> io::Result<Option<u32>> {
        let schema = self.require_table(table)?;
        let Some(root) = schema.index_root else {
            return Ok(None);
        };
        let tree = BTree::open(root, schema.id);
        Ok(Some(tree.height(self.records.pages_mut())?))
    }

    /// Deletes the row identified by `id` in `table`.
    ///
    /// An unknown table or a missing row is [`ErrorKind::NotFound`]. When the
    /// table has a primary key, the key is removed after the row.
    ///
    /// Outside a transaction the buffer pool is flushed before this returns.
    /// A failed call leaves the row in place.
    pub fn delete(&mut self, table: &str, id: RecordId) -> io::Result<()> {
        self.in_statement(|db| db.delete_unflushed(table, id))
    }

    /// Deletes one row without flushing.
    ///
    /// [`Self::execute_statement`] flushes once after the statement, not after
    /// each row.
    pub(crate) fn delete_unflushed(&mut self, table: &str, id: RecordId) -> io::Result<()> {
        let schema = self.require_table(table)?;
        let key = if schema.primary_key.is_some() {
            primary_key_value(&schema, &self.read_row(&schema, id)?)?
        } else {
            None
        };
        self.records.delete(schema.id, id)?;
        if let Some(key) = key {
            self.index_delete(&schema, key)?;
        }
        Ok(())
    }

    fn read_row(&mut self, schema: &TableSchema, id: RecordId) -> io::Result<Vec<Value>> {
        let bytes = self.records.get(schema.id, id)?;
        decode_row(schema, id, &bytes)
    }

    fn write_row(&mut self, table: TableId, id: RecordId, bytes: &[u8]) -> io::Result<RecordId> {
        if self.records.try_update(table, id, bytes)? {
            Ok(id)
        } else {
            let new_id = self.records.insert(table, bytes)?;
            self.records.delete(table, id)?;
            Ok(new_id)
        }
    }

    fn ensure_updated_keys_unique(
        &mut self,
        schema: &TableSchema,
        old_keys: &[i64],
        new_keys: &[i64],
    ) -> io::Result<()> {
        // Keys of the updated rows are removed before the new keys go in, so
        // only an indexed key that belongs to a row outside the update conflicts.
        let released: BTreeSet<i64> = old_keys.iter().copied().collect();
        let mut claimed = BTreeSet::new();
        for &key in new_keys {
            if !claimed.insert(key) {
                return Err(duplicate_key(key));
            }
            if !released.contains(&key) && self.index_contains(schema, key)? {
                return Err(duplicate_key(key));
            }
        }
        Ok(())
    }

    fn index_contains(&mut self, schema: &TableSchema, key: i64) -> io::Result<bool> {
        let tree = open_index(schema)?;
        Ok(tree.get(self.records.pages_mut(), key)?.is_some())
    }

    fn index_insert(&mut self, schema: &TableSchema, key: i64, id: RecordId) -> io::Result<()> {
        let tree = open_index(schema)?;
        tree.insert(self.records.pages_mut(), key, id)
    }

    fn index_delete(&mut self, schema: &TableSchema, key: i64) -> io::Result<()> {
        let tree = open_index(schema)?;
        tree.delete(self.records.pages_mut(), key)?;
        Ok(())
    }

    fn require_table(&self, name: &str) -> io::Result<TableSchema> {
        self.table(name)
            .cloned()
            .ok_or_else(|| io::Error::new(ErrorKind::NotFound, format!("table not found: {name}")))
    }
}

fn open_index(schema: &TableSchema) -> io::Result<BTree> {
    let root = schema
        .index_root
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "missing index root"))?;
    Ok(BTree::open(root, schema.id))
}

fn primary_key_value(schema: &TableSchema, values: &[Value]) -> io::Result<Option<i64>> {
    let Some(index) = schema.primary_key else {
        return Ok(None);
    };
    Ok(Some(require_primary_key(schema, values, index)?))
}

fn require_primary_key(schema: &TableSchema, values: &[Value], index: usize) -> io::Result<i64> {
    match values.get(index) {
        Some(Value::Integer(key)) => Ok(*key),
        Some(Value::Null) => Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("NULL in primary key column: {}", schema.columns[index].name),
        )),
        _ => Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "type mismatch for column {}: expected INTEGER",
                schema
                    .columns
                    .get(index)
                    .map(|column| column.name.as_str())
                    .unwrap_or("?")
            ),
        )),
    }
}

fn duplicate_key(key: i64) -> io::Error {
    io::Error::new(
        ErrorKind::InvalidInput,
        format!("duplicate primary key: {key}"),
    )
}

#[cfg(test)]
mod tests {
    use super::Database;
    use crate::catalog::{ColumnSpec, ColumnType};
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
            let _ = fs::remove_file(crate::wal::wal_path(&path));
            TempDb { path }
        }

        fn path(&self) -> &PathBuf {
            &self.path
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
            let _ = fs::remove_file(crate::wal::wal_path(&self.path));
        }
    }

    fn open_users() -> (TempDb, Database) {
        let db = TempDb::new("rows");
        let mut database = Database::open(db.path()).unwrap();
        database
            .create_table(
                "users",
                &[
                    ColumnSpec::new("id", ColumnType::Integer),
                    ColumnSpec::new("name", ColumnType::Text),
                    ColumnSpec::new("age", ColumnType::Integer),
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
                        ColumnSpec::new("id", ColumnType::Integer),
                        ColumnSpec::new("name", ColumnType::Text),
                        ColumnSpec::new("age", ColumnType::Integer),
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
                    &[
                        ColumnSpec::new("id", ColumnType::Integer),
                        ColumnSpec::new("name", ColumnType::Text),
                    ],
                )
                .unwrap();
            database
                .create_table(
                    "posts",
                    &[
                        ColumnSpec::new("id", ColumnType::Integer),
                        ColumnSpec::new("title", ColumnType::Text),
                    ],
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

    #[test]
    fn primary_key_is_unique_and_follows_the_row() {
        use crate::btree::BTree;

        let db = TempDb::new("pkrows");
        let mut database = Database::open(db.path()).unwrap();
        database
            .create_table(
                "users",
                &[
                    ColumnSpec {
                        name: "id",
                        column_type: ColumnType::Integer,
                        primary_key: true,
                    },
                    ColumnSpec::new("name", ColumnType::Text),
                ],
            )
            .unwrap();

        let err = database
            .insert("users", &[Value::Null, Value::Text("x".to_string())])
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "NULL in primary key column: id");
        assert!(database.scan("users").unwrap().is_empty());

        let first = database
            .insert("users", &[Value::Integer(1), Value::Text("a".repeat(2000))])
            .unwrap();
        let err = database
            .insert_all(
                "users",
                &[
                    vec![Value::Integer(2), Value::Text("Bea".to_string())],
                    vec![Value::Integer(1), Value::Text("Cara".to_string())],
                ],
            )
            .unwrap_err();
        assert_eq!(err.to_string(), "duplicate primary key: 1");
        assert_eq!(database.scan("users").unwrap().len(), 1);

        let err = database
            .insert_all(
                "users",
                &[
                    vec![Value::Integer(3), Value::Text("a".to_string())],
                    vec![Value::Integer(3), Value::Text("b".to_string())],
                ],
            )
            .unwrap_err();
        assert_eq!(err.to_string(), "duplicate primary key: 3");
        assert_eq!(database.scan("users").unwrap().len(), 1);

        let second = database
            .insert("users", &[Value::Integer(2), Value::Text("b".repeat(2000))])
            .unwrap();
        let err = database
            .update(
                "users",
                second,
                &[Value::Integer(1), Value::Text("b".repeat(2000))],
            )
            .unwrap_err();
        assert_eq!(err.to_string(), "duplicate primary key: 1");
        assert_eq!(
            database.get("users", second).unwrap(),
            vec![Value::Integer(2), Value::Text("b".repeat(2000))]
        );

        let moved = database
            .update(
                "users",
                first,
                &[Value::Integer(1), Value::Text("c".repeat(2500))],
            )
            .unwrap();
        assert_ne!(moved, first);
        let schema = database.table("users").unwrap().clone();
        let tree = BTree::open(schema.index_root.unwrap(), schema.id);
        assert_eq!(
            tree.get(database.records.pages_mut(), 1).unwrap(),
            Some(moved)
        );
        assert_eq!(
            database.get("users", moved).unwrap()[1],
            Value::Text("c".repeat(2500))
        );

        database.delete("users", moved).unwrap();
        assert!(tree.get(database.records.pages_mut(), 1).unwrap().is_none());
        let again = database
            .insert(
                "users",
                &[Value::Integer(1), Value::Text("Ada".to_string())],
            )
            .unwrap();
        assert_eq!(
            tree.get(database.records.pages_mut(), 1).unwrap(),
            Some(again)
        );

        let pending = vec![
            (
                again,
                vec![Value::Integer(2), Value::Text("a".repeat(2000))],
            ),
            (
                second,
                vec![Value::Integer(3), Value::Text("b".repeat(2000))],
            ),
        ];
        database.apply_update("users", &pending).unwrap();
        let keys: Vec<i64> = tree
            .scan_all(database.records.pages_mut())
            .unwrap()
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(keys, vec![2, 3]);

        let err = database
            .apply_update(
                "users",
                &[
                    (
                        again,
                        vec![Value::Integer(9), Value::Text("a".repeat(2000))],
                    ),
                    (
                        second,
                        vec![Value::Integer(9), Value::Text("b".repeat(2000))],
                    ),
                ],
            )
            .unwrap_err();
        assert_eq!(err.to_string(), "duplicate primary key: 9");
        let keys: Vec<i64> = tree
            .scan_all(database.records.pages_mut())
            .unwrap()
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(keys, vec![2, 3]);
    }
}
