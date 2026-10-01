# sqltoy

sqltoy is a small learning RDBMS written in Rust. The point is to understand a
database by building its layers, not to match an existing engine. Design notes
are in `docs/adr/`:

- ADR-0001 chooses Rust and the initial scope.
- ADR-0002 fixes a bottom-up implementation order.
- ADR-0003 fixes the page size and the header page format.

## Status

The Storage layer (ADR-0002 step 1) reads and writes a local file by byte
offset. The Page Manager (ADR-0002 step 2, per ADR-0003) is implemented on
top of Storage: fixed 4096-byte pages, a reserved header page (page 0), and
allocate/read/write of pages with persistence. Records and SQL are not
implemented yet.

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
