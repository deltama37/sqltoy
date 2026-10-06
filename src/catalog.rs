//! Table schemas stored in the catalog.
//!
//! The catalog is the record set of [`TableId::CATALOG`]. Each user table is
//! one record. Opening a database reads those records back into memory.

use std::collections::BTreeSet;
use std::fmt;
use std::io::{self, ErrorKind};
use std::path::Path;
use std::str::FromStr;

use crate::btree::BTree;
use crate::buffer::{BufferStats, DEFAULT_POOL_PAGES};
use crate::mvcc::Snapshot;
use crate::page::{Page, PageId, CLEANUP_FLAG_OFFSET, NEXT_XID_OFFSET};
use crate::record::{RecordFile, RecordId, TableId, MAX_RECORD_SIZE};

/// SQL column type stored in the catalog.
///
/// Display is `INTEGER` or `TEXT`. Parsing accepts those names in any ASCII
/// case. The on-disk codes are 1 and 2. Any other string or code is an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    /// 64-bit signed integer.
    Integer,
    /// UTF-8 string.
    Text,
}

impl ColumnType {
    fn code(self) -> u8 {
        match self {
            ColumnType::Integer => 1,
            ColumnType::Text => 2,
        }
    }

    fn from_code(code: u8) -> Option<ColumnType> {
        match code {
            1 => Some(ColumnType::Integer),
            2 => Some(ColumnType::Text),
            _ => None,
        }
    }
}

impl fmt::Display for ColumnType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ColumnType::Integer => f.write_str("INTEGER"),
            ColumnType::Text => f.write_str("TEXT"),
        }
    }
}

impl FromStr for ColumnType {
    type Err = io::Error;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        if raw.eq_ignore_ascii_case("integer") {
            Ok(ColumnType::Integer)
        } else if raw.eq_ignore_ascii_case("text") {
            Ok(ColumnType::Text)
        } else {
            Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("unknown column type: {raw}"),
            ))
        }
    }
}

/// One column in a [`TableSchema`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// Column name, stored with the spelling from the definition.
    pub name: String,
    /// Column type.
    pub column_type: ColumnType,
}

/// One column passed to [`Database::create_table`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnSpec<'a> {
    /// Column name.
    pub name: &'a str,
    /// Column type.
    pub column_type: ColumnType,
    /// Whether this column is the table's primary key.
    pub primary_key: bool,
}

impl<'a> ColumnSpec<'a> {
    /// A column that is not a primary key.
    pub const fn new(name: &'a str, column_type: ColumnType) -> ColumnSpec<'a> {
        ColumnSpec {
            name,
            column_type,
            primary_key: false,
        }
    }
}

/// Name and columns of one user table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSchema {
    /// Table id assigned at creation.
    pub id: TableId,
    /// Table name, stored with the spelling from the definition.
    pub name: String,
    /// Columns in definition order.
    pub columns: Vec<Column>,
    /// Index of the primary-key column, when the table has one.
    pub primary_key: Option<usize>,
    /// Root page of the primary-key index, when the table has one.
    pub index_root: Option<PageId>,
}

/// Identity of one session.
///
/// A [`Database`] starts with one session, returned by
/// [`Database::default_session`]. [`Database::execute`] and the row methods
/// use that session. [`Database::execute_in`] runs on another session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(pub u32);

/// An open database file and the schemas loaded from its catalog.
///
/// Pages sit in a [`crate::buffer::BufferPool`]. Each session has at most one
/// transaction. Outside an explicit transaction, [`Self::execute_statement`]
/// and the public table methods run as a one-statement transaction and commit
/// it when they succeed. A statement that fails is rolled back to the
/// savepoint taken at its start, and the catalog is reloaded from pages.
/// Inside [`Self::begin`], those methods leave the session's versions
/// uncommitted until [`Self::commit`] or [`Self::rollback`]. Commit sets the
/// version flags and flushes. Rollback undoes that session's write set and
/// flushes; it does not drop another session's dirty pages. Dropping a
/// [`Database`] does not flush, so an open transaction is lost unless some
/// other session's commit already wrote its pages.
pub struct Database {
    pub(crate) records: RecordFile,
    tables: Vec<TableSchema>,
    /// Cached header page. Transaction ids are written here, through the pool.
    header: Page,
    next_xid: u64,
    sessions: Vec<Session>,
    default_session: SessionId,
    /// Session whose statement or row call is running.
    exec_session: SessionId,
    next_session_id: u32,
    /// When `false`, plans use sequential scans, nested loops, and a sort.
    planner_enabled: bool,
}

struct Session {
    id: SessionId,
    txn: Option<Transaction>,
}

pub(crate) struct Transaction {
    pub xid: u64,
    pub snapshot: Snapshot,
    pub write_set: Vec<WriteEntry>,
    pub explicit: bool,
    pub created_tables: Vec<CreatedTable>,
    /// Pages were changed outside the write set (`VACUUM`).
    pub physical_writes: bool,
}

pub(crate) struct CreatedTable {
    pub id: TableId,
    pub catalog_rid: RecordId,
}

#[derive(Clone, Copy)]
pub(crate) enum WriteKind {
    Inserted,
    Deleted,
}

#[derive(Clone)]
pub(crate) struct WriteEntry {
    pub table: TableId,
    pub rid: RecordId,
    pub key: Option<i64>,
    pub kind: WriteKind,
}

impl Database {
    /// Opens the database at `path` and loads every catalog record.
    ///
    /// Uses [`DEFAULT_POOL_PAGES`] frames. An empty file is created. Schemas
    /// are ordered by table id. A record that does not match the catalog
    /// layout, an unknown column type, a table id below
    /// [`TableId::FIRST_USER`], or a duplicate id or name is
    /// [`ErrorKind::InvalidData`].
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Database> {
        Self::open_with_frames(path, DEFAULT_POOL_PAGES)
    }

