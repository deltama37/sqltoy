//! Insert, scan, update, and delete of typed rows.
//!
//! Values are checked against the catalog schema. Each stored row is a
//! version: an insert creates one, a delete sets `xmax`, and an update does
//! both. The old bytes are not rewritten in place. A primary key is kept in
//! the table's B+Tree as `(key, record id)`, so older versions stay
//! reachable until `VACUUM`. It cannot be NULL, and at most one version of
//! a key is visible to a transaction.

use std::collections::BTreeSet;
use std::io::{self, ErrorKind};

use crate::btree::BTree;
use crate::catalog::{Database, TableSchema, WriteEntry, WriteKind};
use crate::mvcc::{
    clear_xmax, committed_in, encode_record, serialization_failure, set_xmax, split_record,
    visible, Snapshot, VersionHeader,
};
use crate::record::{RecordId, TableId, MAX_RECORD_SIZE};
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
        let replacing = BTreeSet::new();
        for key in keys.iter().flatten() {
            if !seen.insert(*key) {
                return Err(duplicate_key(*key));
            }
            self.check_key_available(&schema, *key, &replacing)?;
        }
        let xid = self.current_xid()?;
        let mut ids = Vec::with_capacity(encoded.len());
        for (row, key) in encoded.iter().zip(&keys) {
            let id = self.insert_version(&schema, xid, row)?;
            if let Some(key) = key {
                self.index_insert(&schema, *key, id)?;
            }
            self.record_write(WriteEntry {
                table: schema.id,
                rid: id,
                key: *key,
                kind: WriteKind::Inserted,
            })?;
            ids.push(id);
        }
        Ok(ids)
    }

    /// Reads the row identified by `id` in `table`.
    ///
    /// An unknown table or a missing row is [`ErrorKind::NotFound`]. A stored
    /// record that does not match the schema is [`ErrorKind::InvalidData`].
    pub fn get(&mut self, table: &str, id: RecordId) -> io::Result<Vec<Value>> {
        self.in_statement(|db| db.read_visible(table, id))
    }

    /// Rows of `table` with their record ids, in page order then slot order.
    ///
    /// An unknown table is [`ErrorKind::NotFound`]. A stored record that
    /// cannot be decoded with the schema is [`ErrorKind::InvalidData`].
    pub fn scan(&mut self, table: &str) -> io::Result<Vec<(RecordId, Vec<Value>)>> {
        self.in_statement(|db| {
            let schema = db.require_table(table)?;
            db.visible_rows(&schema)
        })
    }

    /// Live records in `table`, including versions the current snapshot cannot see.
    pub fn stored_version_count(&mut self, table: &str) -> io::Result<usize> {
        let schema = self.require_table(table)?;
        Ok(self.records.scan(schema.id)?.len())
    }

    /// Primary-key index entries, including versions that are not visible.
    ///
    /// `Ok(None)` when the table has no primary key.
    pub fn index_entry_count(&mut self, table: &str) -> io::Result<Option<usize>> {
        let schema = self.require_table(table)?;
        let Some(root) = schema.index_root else {
            return Ok(None);
        };
        let tree = BTree::open(root, schema.id);
        Ok(Some(tree.scan_all(self.records.pages_mut())?.len()))
    }

    /// Decodes `bytes` when the version is visible to the executing transaction.
    pub(crate) fn visible_values(
        &self,
        schema: &TableSchema,
        id: RecordId,
        bytes: &[u8],
    ) -> io::Result<Option<Vec<Value>>> {
        let (header, row) = split_record(id, bytes)?;
        if !visible(&header, &self.current_snapshot()?) {
            return Ok(None);
        }
        Ok(Some(decode_row(schema, id, row)?))
    }

    /// Reads `id` when it is visible. `Ok(None)` when the version is not visible.
    ///
    /// A missing record is [`ErrorKind::NotFound`].
    pub(crate) fn row_if_visible(
        &mut self,
        schema: &TableSchema,
        id: RecordId,
    ) -> io::Result<Option<Vec<Value>>> {
        let bytes = self.records.get(schema.id, id)?;
        self.visible_values(schema, id, &bytes)
    }

    /// Replaces the visible version identified by `id` with a new version.
    ///
    /// The old version stays in the table with `xmax` set to this transaction.
    /// The new version is a separate record. The returned id is that record,
    /// which is different from `id`. The checks match [`Self::insert`] and run
    /// before either version is written. A concurrent writer that has already
    /// set `xmax` produces [`ErrorKind::Other`]
    /// (`serialization failure: row was modified by a concurrent transaction`).
    ///
    /// An unknown table or a version this transaction cannot see is
    /// [`ErrorKind::NotFound`]. A count mismatch, a type mismatch, a NULL
    /// primary key, a duplicate primary key, or a row longer than the maximum
    /// record length is [`ErrorKind::InvalidInput`].
    ///
    /// Outside a transaction the buffer pool is flushed before this returns.
    /// A failed call leaves both versions unchanged.
    pub fn update(&mut self, table: &str, id: RecordId, values: &[Value]) -> io::Result<RecordId> {
        self.in_statement(|db| db.update_unflushed(table, id, values))
    }

    fn update_unflushed(
        &mut self,
        table: &str,
        id: RecordId,
        values: &[Value],
    ) -> io::Result<RecordId> {
        let mut ids = self.apply_update_unflushed(table, &[(id, values.to_vec())])?;
        Ok(ids.remove(0))
    }

    /// Applies `pending` updates, or none of them.
    ///
    /// Every new row is encoded first, then every target is checked for
    /// visibility and write-write conflicts. When the table has a primary key,
    /// the new keys must be unique among themselves and against versions that
    /// are not being replaced. The versions in `pending` are part of that
    /// check, so `id = id + 1` succeeds. A duplicate, NULL key, or conflict
    /// writes nothing.
    ///
    /// Each update deletes the old version and inserts a new one. The old
    /// index entry stays until [`Self::execute_statement`] runs `VACUUM` or
    /// the transaction rolls back an insert. Returns the new record id of
    /// each row, in `pending` order.
    ///
    /// Outside a transaction the buffer pool is flushed before this returns.
    /// A failed call leaves every version unchanged.
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
        let mut new_keys = Vec::with_capacity(pending.len());
        for (_, values) in pending {
            encoded.push(encode_row(&schema, values)?);
            new_keys.push(primary_key_value(&schema, values)?);
        }

        let snapshot = self.current_snapshot()?;
        let active = self.active_xid_set();
        let mut old_keys = Vec::with_capacity(pending.len());
        let mut replacing = BTreeSet::new();
        for (id, _) in pending {
            let bytes = self.records.get(schema.id, *id)?;
            let (header, row) = split_record(*id, &bytes)?;
            if !visible(&header, &snapshot) {
                return Err(not_found(*id));
            }
            ensure_conflict(&header, &snapshot, &active)?;
            let old = decode_row(&schema, *id, row)?;
            old_keys.push(primary_key_value(&schema, &old)?);
            replacing.insert(*id);
        }
        let mut claimed = BTreeSet::new();
        for key in new_keys.iter().flatten() {
            if !claimed.insert(*key) {
                return Err(duplicate_key(*key));
            }
            self.check_key_available(&schema, *key, &replacing)?;
        }

        let xid = snapshot.xid;
        let mut ids = Vec::with_capacity(pending.len());
        for (index, (id, _)) in pending.iter().enumerate() {
            self.mark_deleted(schema.id, *id, xid)?;
            self.record_write(WriteEntry {
                table: schema.id,
                rid: *id,
                key: old_keys[index],
                kind: WriteKind::Deleted,
            })?;
            let new_id = self.insert_version(&schema, xid, &encoded[index])?;
            if let Some(key) = new_keys[index] {
                self.index_insert(&schema, key, new_id)?;
            }
            self.record_write(WriteEntry {
                table: schema.id,
                rid: new_id,
                key: new_keys[index],
                kind: WriteKind::Inserted,
            })?;
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

    /// Deletes the visible version identified by `id`.
    ///
    /// Sets `xmax` to this transaction. The record and its index entry stay
    /// until `VACUUM`, crash cleanup, or rollback. An unknown table or a
    /// version this transaction cannot see is [`ErrorKind::NotFound`]. A
    /// concurrent writer produces the same serialization failure as
    /// [`Self::update`].
    ///
    /// Outside a transaction the buffer pool is flushed before this returns.
    /// A failed call leaves the version unchanged.
    pub fn delete(&mut self, table: &str, id: RecordId) -> io::Result<()> {
        self.in_statement(|db| db.delete_unflushed(table, id))
    }

    /// Deletes one version without flushing.
    ///
    /// [`Self::execute_statement`] flushes once after the statement, not after
    /// each row.
    pub(crate) fn delete_unflushed(&mut self, table: &str, id: RecordId) -> io::Result<()> {
        let schema = self.require_table(table)?;
        let bytes = self.records.get(schema.id, id)?;
        let (header, row) = split_record(id, &bytes)?;
        let snapshot = self.current_snapshot()?;
        if !visible(&header, &snapshot) {
            return Err(not_found(id));
        }
        ensure_conflict(&header, &snapshot, &self.active_xid_set())?;
        let values = decode_row(&schema, id, row)?;
        let key = primary_key_value(&schema, &values)?;
        self.mark_deleted(schema.id, id, snapshot.xid)?;
        self.record_write(WriteEntry {
            table: schema.id,
            rid: id,
            key,
            kind: WriteKind::Deleted,
        })?;
        Ok(())
    }

    fn insert_version(
        &mut self,
        schema: &TableSchema,
        xid: u64,
        row: &[u8],
    ) -> io::Result<RecordId> {
        let bytes = encode_record(VersionHeader::created_by(xid), row);
        if bytes.len() > MAX_RECORD_SIZE {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("row too large for table {}", schema.name),
            ));
        }
        self.records.insert(schema.id, &bytes)
    }

    /// Rejects `key` when some other version already owns it.
    ///
    /// `replacing` versions are the ones this statement is about to delete, so
    /// they do not count. A version deleted by this transaction, or deleted
    /// by a commit this snapshot sees, is ignored. A version this snapshot
    /// still sees but whose delete committed later, and an uncommitted creator
    /// or deleter other than this transaction, are serialization failures.
    /// Anything else is a duplicate key.
    fn check_key_available(
        &mut self,
        schema: &TableSchema,
        key: i64,
        replacing: &BTreeSet<RecordId>,
    ) -> io::Result<()> {
        if schema.index_root.is_none() {
            return Ok(());
        }
        let tree = open_index(schema)?;
        let ids = tree.lookup(self.records.pages_mut(), key)?;
        let snapshot = self.current_snapshot()?;
        for id in ids {
            if replacing.contains(&id) {
                continue;
            }
            let bytes = self.records.get(schema.id, id)?;
            let (header, _) = split_record(id, &bytes)?;
            if header.xmax == snapshot.xid {
                continue;
            }
            if header.xmax_committed() {
                // A delete committed after this snapshot leaves the old version
                // visible here, so inserting the key would show it twice.
                if !committed_in(header.xmax, true, &snapshot)
                    && committed_in(header.xmin, header.xmin_committed(), &snapshot)
                {
                    return Err(serialization_failure());
                }
                continue;
            }
            if !header.xmin_committed() && header.xmin != snapshot.xid {
                return Err(serialization_failure());
            }
            if header.xmax != 0 && !header.xmax_committed() && header.xmax != snapshot.xid {
                return Err(serialization_failure());
            }
            return Err(duplicate_key(key));
        }
        Ok(())
    }

    fn mark_deleted(&mut self, table: TableId, id: RecordId, xid: u64) -> io::Result<()> {
        let mut bytes = self.records.get(table, id)?;
        set_xmax(&mut bytes, xid)?;
        self.records.update(table, id, &bytes)
    }

    fn index_insert(&mut self, schema: &TableSchema, key: i64, id: RecordId) -> io::Result<()> {
        let tree = open_index(schema)?;
        tree.insert(self.records.pages_mut(), key, id)
    }

    fn index_delete(&mut self, schema: &TableSchema, key: i64, id: RecordId) -> io::Result<()> {
        let tree = open_index(schema)?;
        if !tree.delete(self.records.pages_mut(), key, id)? {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!("missing index entry: {key} {id}"),
            ));
        }
        Ok(())
    }

    pub(crate) fn undo_inserted(&mut self, entry: &WriteEntry) -> io::Result<()> {
        if let Some(key) = entry.key {
            let schema = self.schema_by_id(entry.table)?;
            self.index_delete(&schema, key, entry.rid)?;
        }
        self.records.delete(entry.table, entry.rid)
    }

    pub(crate) fn undo_deleted(&mut self, entry: &WriteEntry) -> io::Result<()> {
        let mut bytes = self.records.get(entry.table, entry.rid)?;
        clear_xmax(&mut bytes)?;
        self.records.update(entry.table, entry.rid, &bytes)
    }

    /// Drops versions whose creator never committed, and clears `xmax` when
    /// the deleter never committed.
    ///
    /// Catalog records are not versions and are left alone. A table created
    /// by a transaction that another session flushed therefore remains after
    /// recovery.
    pub(crate) fn purge_aborted_versions(&mut self) -> io::Result<()> {
        let schemas = self.tables().to_vec();
        for schema in schemas {
            let rows = self.records.scan(schema.id)?;
            for (id, bytes) in rows {
                let Some(header) = VersionHeader::decode(&bytes) else {
                    self.records.delete(schema.id, id)?;
                    continue;
                };
                if !header.xmin_committed() {
                    self.remove_index_entry(&schema, id, &bytes)?;
                    self.records.delete(schema.id, id)?;
                } else if header.xmax != 0 && !header.xmax_committed() {
                    let mut bytes = bytes;
                    clear_xmax(&mut bytes)?;
                    self.records.update(schema.id, id, &bytes)?;
                }
            }
        }
        Ok(())
    }

    /// Removes dead versions whose `xmax` is committed for every active snapshot.
    ///
    /// Returns how many versions were removed. Does not record them in a write
    /// set: the caller flushes after this returns.
    pub(crate) fn vacuum_dead_versions(&mut self) -> io::Result<u64> {
        let snapshots = self.active_snapshots();
        let schemas = self.tables().to_vec();
        let mut removed = 0u64;
        for schema in schemas {
            let rows = self.records.scan(schema.id)?;
            for (id, bytes) in rows {
                let (header, _) = split_record(id, &bytes)?;
                if !header.xmax_committed() {
                    continue;
                }
                let dead = snapshots.iter().all(|snapshot| {
                    header.xmax < snapshot.bound && !snapshot.active.contains(&header.xmax)
                });
                if !dead {
                    continue;
                }
                self.remove_index_entry(&schema, id, &bytes)?;
                self.records.delete(schema.id, id)?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    fn remove_index_entry(
        &mut self,
        schema: &TableSchema,
        id: RecordId,
        bytes: &[u8],
    ) -> io::Result<()> {
        if schema.index_root.is_none() {
            return Ok(());
        }
        if let Some(key) = version_key(schema, id, bytes)? {
            let tree = open_index(schema)?;
            tree.delete(self.records.pages_mut(), key, id)?;
            return Ok(());
        }
        let tree = open_index(schema)?;
        let entries = tree.scan_all(self.records.pages_mut())?;
        for (key, rid) in entries {
            if rid == id {
                tree.delete(self.records.pages_mut(), key, rid)?;
            }
        }
        Ok(())
    }

    fn schema_by_id(&self, id: TableId) -> io::Result<TableSchema> {
        self.tables()
            .iter()
            .find(|table| table.id == id)
            .cloned()
            .ok_or_else(|| {
                io::Error::new(ErrorKind::NotFound, format!("table not found: {}", id.0))
            })
    }

    fn visible_rows(&mut self, schema: &TableSchema) -> io::Result<Vec<(RecordId, Vec<Value>)>> {
        let mut rows = Vec::new();
        for (id, bytes) in self.records.scan(schema.id)? {
            if let Some(values) = self.visible_values(schema, id, &bytes)? {
                rows.push((id, values));
            }
        }
        Ok(rows)
    }

    fn read_visible(&mut self, table: &str, id: RecordId) -> io::Result<Vec<Value>> {
        let schema = self.require_table(table)?;
        self.row_if_visible(&schema, id)?
            .ok_or_else(|| not_found(id))
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

fn not_found(id: RecordId) -> io::Error {
    io::Error::new(ErrorKind::NotFound, format!("record not found: {id}"))
}

fn version_key(schema: &TableSchema, id: RecordId, bytes: &[u8]) -> io::Result<Option<i64>> {
    let (_, row) = split_record(id, bytes)?;
    let values = decode_row(schema, id, row)?;
    primary_key_value(schema, &values)
}

/// First updater wins. `active` is the transactions running now, not the
/// snapshot's frozen set.
fn ensure_conflict(
    header: &VersionHeader,
    snapshot: &Snapshot,
    active: &BTreeSet<u64>,
) -> io::Result<()> {
    if header.xmax == 0 || header.xmax == snapshot.xid {
        return Ok(());
    }
    let running = active.contains(&header.xmax);
    let committed_later = header.xmax_committed() && !committed_in(header.xmax, true, snapshot);
    if running || committed_later {
        Err(serialization_failure())
    } else {
        Ok(())
    }
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
        assert_ne!(updated, alice);
        let err = database.get("users", alice).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(
            database.get("users", updated).unwrap(),
            row(1, Some("Alicia"), Some(31))
        );

        let shrunk = database
            .update("users", updated, &row(1, Some("Al"), Some(31)))
            .unwrap();
        assert_ne!(shrunk, updated);

        database.delete("users", bob).unwrap();
        let err = database.get("users", bob).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(err.to_string(), "record not found: 2:1");
        assert_eq!(
            database.scan("users").unwrap(),
            vec![(shrunk, row(1, Some("Al"), Some(31)))]
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
    fn update_creates_a_new_version() {
        let (_db, mut database) = open_users();
        let id = database
            .insert("users", &row(1, Some("Al"), Some(1)))
            .unwrap();
        let grown = database
            .update("users", id, &row(1, Some("Alicia"), Some(2)))
            .unwrap();
        assert_ne!(grown, id);
        assert_eq!(grown.page_id, PageId(2));
        let err = database.get("users", id).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(
            database.get("users", grown).unwrap(),
            row(1, Some("Alicia"), Some(2))
        );
        assert_eq!(database.scan("users").unwrap().len(), 1);
    }

    #[test]
    fn update_that_does_not_fit_moves_the_row() {
        let (_db, mut database) = open_users();
        // An update always stores a new version. The previous record id is
        // no longer visible.
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
        assert_ne!(kept, second);
        let err = database.get("users", second).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);

        let moved = database
            .update("users", first, &row(1, Some(&"c".repeat(2100)), Some(1)))
            .unwrap();
        assert_ne!(moved, first);

        let err = database.get("users", first).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(err.to_string(), format!("record not found: {first}"));

        let scanned = database.scan("users").unwrap();
        assert_eq!(scanned.len(), 2);
        assert_eq!(scanned.iter().filter(|(id, _)| *id == first).count(), 0);
        assert_eq!(scanned.iter().filter(|(id, _)| *id == moved).count(), 1);
        assert!(scanned
            .iter()
            .any(|(_, values)| values == &row(2, Some(&"b".repeat(2000)), Some(3))));
        assert!(scanned
            .iter()
            .any(|(_, values)| values == &row(1, Some(&"c".repeat(2100)), Some(1))));
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
        assert!(tree
            .lookup(database.records.pages_mut(), 1)
            .unwrap()
            .contains(&moved));
        assert_eq!(
            database.get("users", moved).unwrap()[1],
            Value::Text("c".repeat(2500))
        );

        database.delete("users", moved).unwrap();
        assert!(database.get("users", moved).is_err());
        assert!(!tree
            .lookup(database.records.pages_mut(), 1)
            .unwrap()
            .is_empty());
        let again = database
            .insert(
                "users",
                &[Value::Integer(1), Value::Text("Ada".to_string())],
            )
            .unwrap();
        assert!(tree
            .lookup(database.records.pages_mut(), 1)
            .unwrap()
            .contains(&again));

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
        let updated = database.apply_update("users", &pending).unwrap();
        assert_eq!(visible_keys(&mut database), vec![2, 3]);

        let err = database
            .apply_update(
                "users",
                &[
                    (
                        updated[0],
                        vec![Value::Integer(9), Value::Text("a".repeat(2000))],
                    ),
                    (
                        updated[1],
                        vec![Value::Integer(9), Value::Text("b".repeat(2000))],
                    ),
                ],
            )
            .unwrap_err();
        assert_eq!(err.to_string(), "duplicate primary key: 9");
        assert_eq!(visible_keys(&mut database), vec![2, 3]);
    }

    fn visible_keys(database: &mut Database) -> Vec<i64> {
        let mut keys: Vec<i64> = database
            .scan("users")
            .unwrap()
            .into_iter()
            .map(|(_, values)| match values[0] {
                Value::Integer(key) => key,
                _ => panic!("id"),
            })
            .collect();
        keys.sort();
        keys
    }
}
