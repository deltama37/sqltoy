# sqltoy

sqltoy is a small learning RDBMS written in Rust. The point is to understand a
database by building its layers, not to match an existing engine. Design notes
are in `docs/adr/`:

- ADR-0001 chooses Rust and the initial scope.
- ADR-0002 fixes a bottom-up implementation order.
- ADR-0003 fixes the page size and the header page format.
- ADR-0004 fixes the slotted-page record format.
- ADR-0005 fixes table page ownership and the catalog record format.

## Status

The Storage layer (ADR-0002 step 1) reads and writes a local file by byte
offset. The Page Manager (ADR-0002 step 2, per ADR-0003) is implemented on
top of Storage: fixed 4096-byte pages, a reserved header page (page 0), and
allocate/read/write of pages with persistence. Record Storage (ADR-0002
step 3, per ADR-0004) stores variable-length records in slotted pages and
addresses them with stable record ids. Each record page belongs to one table.
The Catalog (ADR-0002 step 4, per ADR-0005) is done: table schemas are stored
in the database file and restored when the file is opened. The header format
version is 2. Table Operations and SQL are not implemented yet.

## Build and test

```bash
cargo build
cargo test
```

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
user-table id, writes one catalog record, and syncs. `table-list` below is a
new process; it prints the schemas loaded back from the file, in table-id
order:

```text
created table users (id 2)
created table posts (id 3)
2 users (id INTEGER, name TEXT)
3 posts (id INTEGER, title TEXT)
```