    /// Opens `path` with `frames` buffer-pool frames and loads the catalog.
    ///
    /// `frames` must be at least 1. The catalog rules match [`Self::open`].
    pub fn open_with_frames<P: AsRef<Path>>(path: P, frames: usize) -> io::Result<Database> {
        let mut records = RecordFile::open_pooled(path, frames)?;
        let header = records.pages_mut().read_page(PageId(0))?;
        let next_xid = read_u64(&header.data()[NEXT_XID_OFFSET..NEXT_XID_OFFSET + 8]);
        if next_xid == 0 {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "invalid next transaction id: 0",
            ));
        }
        let cleanup = header.data()[CLEANUP_FLAG_OFFSET];
        let tables = load_tables(&mut records)?;
        let default_session = SessionId(1);
        let mut database = Database {
            records,
            tables,
            header,
            next_xid,
            sessions: vec![Session {
                id: default_session,
                txn: None,
            }],
            default_session,
            exec_session: default_session,
            next_session_id: 2,
            planner_enabled: true,
        };
        if cleanup == 1 {
            database.crash_cleanup()?;
        }
        Ok(database)
    }

    /// Turns the rule-based planner on or off.
    ///
    /// The planner is on when a database is opened. Off, every binding is a
    /// sequential scan, joins are nested loops, `WHERE` is one filter above
    /// the joins, and `ORDER BY` is a sort. Index lookup is not used.
    /// Results match the planner-on plans.
    pub fn set_planner_enabled(&mut self, enabled: bool) {
        self.planner_enabled = enabled;
    }

    /// Whether [`Self::set_planner_enabled`] last turned the planner on.
    pub fn planner_enabled(&self) -> bool {
        self.planner_enabled
    }

    /// The session used by [`Self::execute`], [`Self::begin`], and the row methods.
    pub fn default_session(&self) -> SessionId {
        self.default_session
    }

    /// Starts a session with no transaction and returns its id.
    pub fn create_session(&mut self) -> SessionId {
        let id = SessionId(self.next_session_id);
        self.next_session_id = self.next_session_id.saturating_add(1);
        self.sessions.push(Session { id, txn: None });
        id
    }

    /// Rolls back an open transaction on `id`, then forgets the session.
    ///
    /// An unknown id is [`ErrorKind::NotFound`] (`unknown session: {id}`).
    pub fn close_session(&mut self, id: SessionId) -> io::Result<()> {
        let explicit = self.session_in_transaction(id)?;
        let has_txn = self.session(id)?.txn.is_some();
        if explicit {
            self.rollback_on(id)?;
        } else if has_txn {
            self.finish_rollback(id)?;
        }
        let index = self.session_index(id)?;
        self.sessions.remove(index);
        if self.exec_session == id {
            self.exec_session = self.default_session;
        }
        Ok(())
    }

    /// Whether `id` is inside [`Self::begin`].
    ///
    /// An implicit one-statement transaction does not count. An unknown id is
    /// [`ErrorKind::NotFound`].
    pub fn session_in_transaction(&self, id: SessionId) -> io::Result<bool> {
        Ok(self
            .session(id)?
            .txn
            .as_ref()
            .is_some_and(|txn| txn.explicit))
    }

    /// Next transaction id that will be assigned.
    ///
    /// Stored in the header on every flush. Every version written since the
    /// previous flush has a smaller id.
    pub fn next_transaction_id(&self) -> u64 {
        self.next_xid
    }

    /// Creates a user table and, outside a transaction, flushes its catalog record.
    ///
    /// The assigned id is one greater than the highest user-table id already
    /// in the catalog, or [`TableId::FIRST_USER`] when there is none. `name`
    /// and each column name must be identifiers. Comparison ignores ASCII
    /// case; the stored spelling is the one passed in.
    ///
    /// An empty column list, a bad identifier, a duplicate table or column
    /// name, more than one primary key, a non-`INTEGER` primary key, a
    /// definition longer than [`MAX_RECORD_SIZE`], or an id that would not
    /// fit in `u16` is [`ErrorKind::InvalidInput`].
    ///
    /// A primary key allocates an empty B+Tree leaf before the catalog record
    /// is written. Tables without a primary key have no index.
    ///
    /// Outside a transaction the buffer pool is flushed before this returns.
    /// A failed call rolls the statement back to its savepoint. If the
    /// autocommit flush fails before the WAL sync, that savepoint is restored.
    /// Pages that a synced commit already wrote can remain on disk.
    pub fn create_table(
        &mut self,
        name: &str,
        columns: &[ColumnSpec<'_>],
    ) -> io::Result<&TableSchema> {
        self.in_statement(|db| db.create_table_unflushed(name, columns))?;
        Ok(&self.tables[self.tables.len() - 1])
    }

    /// Creates a user table without flushing.
    ///
    /// [`Self::execute_statement`] and [`Self::create_table`] flush once.
    pub(crate) fn create_table_unflushed(
        &mut self,
        name: &str,
        columns: &[ColumnSpec<'_>],
    ) -> io::Result<()> {
        check_identifier(name)?;
        if columns.is_empty() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "table needs at least one column",
            ));
        }
        let mut defined = Vec::with_capacity(columns.len());
        let mut primary_key = None;
        for column in columns {
            check_identifier(column.name)?;
            if defined
                .iter()
                .any(|existing: &Column| names_eq(&existing.name, column.name))
            {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    format!("duplicate column: {}", column.name),
                ));
            }
            if column.primary_key {
                if primary_key.is_some() {
                    return Err(io::Error::new(
                        ErrorKind::InvalidInput,
                        format!("multiple primary keys for table {name}"),
                    ));
                }
                if column.column_type != ColumnType::Integer {
                    return Err(io::Error::new(
                        ErrorKind::InvalidInput,
                        format!("primary key must be INTEGER: {}", column.name),
                    ));
                }
                primary_key = Some(defined.len());
            }
            defined.push(Column {
                name: column.name.to_string(),
                column_type: column.column_type,
            });
        }
        if self.tables.iter().any(|table| names_eq(&table.name, name)) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("table already exists: {name}"),
            ));
        }
        if self.other_transactions_active() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "CREATE TABLE requires no other active transactions",
            ));
        }
        let id = next_table_id(&self.tables)?;
        let index_root = if primary_key.is_some() {
            Some(BTree::create(self.records.pages_mut(), id)?)
        } else {
            None
        };
        let schema = TableSchema {
            id,
            name: name.to_string(),
            columns: defined,
            primary_key,
            index_root,
        };
        let bytes = encode_schema(&schema)?;
        let catalog_rid = self.records.insert(TableId::CATALOG, &bytes)?;
        self.tables.push(schema);
        self.record_created_table(CreatedTable { id, catalog_rid })?;
        Ok(())
    }

    /// Logical page reads since this database was opened.
    ///
    /// Each buffer-pool read counts once, hit or miss, so index and limit
    /// checks do not depend on which pages are cached. The counter is not
    /// stored in the file.
    pub fn pages_read(&self) -> u64 {
        self.records.pages_read()
    }

    /// Buffer-pool counters since this database was opened.
    pub fn buffer_stats(&self) -> BufferStats {
        self.records.buffer_stats()
    }

    /// Starts an explicit transaction on the default session.
    ///
    /// The snapshot is taken here. A second call while one is open is
    /// [`ErrorKind::InvalidInput`] (`transaction already in progress`).
    pub fn begin(&mut self) -> io::Result<()> {
        self.begin_on(self.default_session)
    }

    /// Sets commit flags on the default session's versions and flushes.
    ///
    /// No open transaction is [`ErrorKind::InvalidInput`]
    /// (`no transaction in progress`). The flush commits through the WAL. If
    /// it fails before that log is synced, the transaction stays open and the
    /// dirty pages remain. If it fails after the sync, the commit is durable
    /// in the WAL and the frames are clean. Retry [`Self::commit`] or call
    /// [`Self::rollback`]. Rollback does not erase a commit already synced to
    /// the WAL; the next open replays it.
    pub fn commit(&mut self) -> io::Result<()> {
        self.commit_on(self.default_session)
    }

    /// Undoes the default session's write set, flushes, and ends the transaction.
    ///
    /// Created versions are deleted, including their index entries. Deleted
    /// versions get `xmax` reset to 0. Tables created in the transaction lose
    /// their catalog record after the rows are undone; their pages stay.
    /// Another session's changes on the same pages are left in place.
    ///
    /// No open transaction is [`ErrorKind::InvalidInput`]
    /// (`no transaction in progress`).
    pub fn rollback(&mut self) -> io::Result<()> {
        self.rollback_on(self.default_session)
    }

    /// Whether [`Self::begin`] is in effect on the default session.
    pub fn in_transaction(&self) -> bool {
        self.session_in_transaction(self.default_session)
            .unwrap_or(false)
    }

    /// Frames held by the buffer pool, including overflow past its capacity.
    #[cfg(test)]
    pub(crate) fn frame_count(&self) -> usize {
        self.records.frame_count()
    }

    /// Runs `body` as one atomic statement on the executing session.
    ///
    /// A session with no transaction starts an implicit one for this call and
    /// commits it on success. On failure the savepoint is restored, the write
    /// set and created-table list are truncated to their lengths at the start,
    /// and the catalog is reloaded. An explicit transaction stays open.
    ///
    /// If the autocommit flush fails before the WAL sync, the savepoint is
    /// restored and the implicit transaction ends. If it fails after that
    /// sync, the commit is already in the log. The next open replays it.
    /// Dirty frames are not discarded: another session may share them.
    pub(crate) fn in_statement<T>(
        &mut self,
        body: impl FnOnce(&mut Self) -> io::Result<T>,
    ) -> io::Result<T> {
        let session = self.exec_session;
        let started_here = self.session(session)?.txn.is_none();
        if started_here {
            self.begin_transaction(session, false)?;
        }
        let (write_len, created_len) = {
            let txn = self.session(session)?.txn.as_ref().expect("transaction");
            (txn.write_set.len(), txn.created_tables.len())
        };
        self.records.set_savepoint()?;
        let result = body(self);
        self.finish_statement(session, started_here, write_len, created_len, result)
    }

    fn finish_statement<T>(
        &mut self,
        session: SessionId,
        started_here: bool,
        write_len: usize,
        created_len: usize,
        result: io::Result<T>,
    ) -> io::Result<T> {
        match result {
            Ok(value) => {
                if !started_here {
                    self.records.release_savepoint()?;
                    return Ok(value);
                }
                match self.prepare_and_flush(session) {
                    Ok(()) => {
                        self.clear_txn(session)?;
                        self.records.release_savepoint()?;
                        Ok(value)
                    }
                    Err(flush_err) => {
                        if self.records.wal_commit_pending() {
                            let _ = self.clear_txn(session);
                            let _ = self.records.release_savepoint();
                        } else {
                            let _ = self.records.rollback_to_savepoint();
                            let _ = self.records.release_savepoint();
                            let _ = self.clear_txn(session);
                            let _ = self.reload_catalog();
                        }
                        Err(flush_err)
                    }
                }
            }
            Err(original) => {
                let _ = self.records.rollback_to_savepoint();
                let _ = self.records.release_savepoint();
                if started_here {
                    let _ = self.clear_txn(session);
                } else {
                    let _ = self.truncate_txn(session, write_len, created_len);
                }
                let _ = self.reload_catalog();
                Err(original)
            }
        }
    }

    fn reload_catalog(&mut self) -> io::Result<()> {
        self.tables = load_tables(&mut self.records)?;
        Ok(())
    }

    pub(crate) fn begin_on(&mut self, id: SessionId) -> io::Result<()> {
        if self.session(id)?.txn.is_some() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "transaction already in progress",
            ));
        }
        self.begin_transaction(id, true)
    }

    pub(crate) fn commit_on(&mut self, id: SessionId) -> io::Result<()> {
        self.ensure_explicit(id)?;
        self.prepare_and_flush(id)?;
        self.clear_txn(id)?;
        Ok(())
    }

    pub(crate) fn rollback_on(&mut self, id: SessionId) -> io::Result<()> {
        self.ensure_explicit(id)?;
        self.finish_rollback(id)
    }

    fn finish_rollback(&mut self, id: SessionId) -> io::Result<()> {
        if self.records.wal_commit_pending() {
            self.clear_txn(id)?;
            return Ok(());
        }
        self.undo_transaction(id)?;
        self.clear_txn(id)?;
        self.persist_header(self.cleanup_flag())?;
        self.records.flush()?;
        Ok(())
    }

    fn begin_transaction(&mut self, id: SessionId, explicit: bool) -> io::Result<()> {
        let xid = self.next_xid;
        self.next_xid = self.next_xid.checked_add(1).ok_or_else(|| {
            io::Error::new(ErrorKind::InvalidData, "transaction id space exhausted")
        })?;
        let bound = self.next_xid;
        let active = self.active_xids();
        let index = self.session_index(id)?;
        self.sessions[index].txn = Some(Transaction {
            xid,
            snapshot: Snapshot { xid, bound, active },
            write_set: Vec::new(),
            explicit,
            created_tables: Vec::new(),
            physical_writes: false,
        });
        Ok(())
    }

    pub(crate) fn mark_physical_writes(&mut self) -> io::Result<()> {
        let index = self.session_index(self.exec_session)?;
        if let Some(txn) = self.sessions[index].txn.as_mut() {
            txn.physical_writes = true;
        }
        Ok(())
    }

    /// Commit flags, then the header, then the WAL flush.
    ///
    /// The header cleanup flag is 1 when some other session still has a
    /// non-empty write set. A flush that already synced its log returns
    /// without writing again.
    fn prepare_and_flush(&mut self, id: SessionId) -> io::Result<()> {
        if self.records.wal_commit_pending() {
            return Ok(());
        }
        // A read-only transaction's xid never reaches disk, so there is
        // nothing to commit and the header need not advance.
        let index = self.session_index(id)?;
        let read_only = self.sessions[index].txn.as_ref().is_some_and(|txn| {
            txn.write_set.is_empty() && txn.created_tables.is_empty() && !txn.physical_writes
        });
        if read_only {
            return Ok(());
        }
        self.apply_commit_flags(id)?;
        self.persist_header(self.cleanup_flag_except(id))?;
        self.records.flush()
    }

    fn apply_commit_flags(&mut self, id: SessionId) -> io::Result<()> {
        let index = self.session_index(id)?;
        let writes = self.sessions[index]
            .txn
            .as_ref()
            .ok_or_else(no_transaction)?
            .write_set
            .clone();
        for entry in writes {
            let mut bytes = self.records.get(entry.table, entry.rid)?;
            match entry.kind {
                WriteKind::Inserted => crate::mvcc::set_xmin_committed(&mut bytes)?,
                WriteKind::Deleted => crate::mvcc::set_xmax_committed(&mut bytes)?,
            }
            self.records.update(entry.table, entry.rid, &bytes)?;
        }
        Ok(())
    }

    fn undo_transaction(&mut self, id: SessionId) -> io::Result<()> {
        let index = self.session_index(id)?;
        let (writes, created) = {
            let txn = self.sessions[index]
                .txn
                .as_mut()
                .ok_or_else(no_transaction)?;
            (
                std::mem::take(&mut txn.write_set),
                std::mem::take(&mut txn.created_tables),
            )
        };
        for entry in writes.into_iter().rev() {
            match entry.kind {
                WriteKind::Inserted => self.undo_inserted(&entry)?,
                WriteKind::Deleted => self.undo_deleted(&entry)?,
            }
        }
        for created in created.into_iter().rev() {
            self.records.delete(TableId::CATALOG, created.catalog_rid)?;
            self.tables.retain(|table| table.id != created.id);
        }
        Ok(())
    }

    fn crash_cleanup(&mut self) -> io::Result<()> {
        self.purge_aborted_versions()?;
        self.persist_header(0)?;
        self.records.flush()
    }

    fn persist_header(&mut self, cleanup: u8) -> io::Result<()> {
        self.header.data_mut()[NEXT_XID_OFFSET..NEXT_XID_OFFSET + 8]
            .copy_from_slice(&self.next_xid.to_le_bytes());
        self.header.data_mut()[CLEANUP_FLAG_OFFSET] = cleanup;
        let mut page = Page::zeroed();
        page.data_mut().copy_from_slice(self.header.data());
        self.records.pages_mut().write_page(PageId(0), &page)
    }

    fn cleanup_flag(&self) -> u8 {
        u8::from(self.sessions.iter().any(|session| {
            session
                .txn
                .as_ref()
                .is_some_and(|txn| !txn.write_set.is_empty())
        }))
    }

    fn cleanup_flag_except(&self, id: SessionId) -> u8 {
        u8::from(self.sessions.iter().any(|session| {
            session.id != id
                && session
                    .txn
                    .as_ref()
                    .is_some_and(|txn| !txn.write_set.is_empty())
        }))
    }

    fn clear_txn(&mut self, id: SessionId) -> io::Result<()> {
        let index = self.session_index(id)?;
        self.sessions[index].txn = None;
        Ok(())
    }

    fn truncate_txn(
        &mut self,
        id: SessionId,
        write_len: usize,
        created_len: usize,
    ) -> io::Result<()> {
        let index = self.session_index(id)?;
        if let Some(txn) = self.sessions[index].txn.as_mut() {
            txn.write_set.truncate(write_len);
            txn.created_tables.truncate(created_len);
        }
        Ok(())
    }

    fn ensure_explicit(&self, id: SessionId) -> io::Result<()> {
        match self.session(id)?.txn.as_ref() {
            Some(txn) if txn.explicit => Ok(()),
            _ => Err(no_transaction()),
        }
    }

    fn session(&self, id: SessionId) -> io::Result<&Session> {
        let index = self.session_index(id)?;
        Ok(&self.sessions[index])
    }

    fn session_index(&self, id: SessionId) -> io::Result<usize> {
        self.sessions
            .iter()
            .position(|session| session.id == id)
            .ok_or_else(|| {
                io::Error::new(ErrorKind::NotFound, format!("unknown session: {}", id.0))
            })
    }

    fn active_xids(&self) -> BTreeSet<u64> {
        self.sessions
            .iter()
            .filter_map(|session| session.txn.as_ref().map(|txn| txn.xid))
            .collect()
    }

    pub(crate) fn active_xid_set(&self) -> BTreeSet<u64> {
        self.active_xids()
    }

    pub(crate) fn active_snapshots(&self) -> Vec<Snapshot> {
        self.sessions
            .iter()
            .filter_map(|session| session.txn.as_ref().map(|txn| txn.snapshot.clone()))
            .collect()
    }

    pub(crate) fn other_transactions_active(&self) -> bool {
        self.sessions
            .iter()
            .any(|session| session.id != self.exec_session && session.txn.is_some())
    }

    pub(crate) fn explicit_transaction(&self) -> bool {
        self.sessions
            .iter()
            .find(|session| session.id == self.exec_session)
            .and_then(|session| session.txn.as_ref())
            .is_some_and(|txn| txn.explicit)
    }

    pub(crate) fn current_snapshot(&self) -> io::Result<Snapshot> {
        if let Some(txn) = self.session(self.exec_session)?.txn.as_ref() {
            return Ok(txn.snapshot.clone());
        }
        // Operators can be pulled outside a statement. They see every
        // committed version and nothing this process has left uncommitted.
        Ok(Snapshot {
            xid: 0,
            bound: self.next_xid,
            active: BTreeSet::new(),
        })
    }

    pub(crate) fn current_xid(&self) -> io::Result<u64> {
        self.session(self.exec_session)?
            .txn
            .as_ref()
            .map(|txn| txn.xid)
            .ok_or_else(no_transaction)
    }

    pub(crate) fn record_write(&mut self, entry: WriteEntry) -> io::Result<()> {
        let index = self.session_index(self.exec_session)?;
        self.sessions[index]
            .txn
            .as_mut()
            .ok_or_else(no_transaction)?
            .write_set
            .push(entry);
        Ok(())
    }

    fn record_created_table(&mut self, created: CreatedTable) -> io::Result<()> {
        let index = self.session_index(self.exec_session)?;
        self.sessions[index]
            .txn
            .as_mut()
            .ok_or_else(no_transaction)?
            .created_tables
            .push(created);
        Ok(())
    }

    pub(crate) fn set_exec_session(&mut self, id: SessionId) -> io::Result<()> {
        self.session_index(id)?;
        self.exec_session = id;
        Ok(())
    }

    pub(crate) fn exec_session(&self) -> SessionId {
        self.exec_session
    }

    /// Returns the schema named `name`, ignoring ASCII case.
    pub fn table(&self, name: &str) -> Option<&TableSchema> {
        self.tables.iter().find(|table| names_eq(&table.name, name))
    }

    /// Schemas in table-id order.
    ///
    /// [`Self::create_table`] assigns increasing ids, so this is also creation
    /// order when every table was created through that method.
    pub fn tables(&self) -> &[TableSchema] {
        &self.tables
    }
}

