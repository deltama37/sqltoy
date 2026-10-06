# sqltoy

sqltoy is a small learning RDBMS written in Rust. The point is to understand a
database by building its layers, not to match an existing engine. Design notes
are in `docs/adr/`:

- ADR-0001 chooses Rust and the initial scope.
- ADR-0002 fixes a bottom-up implementation order.
- ADR-0003 fixes the page size and the header page format.
- ADR-0004 fixes the slotted-page record format.
- ADR-0005 fixes table page ownership and the catalog record format.
- ADR-0006 fixes row encoding and table operations.
- ADR-0007 fixes the SQL lexer, grammar, and AST.
- ADR-0008 fixes SQL execution rules and the CLI.
- ADR-0009 fixes the primary key and the B+Tree index page format.
- ADR-0010 fixes operator-based query execution, including `ORDER BY`,
  `LIMIT` / `OFFSET`, and `JOIN`.
- ADR-0011 fixes the buffer pool: a fixed number of frames, LRU eviction,
  and a flush at the end of each statement.
- ADR-0012 fixes transactions: `BEGIN` / `COMMIT` / `ROLLBACK`, statement
  atomicity, and no-steal rollback.
- ADR-0013 fixes the write-ahead log and crash recovery.

## Status

The Storage layer (ADR-0002 step 1) reads and writes a local file by byte
offset. The Page Manager (ADR-0002 step 2, per ADR-0003) is implemented on
top of Storage: fixed 4096-byte pages, a reserved header page (page 0), and
allocate/read/write of pages with persistence. Record Storage (ADR-0002
step 3, per ADR-0004) stores variable-length records in slotted pages and
addresses them with stable record ids. Each record page belongs to one table.
The Catalog (ADR-0002 step 4, per ADR-0005) is done: table schemas are stored
in the database file and restored when the file is opened. The header format
version is 3. Table Operations (ADR-0002 step 5, per ADR-0006) are done:
typed rows, including NULL, can be inserted, scanned, updated, and deleted.
An update that no longer fits on its page is stored at a new record id. The
SQL parser (ADR-0002 step 6, per ADR-0007) is done: `CREATE TABLE`, `INSERT`,
`SELECT`, `UPDATE`, and `DELETE` parse into an AST, including `WHERE` and
expressions. The executor (ADR-0002 step 7, per ADR-0008) is done: those
statements run from the library and from `sqltoy sql` / `sqltoy repl`. This
is the first SQL milestone. The primary-key index (ADR-0002 step 8, per
ADR-0009) is done. An `INTEGER` column marked `PRIMARY KEY` is unique and
not NULL, and `WHERE id = 1` reads that one row from a B+Tree instead of
scanning the table. Query execution (ADR-0002 step 9, per ADR-0010) is
done. A `SELECT` is a tree of operators: a sequential scan reads one page
at a time, an index lookup replaces that scan when a primary key is a
constant equality, and `JOIN`, `ORDER BY`, and `LIMIT` / `OFFSET` are a
nested-loop join, a stable sort, and a limit. `LIMIT` stops the scan once
it has enough rows. A comma in `FROM` is a cross join. The buffer pool
(ADR-0002 step 10, per ADR-0011) is done. Record storage, the B+Tree, and
the catalog read and write pages through a fixed set of frames (256 by
default, at least 1). A read copies a frame out. A write copies into a
frame and marks it dirty, and does not read the file when the page is not
cached. Only a clean frame is evicted. When every frame is dirty the pool
grows past its configured size and shrinks back after commit or rollback.
A flush with no dirty pages does not touch the files. Transactions
(ADR-0002 step 11, per ADR-0012) are done. `BEGIN` keeps later changes in
dirty frames until `COMMIT` flushes them or `ROLLBACK` drops them. A
statement outside a transaction commits itself when it succeeds. A statement
that fails is undone, including inside an open transaction, which then
continues. `Database::insert`, `update`, `delete`, `insert_all`,
`apply_update`, and `create_table` are each one statement. Dropping a
database does not flush, so an open transaction is lost. The WAL and
recovery (ADR-0002 step 12, per ADR-0013) are done. A commit appends the
dirty page images and a commit record to `{db}-wal`, syncs that log, writes
the same pages into the database file, and truncates the log to its header.
Opening the file replays any commit the log still holds. A torn or corrupt
tail is ignored, and the log is truncated so the next commit is not appended
after it.

## Build and test

```bash
cargo build
cargo test
```

## SQL

`sql` runs statements against a database file and prints each result. This is
the first milestone: create a table, insert a row, and read it back.

```bash
cargo run --quiet -- sql /tmp/sqltoy-sql.db "CREATE TABLE users (id INTEGER, name TEXT); INSERT INTO users VALUES (1, 'Alice'); SELECT * FROM users"
```

