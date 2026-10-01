# sqltoy

sqltoy is a small learning RDBMS written in Rust. The point is to understand a
database by building its layers, not to match an existing engine. Design notes
are in `docs/adr/`:

- ADR-0001 chooses Rust and the initial scope.
- ADR-0002 fixes a bottom-up implementation order.

## Status

Step 1 of ADR-0002 only: a `Storage` layer that reads and writes a local file
by byte offset, and a small CLI that exercises it. Pages, records, and SQL are
not implemented yet.

## Build and test

```bash
cargo build
cargo test
```

## CLI

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