fn no_transaction() -> io::Error {
    io::Error::new(ErrorKind::InvalidInput, "no transaction in progress")
}

fn read_u64(bytes: &[u8]) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(buf)
}

fn load_tables(records: &mut RecordFile) -> io::Result<Vec<TableSchema>> {
    let mut tables: Vec<TableSchema> = Vec::new();
    for (rid, bytes) in records.scan(TableId::CATALOG)? {
        let schema = decode_catalog_record(rid, &bytes)?;
        if tables.iter().any(|table| table.id == schema.id) {
            return Err(duplicate_table_id(schema.id));
        }
        if tables
            .iter()
            .any(|table| names_eq(&table.name, &schema.name))
        {
            return Err(duplicate_table_name(&schema.name));
        }
        tables.push(schema);
    }
    tables.sort_by_key(|table| table.id);
    Ok(tables)
}

fn encode_schema(schema: &TableSchema) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    push_u16(&mut bytes, schema.id.0);
    push_str(&mut bytes, &schema.name);
    let count =
        u16::try_from(schema.columns.len()).map_err(|_| definition_too_large(&schema.name))?;
    push_u16(&mut bytes, count);
    for column in &schema.columns {
        push_str(&mut bytes, &column.name);
        bytes.push(column.column_type.code());
    }
    let pk_index = match schema.primary_key {
        Some(index) => u16::try_from(index).map_err(|_| definition_too_large(&schema.name))?,
        None => 0xFFFF,
    };
    push_u16(&mut bytes, pk_index);
    let root = schema.index_root.map(|page| page.0).unwrap_or(0);
    bytes.extend_from_slice(&root.to_le_bytes());
    if bytes.len() > MAX_RECORD_SIZE {
        return Err(definition_too_large(&schema.name));
    }
    Ok(bytes)
}