```text
CREATE TABLE
INSERT 1
 id | name  
----+-------
  1 | Alice 
(1 row)
```

A primary key is one `INTEGER` column. A second insert of the same key fails,
and the earlier row stays:

```bash
cargo run --quiet -- sql /tmp/sqltoy-pk.db "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT); INSERT INTO users VALUES (1, 'Alice'); INSERT INTO users VALUES (1, 'Bob'); SELECT * FROM users WHERE id = 1"
```

```text
CREATE TABLE
INSERT 1
error: duplicate primary key: 1
```

`Database::pages_read` counts logical page reads since the database was
opened, hits and misses together, so the count does not depend on which
pages are cached. A primary-key equality reads a handful of pages (the tree
height, plus the row). A query without that equality reads every page.
`SELECT * FROM t LIMIT 1` on a table that spans dozens of pages reads one
or two pages and then stops. `repl` prints the pool counters with `.stats`.

A left join keeps unmatched left rows and fills the right side with `NULL`.
`ORDER BY` is stable, and `NULL` sorts first in `ASC` and last in `DESC`.
`LIMIT` is applied after the sort. `Cam` has no order, so the right-hand
`sku` would be `NULL`; `LIMIT 3` stops before that row:

```bash
cargo run --quiet -- sql /tmp/sqltoy-query.db "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT); CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, sku TEXT); INSERT INTO users VALUES (1, 'Bob'), (2, 'Ann'), (3, 'Cam'); INSERT INTO orders VALUES (10, 1, 'pen'), (11, 2, 'cup'), (12, 1, 'mug'); SELECT users.name, orders.sku FROM users LEFT JOIN orders ON users.id = orders.user_id ORDER BY users.name, orders.sku LIMIT 3"
```

```text
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
```

The next statement skips the leading `NULL` name, then keeps two rows. The
two rows named `a` stay in `id DESC` order:

```bash
cargo run --quiet -- sql /tmp/sqltoy-order.db "CREATE TABLE t (id INTEGER, name TEXT); INSERT INTO t VALUES (1, 'a'), (2, NULL), (3, 'b'), (4, 'a'); SELECT id, name FROM t ORDER BY name, id DESC LIMIT 2 OFFSET 1"
```

```text
CREATE TABLE
INSERT 4
 id | name 
----+------
  4 | a    
  1 | a    
(2 rows)
```

`repl` reads SQL from stdin. A statement runs when the buffer, ignoring
trailing whitespace, ends with `;`. `.quit` or `.exit` ends the session when
no statement is in progress. Piped input is not a terminal, so it prints no
prompt:

```bash
cargo run --quiet -- repl /tmp/sqltoy-repl.db <<'EOF'
CREATE TABLE users (
  id INTEGER,
  name TEXT
);
INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob');
SELECT * FROM users WHERE id = 1;
.quit
EOF
```

```text
CREATE TABLE
INSERT 2
 id | name  
----+-------
  1 | Alice 
(1 row)
```

## Buffer pool

`repl` prints buffer-pool counters with `.stats` when no statement is in
progress. The numbers are cumulative since the database was opened. In this
session the new pages were allocated in the pool, so every read was a hit
and the file was not read back. `CREATE TABLE` and `INSERT` wrote 4 pages
and synced at the end of each statement:

```bash
cargo run --quiet -- repl /tmp/sqltoy-buffer.db <<'EOF'
CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob');
SELECT * FROM users WHERE id = 1;
SELECT * FROM users WHERE id = 1;
.stats
.quit
EOF
```

```text
CREATE TABLE
INSERT 2
 id | name  
----+-------
  1 | Alice 
(1 row)
 id | name  
----+-------
  1 | Alice 
(1 row)
logical reads: 14, hits: 14, misses: 0, pages written: 4, evictions: 0
```

## Transactions

`BEGIN` starts a transaction. `COMMIT` writes its dirty pages and syncs.
`ROLLBACK` drops those pages, so the file is unchanged. A statement that
fails undoes only its own changes. The prompt is `sqltoy*> ` while a
transaction is open, when stdin is a terminal. At the end of input, an open
transaction is rolled back and the REPL prints a warning.

```bash
cargo run --quiet -- repl /tmp/sqltoy-txn.db <<'EOF'
CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
BEGIN;
INSERT INTO users VALUES (1, 'Alice');
SELECT * FROM users;
ROLLBACK;
SELECT * FROM users;
BEGIN;
INSERT INTO users VALUES (2, 'Bob');
COMMIT;
SELECT * FROM users;
.quit
EOF
```