fn decode_catalog_record(rid: RecordId, bytes: &[u8]) -> io::Result<TableSchema> {
    decode_parts(bytes).ok_or_else(|| invalid_catalog_record(rid))
}

fn decode_parts(bytes: &[u8]) -> Option<TableSchema> {
    let mut reader = Reader::new(bytes);
    let table_id = reader.u16()?;
    if table_id < TableId::FIRST_USER.0 {
        return None;
    }
    let name = reader.string()?;
    let count = reader.u16()? as usize;
    let mut columns = Vec::new();
    for _ in 0..count {
        let column_name = reader.string()?;
        let column_type = ColumnType::from_code(reader.u8()?)?;
        columns.push(Column {
            name: column_name,
            column_type,
        });
    }
    let pk_raw = reader.u16()?;
    let root_raw = reader.u32()?;
    if !reader.is_empty() {
        return None;
    }
    let primary_key = if pk_raw == 0xFFFF {
        None
    } else {
        let index = pk_raw as usize;
        if index >= columns.len() || columns[index].column_type != ColumnType::Integer {
            return None;
        }
        Some(index)
    };
    let index_root = if root_raw == 0 {
        None
    } else {
        Some(PageId(root_raw))
    };
    match (primary_key, index_root) {
        (None, None) | (Some(_), Some(_)) => {}
        _ => return None,
    }
    Some(TableSchema {
        id: TableId(table_id),
        name,
        columns,
        primary_key,
        index_root,
    })
}

struct Reader<'a> {
    bytes: &'a [u8],
    index: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { bytes, index: 0 }
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.index.checked_add(len)?;
        if end > self.bytes.len() {
            return None;
        }
        let slice = &self.bytes[self.index..end];
        self.index = end;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|bytes| bytes[0])
    }

    fn u16(&mut self) -> Option<u16> {
        self.take(2)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Option<u32> {
        self.take(4)
            .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn string(&mut self) -> Option<String> {
        let len = self.u16()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).ok()
    }

    fn is_empty(&self) -> bool {
        self.index == self.bytes.len()
    }
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_str(bytes: &mut Vec<u8>, value: &str) {
    let len = u16::try_from(value.len()).expect("name length fits in u16");
    push_u16(bytes, len);
    bytes.extend_from_slice(value.as_bytes());
}

fn next_table_id(tables: &[TableSchema]) -> io::Result<TableId> {
    let max = tables.iter().map(|table| table.id.0).max().unwrap_or(1);
    match max.checked_add(1) {
        Some(id) => Ok(TableId(id)),
        None => Err(io::Error::new(ErrorKind::InvalidInput, "too many tables")),
    }
}

fn check_identifier(name: &str) -> io::Result<()> {
    if is_identifier(name) {
        Ok(())
    } else {
        Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("invalid identifier: {name}"),
        ))
    }
}