```text
CREATE TABLE
BEGIN
INSERT 1
 id | name  
----+-------
  1 | Alice 
(1 row)
ROLLBACK
 id | name 
----+------
(0 rows)
BEGIN
INSERT 1
COMMIT
 id | name 
----+------
  2 | Bob  
(1 row)
```

Leaving `BEGIN` without `COMMIT` rolls the insert back. `sql` does the same.
Stdout is the statement results. Stderr is the warning:

```bash
cargo run --quiet -- sql /tmp/sqltoy-txn-open.db "CREATE TABLE t (id INTEGER); BEGIN; INSERT INTO t VALUES (1)"
```

```text
CREATE TABLE
BEGIN
INSERT 1
```

```text
warning: transaction rolled back
```

A later `SELECT * FROM t` is a new process and prints no rows:

```text
 id 
----
(0 rows)
```

## WAL and recovery

The database file `db` has a log at `db-wal` in the same directory. A commit
appends one record per dirty page (the page id and the full page image) and
then a commit record (the logical page count and how many page records belong
to this commit). Each record ends with an IEEE CRC-32. The log is synced
after the commit record. That sync is the commit. The same pages are then
written to the database file, the database file is synced, and the log is
truncated to its 16-byte header.

Opening the database reads the log before any query runs. Complete commits
are written back in log order. The database file is extended or truncated to
the page count in the last of those commits, then synced, and the log is
truncated to the header. A record that is cut off, has a bad CRC, has an
unknown type, or whose commit record names the wrong number of pages stops
the scan. That tail, and any page images that never got a commit record, are
discarded. The log is still truncated, so the next commit is not appended
after the torn bytes. Replaying the same log twice leaves the same file.

`SQLTOY_CRASH_AT` aborts the process at one point in that protocol:
`wal-partial` (after the first page record, before the commit record),
`before-wal-sync` (commit record written, log not synced), `after-wal-sync`
(log synced, database file not yet written), `mid-checkpoint` (first database
page written), and `before-wal-truncate` (database file synced, log not yet
truncated). The shell reports the abort. Status 134 is SIGABRT.

A crash before the commit record does not keep the insert. Create the table
first:

```bash
cargo run --quiet -- sql /tmp/sqltoy-wal.db "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT); INSERT INTO users VALUES (1, 'Ann'), (2, 'Bob')"
```

```text
CREATE TABLE
INSERT 2
```

Abort during the next insert. The shell reports `Aborted` and the status is
134:

```bash
SQLTOY_CRASH_AT=wal-partial cargo run --quiet -- sql /tmp/sqltoy-wal.db "INSERT INTO users VALUES (3, 'Cam')"
```

```text
Aborted
```

The next process prints the rows from before that crash:

```bash
cargo run --quiet -- sql /tmp/sqltoy-wal.db "SELECT * FROM users"
```

```text
 id | name 
----+------
  1 | Ann  
  2 | Bob  
(2 rows)
```

A crash after the log sync keeps the insert. Recovery replays it, and the
log is the 16-byte header again:

```bash
SQLTOY_CRASH_AT=after-wal-sync cargo run --quiet -- sql /tmp/sqltoy-wal.db "INSERT INTO users VALUES (3, 'Cam')"
```

```text
Aborted
```

```bash
cargo run --quiet -- sql /tmp/sqltoy-wal.db "SELECT * FROM users"
```

```text
 id | name 
----+------
  1 | Ann  
  2 | Bob  
  3 | Cam  
(3 rows)
```

`before-wal-sync` can go either way: the commit record is in the kernel cache
but was not synced, so a later open sees every change from that commit or
none of them. `after-wal-sync`, `mid-checkpoint`, and `before-wal-truncate`
all show the committed state.

## Storage CLI

```bash
cargo run --quiet -- write /tmp/demo.db 0 "Alice"
cargo run --quiet -- read /tmp/demo.db 0 5
cargo run --quiet -- len /tmp/demo.db
```

`write` stores the UTF-8 bytes of the text and syncs the file. A later `read`
is a new process; it prints the same value, which shows the bytes stayed on disk:

```text
wrote 5 bytes at offset 0
Alice
5
```

## Page CLI

```bash
cargo run --quiet -- page-alloc /tmp/pages.db
cargo run --quiet -- page-write /tmp/pages.db 1 "hello-page"
cargo run --quiet -- page-read /tmp/pages.db 1 10
cargo run --quiet -- page-count /tmp/pages.db
```

`page-alloc` appends a zeroed page. The first allocated id is 1, because page 0
is the header created when the file is new. `page-write` copies the text into
the start of that page and syncs. A later `page-read` is a new process and
prints the same bytes. `page-count` includes the header:

```text
allocated page 1
wrote 10 bytes to page 1
hello-page
2
```