fn is_identifier(name: &str) -> bool {
    let bytes = name.as_bytes();
    let Some(first) = bytes.first() else {
        return false;
    };
    if !first.is_ascii_alphabetic() && *first != b'_' {
        return false;
    }
    bytes.len() <= 64
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

fn names_eq(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

fn invalid_catalog_record(rid: RecordId) -> io::Error {
    io::Error::new(
        ErrorKind::InvalidData,
        format!("invalid catalog record: {rid}"),
    )
}

fn duplicate_table_id(id: TableId) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, format!("duplicate table id: {id}"))
}

fn duplicate_table_name(name: &str) -> io::Error {
    io::Error::new(
        ErrorKind::InvalidData,
        format!("duplicate table name: {name}"),
    )
}

fn definition_too_large(name: &str) -> io::Error {
    io::Error::new(
        ErrorKind::InvalidInput,
        format!("table definition too large: {name}"),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        decode_catalog_record, encode_schema, Column, ColumnSpec, ColumnType, Database, TableSchema,
    };
    use crate::page::PageId;
    use crate::record::{RecordFile, RecordId, TableId};
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

    fn sample_rid() -> RecordId {
        RecordId {
            page_id: PageId(1),
            slot_id: 4,
        }
    }

    fn users_schema() -> TableSchema {
        TableSchema {
            id: TableId(2),
            name: "users".to_string(),
            columns: vec![
                Column {
                    name: "id".to_string(),
                    column_type: ColumnType::Integer,
                },
                Column {
                    name: "name".to_string(),
                    column_type: ColumnType::Text,
                },
            ],
            primary_key: None,
            index_root: None,
        }
    }

    #[test]
    fn column_type_display_and_parse() {
        assert_eq!(ColumnType::Integer.to_string(), "INTEGER");
        assert_eq!(ColumnType::Text.to_string(), "TEXT");
        assert_eq!(
            "integer".parse::<ColumnType>().unwrap(),
            ColumnType::Integer
        );
        assert_eq!("TEXT".parse::<ColumnType>().unwrap(), ColumnType::Text);
        assert_eq!("Text".parse::<ColumnType>().unwrap(), ColumnType::Text);
        let err = "blob".parse::<ColumnType>().unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "unknown column type: blob");
    }

    #[test]
    fn catalog_record_layout_is_little_endian() {
        let schema = users_schema();
        let bytes = encode_schema(&schema).unwrap();
        let expected = [
            2, 0, 5, 0, b'u', b's', b'e', b'r', b's', 2, 0, 2, 0, b'i', b'd', 1, 4, 0, b'n', b'a',
            b'm', b'e', 2, 0xff, 0xff, 0, 0, 0, 0,
        ];
        assert_eq!(bytes, expected);
        assert_eq!(decode_catalog_record(sample_rid(), &bytes).unwrap(), schema);
    }

    #[test]
    fn catalog_record_rejects_bad_bytes() {
        let rid = sample_rid();
        let valid = encode_schema(&users_schema()).unwrap();
        let mut cases: Vec<Vec<u8>> = vec![
            Vec::new(),
            vec![2, 0],
            vec![2, 0, 5, 0, b'u'],
            valid.clone(),
        ];
        cases[3].push(0);
        let mut bad_id_zero = valid.clone();
        bad_id_zero[0] = 0;
        bad_id_zero[1] = 0;
        cases.push(bad_id_zero);
        let mut bad_id_one = valid.clone();
        bad_id_one[0] = 1;
        cases.push(bad_id_one);
        cases.push(vec![2, 0, 1, 0, 0xff, 0, 0]);
        let mut bad_type = valid.clone();
        // The last column's type byte is immediately before the 6-byte key suffix.
        let type_at = bad_type.len() - 7;
        bad_type[type_at] = 9;
        cases.push(bad_type);
        let mut bad_column_utf8 = valid;
        // Table-name length and bytes become a one-byte invalid UTF-8 sequence.
        bad_column_utf8.splice(2..9, [1, 0, 0xff]);
        cases.push(bad_column_utf8);

        for bytes in cases {
            let err = decode_catalog_record(rid, &bytes).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidData);
            assert_eq!(err.to_string(), "invalid catalog record: 1:4");
        }
    }

    #[test]
    fn create_then_reopen_restores_schemas() {
        let db = TempDb::new("catalog");
        let created;
        {
            let mut database = Database::open(db.path()).unwrap();
            let users_id = database
                .create_table(
                    "users",
                    &[
                        ColumnSpec::new("id", ColumnType::Integer),
                        ColumnSpec::new("name", ColumnType::Text),
                    ],
                )
                .unwrap()
                .id;
            let posts_id = database
                .create_table(
                    "posts",
                    &[
                        ColumnSpec::new("id", ColumnType::Integer),
                        ColumnSpec::new("title", ColumnType::Text),
                    ],
                )
                .unwrap()
                .id;
            assert_eq!(users_id, TableId(2));
            assert_eq!(posts_id, TableId(3));
            assert_eq!(database.table("USERS").unwrap().name, "users");
            assert_eq!(database.table("users").unwrap().columns[1].name, "name");
            assert_eq!(
                database.table("users").unwrap().columns[1].column_type,
                ColumnType::Text
            );
            assert!(database.table("user").is_none());
            let err = database
                .create_table("Users", &[ColumnSpec::new("id", ColumnType::Integer)])
                .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidInput);
            assert_eq!(err.to_string(), "table already exists: Users");
            assert_eq!(database.tables().len(), 2);
            created = database.tables().to_vec();
        }

        let database = Database::open(db.path()).unwrap();
        assert_eq!(database.tables(), created.as_slice());
        assert_eq!(database.table("Posts").unwrap().id, TableId(3));
    }

    #[test]
    fn invalid_identifiers_and_columns() {
        let db = TempDb::new("idents");
        let mut database = Database::open(db.path()).unwrap();
        let too_long = "a".repeat(65);
        for name in ["", "1abc", "a-b", too_long.as_str(), "has space"] {
            let err = database
                .create_table(name, &[ColumnSpec::new("id", ColumnType::Integer)])
                .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidInput);
            assert_eq!(err.to_string(), format!("invalid identifier: {name}"));
        }
        let err = database
            .create_table("users", &[ColumnSpec::new("1id", ColumnType::Integer)])
            .unwrap_err();
        assert_eq!(err.to_string(), "invalid identifier: 1id");
        let err = database.create_table("users", &[]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "table needs at least one column");
        let err = database
            .create_table(
                "users",
                &[
                    ColumnSpec::new("id", ColumnType::Integer),
                    ColumnSpec::new("ID", ColumnType::Text),
                ],
            )
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "duplicate column: ID");
        assert!(database.tables().is_empty());

        let long = "a".repeat(64);
        let id = database
            .create_table(&long, &[ColumnSpec::new("_id", ColumnType::Integer)])
            .unwrap()
            .id;
        assert_eq!(id, TableId::FIRST_USER);
        assert_eq!(database.table(&long).unwrap().columns[0].name, "_id");
        database
            .create_table("_t", &[ColumnSpec::new("_c", ColumnType::Text)])
            .unwrap();
        assert_eq!(
            database.table("_T").unwrap().columns[0].column_type,
            ColumnType::Text
        );
    }

    #[test]
    fn table_definition_too_large() {
        let db = TempDb::new("toolarge");
        let mut database = Database::open(db.path()).unwrap();
        let wide: Vec<(String, ColumnType)> = (0..61)
            .map(|index| (format!("c{index:063}"), ColumnType::Text))
            .collect();
        let columns: Vec<ColumnSpec> = wide
            .iter()
            .map(|(name, column_type)| ColumnSpec::new(name, *column_type))
            .collect();
        let err = database.create_table("t", &columns).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "table definition too large: t");
        assert!(database.tables().is_empty());

        let fitting: Vec<(String, ColumnType)> = (0..60)
            .map(|index| (format!("c{index:063}"), ColumnType::Text))
            .collect();
        let columns: Vec<ColumnSpec> = fitting
            .iter()
            .map(|(name, column_type)| ColumnSpec::new(name, *column_type))
            .collect();
        assert_eq!(database.create_table("t", &columns).unwrap().id, TableId(2));
    }

    #[test]
    fn corrupt_catalog_record_fails_open() {
        let db = TempDb::new("corrupt");
        {
            let mut records = RecordFile::open(db.path()).unwrap();
            records.insert(TableId::CATALOG, b"bogus").unwrap();
        }
        let err = error_of(Database::open(db.path()));
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "invalid catalog record: 1:0");
    }

    #[test]
    fn duplicate_catalog_entries_fail_open() {
        let db = TempDb::new("dupcat");
        {
            let mut records = RecordFile::open(db.path()).unwrap();
            let users = users_schema();
            let mut same_name = users_schema();
            same_name.id = TableId(3);
            same_name.name = "Users".to_string();
            records
                .insert(TableId::CATALOG, &encode_schema(&users).unwrap())
                .unwrap();
            records
                .insert(TableId::CATALOG, &encode_schema(&same_name).unwrap())
                .unwrap();
        }
        let err = error_of(Database::open(db.path()));
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "duplicate table name: Users");

        let db = TempDb::new("dupid");
        {
            let mut records = RecordFile::open(db.path()).unwrap();
            let users = users_schema();
            let mut same_id = users_schema();
            same_id.name = "posts".to_string();
            records
                .insert(TableId::CATALOG, &encode_schema(&users).unwrap())
                .unwrap();
            records
                .insert(TableId::CATALOG, &encode_schema(&same_id).unwrap())
                .unwrap();
        }
        let err = error_of(Database::open(db.path()));
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "duplicate table id: 2");
    }

    #[test]
    fn user_table_records_do_not_appear_in_the_catalog() {
        let db = TempDb::new("usersonly");
        {
            let mut records = RecordFile::open(db.path()).unwrap();
            records.insert(TableId(2), b"not a schema").unwrap();
        }
        {
            let mut database = Database::open(db.path()).unwrap();
            assert!(database.tables().is_empty());
            database
                .create_table("users", &[ColumnSpec::new("id", ColumnType::Integer)])
                .unwrap();
        }
        let database = Database::open(db.path()).unwrap();
        assert_eq!(database.tables().len(), 1);
        assert_eq!(database.table("users").unwrap().id, TableId(2));
        assert_eq!(database.table("users").unwrap().columns.len(), 1);
        drop(database);

        let mut records = RecordFile::open(db.path()).unwrap();
        let rows = records.scan(TableId(2)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, b"not a schema");
        assert_eq!(records.scan(TableId::CATALOG).unwrap().len(), 1);
    }

    #[test]
    fn open_sorts_catalog_records_by_table_id() {
        let db = TempDb::new("order");
        {
            let mut records = RecordFile::open(db.path()).unwrap();
            let mut posts = users_schema();
            posts.id = TableId(4);
            posts.name = "posts".to_string();
            let users = users_schema();
            records
                .insert(TableId::CATALOG, &encode_schema(&posts).unwrap())
                .unwrap();
            records
                .insert(TableId::CATALOG, &encode_schema(&users).unwrap())
                .unwrap();
        }
        let mut database = Database::open(db.path()).unwrap();
        assert_eq!(database.tables()[0].id, TableId(2));
        assert_eq!(database.tables()[0].name, "users");
        assert_eq!(database.tables()[1].id, TableId(4));
        let next = database
            .create_table("comments", &[ColumnSpec::new("id", ColumnType::Integer)])
            .unwrap()
            .id;
        assert_eq!(next, TableId(5));
    }

    #[test]
    fn too_many_tables() {
        let db = TempDb::new("manytables");
        {
            let mut records = RecordFile::open(db.path()).unwrap();
            let mut full = users_schema();
            full.id = TableId(u16::MAX);
            full.name = "full".to_string();
            records
                .insert(TableId::CATALOG, &encode_schema(&full).unwrap())
                .unwrap();
        }
        let mut database = Database::open(db.path()).unwrap();
        let err = database
            .create_table("another", &[ColumnSpec::new("id", ColumnType::Integer)])
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "too many tables");
        assert_eq!(database.tables().len(), 1);
    }

    #[test]
    fn primary_key_is_stored_and_restored() {
        let db = TempDb::new("pk");
        let root;
        {
            let mut database = Database::open(db.path()).unwrap();
            let schema = database
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
            assert_eq!(schema.primary_key, Some(0));
            root = schema.index_root.unwrap();
            assert_ne!(root, PageId(0));
            database
                .create_table("posts", &[ColumnSpec::new("id", ColumnType::Integer)])
                .unwrap();
            assert!(database.table("posts").unwrap().primary_key.is_none());
            assert!(database.table("posts").unwrap().index_root.is_none());
        }
        let mut database = Database::open(db.path()).unwrap();
        let users = database.table("users").unwrap();
        assert_eq!(users.primary_key, Some(0));
        assert_eq!(users.index_root, Some(root));
        let page = database.records.pages_mut().read_page(root).unwrap();
        assert_eq!(page.data()[0], 2);
        assert_eq!(page.data()[1], 1);
        assert_eq!(&page.data()[4..6], &2u16.to_le_bytes());

        let err = database
            .create_table(
                "bad",
                &[
                    ColumnSpec {
                        name: "id",
                        column_type: ColumnType::Integer,
                        primary_key: true,
                    },
                    ColumnSpec {
                        name: "other",
                        column_type: ColumnType::Integer,
                        primary_key: true,
                    },
                ],
            )
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "multiple primary keys for table bad");
        let err = database
            .create_table(
                "names",
                &[ColumnSpec {
                    name: "name",
                    column_type: ColumnType::Text,
                    primary_key: true,
                }],
            )
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "primary key must be INTEGER: name");
        assert!(database.table("bad").is_none());
        assert!(database.table("names").is_none());
    }

    #[test]
    fn catalog_record_rejects_a_bad_primary_key() {
        let rid = sample_rid();
        let mut with_pk = users_schema();
        with_pk.primary_key = Some(0);
        with_pk.index_root = Some(PageId(7));
        let bytes = encode_schema(&with_pk).unwrap();
        assert_eq!(decode_catalog_record(rid, &bytes).unwrap(), with_pk);

        let mut text_pk = with_pk.clone();
        text_pk.primary_key = Some(1);
        let err = decode_catalog_record(rid, &encode_schema(&text_pk).unwrap()).unwrap_err();
        assert_eq!(err.to_string(), "invalid catalog record: 1:4");

        let mut missing_root = with_pk.clone();
        missing_root.index_root = None;
        let err = decode_catalog_record(rid, &encode_schema(&missing_root).unwrap()).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);

        let mut root_without_pk = users_schema();
        root_without_pk.index_root = Some(PageId(3));
        let err =
            decode_catalog_record(rid, &encode_schema(&root_without_pk).unwrap()).unwrap_err();
        assert_eq!(err.to_string(), "invalid catalog record: 1:4");

        let mut out_of_range = with_pk.clone();
        out_of_range.primary_key = Some(5);
        let err = decode_catalog_record(rid, &encode_schema(&out_of_range).unwrap()).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
    }

    fn error_of<T>(result: std::io::Result<T>) -> std::io::Error {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(err) => err,
        }
    }
}