## Record CLI

```bash
cargo run --quiet -- rec-insert /tmp/records.db 2 "Alice"
cargo run --quiet -- rec-insert /tmp/records.db 2 "Bob"
cargo run --quiet -- rec-get /tmp/records.db 2 1:0
cargo run --quiet -- rec-update /tmp/records.db 2 1:0 "Alicia"
cargo run --quiet -- rec-scan /tmp/records.db 2
cargo run --quiet -- rec-delete /tmp/records.db 2 1:1
cargo run --quiet -- rec-scan /tmp/records.db 2
```

`rec-*` is a low-level tool. It reads and writes record pages directly and
does not consult the catalog. The argument after the database path is the
table id that owns those pages. User tables start at id 2; the examples use
that id.

`rec-insert` appends a record and prints its id (`page:slot`). `rec-get` prints
the stored bytes as text. `rec-update` and `rec-delete` keep that same id.
`rec-scan` prints each live record of that table in page order, then slot
order. Each command is a new process; a later `rec-scan` shows the update and
the delete:

```text
inserted record 1:0
inserted record 1:1
Alice
updated record 1:0
1:0 Alicia
1:1 Bob
deleted record 1:1
1:0 Alicia
```

## Catalog CLI

```bash
cargo run --quiet -- table-create /tmp/catalog.db users id:INTEGER name:TEXT
cargo run --quiet -- table-create /tmp/catalog.db posts id:INTEGER title:TEXT
cargo run --quiet -- table-list /tmp/catalog.db
```

`table-create` checks the name and the `column:type` pairs, assigns the next
user-table id, writes one catalog record, and syncs. Add `:pk` to make an
`INTEGER` column the primary key (`id:INTEGER:pk`). `table-list` prints
`PRIMARY KEY` after that column's type. `table-list` below is a new process;
it prints the schemas loaded back from the file, in table-id order:

```text
created table users (id 2)
created table posts (id 3)
2 users (id INTEGER, name TEXT)
3 posts (id INTEGER, title TEXT)
```

## Row CLI

```bash
cargo run --quiet -- table-create /tmp/rows.db users id:INTEGER name:TEXT age:INTEGER
cargo run --quiet -- row-insert /tmp/rows.db users 1 Alice 30
cargo run --quiet -- row-insert /tmp/rows.db users 2 Bob NULL
cargo run --quiet -- row-scan /tmp/rows.db users
cargo run --quiet -- row-update /tmp/rows.db users 2:0 1 Alicia 31
cargo run --quiet -- row-delete /tmp/rows.db users 2:1
cargo run --quiet -- row-scan /tmp/rows.db users
```

`row-insert` checks the values against the table's columns and prints the new
record id (`page:slot`). `NULL`, in any ASCII case, is a null. Other values
for an `INTEGER` column are parsed as decimal `i64` values, and a `TEXT`
column stores the argument as given. Because `NULL` is reserved, the CLI
cannot store that word as text. `row-scan` prints each live row. Text is
shown in single quotes, and a quote inside the text is doubled. `row-update`
replaces the whole row. The id stays the same when the new row fits on its
page; when it does not, the line includes the new id
(`updated row 2:0 -> 3:0`). `row-delete` removes one row. Each command is a
new process. The last `row-scan` shows the update and the delete:

```text
created table users (id 2)
inserted row 2:0
inserted row 2:1
2:0 (1, 'Alice', 30)
2:1 (2, 'Bob', NULL)
updated row 2:0
deleted row 2:1
2:0 (1, 'Alicia', 31)
```

## SQL parser CLI

```bash
cargo run --quiet -- parse "CREATE TABLE users (id INTEGER, name TEXT)"
cargo run --quiet -- parse "INSERT INTO users (id, name) VALUES (1, 'Alice'), (2, 'Bob')"
cargo run --quiet -- parse "SELECT id, name AS n FROM users WHERE id = 1"
cargo run --quiet -- parse "SELECT 1 + 2 * 3 FROM t; DELETE FROM t WHERE id != 1"
```

`parse` does not open a database file. It prints each statement on its own
line, followed by a semicolon. Keywords come out in uppercase. Every compound
expression is parenthesized, and `!=` is printed as `<>`:

```text
CREATE TABLE users (id INTEGER, name TEXT);
INSERT INTO users (id, name) VALUES (1, 'Alice'), (2, 'Bob');
SELECT id, name AS n FROM users WHERE (id = 1);
SELECT (1 + (2 * 3)) FROM t;
DELETE FROM t WHERE (id <> 1);
```

A syntax error goes to stderr and the process exits with status 1.
`SELECT * WHERE id = 1` prints
`error: syntax error at 1:10: expected FROM, found WHERE`.
