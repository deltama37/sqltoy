use std::env;
use std::io::{self, BufRead, IsTerminal, Write};
use std::process::ExitCode;

use sqltoy::sql::Statement;
use sqltoy::{
    format_result, Column, ColumnSpec, ColumnType, Database, PageId, PageManager, RecordFile,
    RecordId, SessionId, Storage, TableId, Value, PAGE_SIZE,
};

const USAGE: &str = "\
usage: sqltoy <command> [args]

commands:
  sqltoy write <db_path> <offset> <text>
  sqltoy read <db_path> <offset> <len>
  sqltoy len <db_path>
  sqltoy page-count <db_path>
  sqltoy page-alloc <db_path>
  sqltoy page-write <db_path> <page_id> <text>
  sqltoy page-read <db_path> <page_id> <len>
  sqltoy rec-insert <db_path> <table_id> <text>
  sqltoy rec-get <db_path> <table_id> <record_id>
  sqltoy rec-update <db_path> <table_id> <record_id> <text>
  sqltoy rec-delete <db_path> <table_id> <record_id>
  sqltoy rec-scan <db_path> <table_id>
  sqltoy table-create <db_path> <name> <column:type[:pk]>...
  sqltoy table-list <db_path>
  sqltoy row-insert <db_path> <table> <value>...
  sqltoy row-scan <db_path> <table>
  sqltoy row-update <db_path> <table> <record_id> <value>...
  sqltoy row-delete <db_path> <table> <record_id>
  sqltoy parse <sql>
  sqltoy sql <db_path> <sql>
  sqltoy repl <db_path>
";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let result = dispatch(&args);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliError::Usage) => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
        Err(CliError::Io(err)) => {
            eprintln!("error: {err}");
            ExitCode::from(1)
        }
    }
}

enum CliError {
    Usage,
    Io(io::Error),
}

impl From<io::Error> for CliError {
    fn from(err: io::Error) -> Self {
        CliError::Io(err)
    }
}

fn dispatch(args: &[String]) -> Result<(), CliError> {
    match args {
        [cmd, path, offset, text] if cmd == "write" => {
            cmd_write(path, offset, text)?;
            Ok(())
        }
        [cmd, path, offset, len] if cmd == "read" => {
            cmd_read(path, offset, len)?;
            Ok(())
        }
        [cmd, path] if cmd == "len" => {
            cmd_len(path)?;
            Ok(())
        }
        [cmd, path] if cmd == "page-count" => {
            cmd_page_count(path)?;
            Ok(())
        }
        [cmd, path] if cmd == "page-alloc" => {
            cmd_page_alloc(path)?;
            Ok(())
        }
        [cmd, path, page_id, text] if cmd == "page-write" => {
            cmd_page_write(path, page_id, text)?;
            Ok(())
        }
        [cmd, path, page_id, len] if cmd == "page-read" => {
            cmd_page_read(path, page_id, len)?;
            Ok(())
        }
        [cmd, path, table_id, text] if cmd == "rec-insert" => {
            cmd_rec_insert(path, table_id, text)?;
            Ok(())
        }
        [cmd, path, table_id, record_id] if cmd == "rec-get" => {
            cmd_rec_get(path, table_id, record_id)?;
            Ok(())
        }
        [cmd, path, table_id, record_id, text] if cmd == "rec-update" => {
            cmd_rec_update(path, table_id, record_id, text)?;
            Ok(())
        }
        [cmd, path, table_id, record_id] if cmd == "rec-delete" => {
            cmd_rec_delete(path, table_id, record_id)?;
            Ok(())
        }
        [cmd, path, table_id] if cmd == "rec-scan" => {
            cmd_rec_scan(path, table_id)?;
            Ok(())
        }
        [cmd, path, name, columns @ ..] if cmd == "table-create" && !columns.is_empty() => {
            cmd_table_create(path, name, columns)?;
            Ok(())
        }
        [cmd, path] if cmd == "table-list" => {
            cmd_table_list(path)?;
            Ok(())
        }
        [cmd, path, table, values @ ..] if cmd == "row-insert" && !values.is_empty() => {
            cmd_row_insert(path, table, values)?;
            Ok(())
        }
        [cmd, path, table] if cmd == "row-scan" => {
            cmd_row_scan(path, table)?;
            Ok(())
        }
        [cmd, path, table, record_id, values @ ..] if cmd == "row-update" && !values.is_empty() => {
            cmd_row_update(path, table, record_id, values)?;
            Ok(())
        }
        [cmd, path, table, record_id] if cmd == "row-delete" => {
            cmd_row_delete(path, table, record_id)?;
            Ok(())
        }
        [cmd, sql] if cmd == "parse" => {
            cmd_parse(sql)?;
            Ok(())
        }
        [cmd, path, sql] if cmd == "sql" => {
            cmd_sql(path, sql)?;
            Ok(())
        }
        [cmd, path] if cmd == "repl" => {
            cmd_repl(path)?;
            Ok(())
        }
        _ => Err(CliError::Usage),
    }
}

fn cmd_write(path: &str, offset: &str, text: &str) -> io::Result<()> {
    let offset = parse_offset(offset)?;
    let bytes = text.as_bytes();
    let mut storage = Storage::open(path)?;
    storage.write_at(offset, bytes)?;
    storage.sync()?;
    println!("wrote {} bytes at offset {offset}", bytes.len());
    Ok(())
}

fn cmd_read(path: &str, offset: &str, len: &str) -> io::Result<()> {
    let offset = parse_offset(offset)?;
    let len = parse_len(len)?;
    let mut storage = Storage::open(path)?;
    let bytes = storage.read_at(offset, len)?;
    println!("{}", String::from_utf8_lossy(&bytes));
    Ok(())
}

fn cmd_len(path: &str) -> io::Result<()> {
    let storage = Storage::open(path)?;
    println!("{}", storage.len()?);
    Ok(())
}

fn cmd_page_count(path: &str) -> io::Result<()> {
    let pages = PageManager::open(path)?;
    println!("{}", pages.page_count()?);
    Ok(())
}

fn cmd_page_alloc(path: &str) -> io::Result<()> {
    let mut pages = PageManager::open(path)?;
    let id = pages.allocate_page()?;
    println!("allocated page {id}");
    Ok(())
}

fn cmd_page_write(path: &str, page_id: &str, text: &str) -> io::Result<()> {
    let id = PageId(parse_u32(page_id)?);
    let bytes = text.as_bytes();
    if bytes.len() > PAGE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("text is longer than page size ({PAGE_SIZE})"),
        ));
    }
    let mut pages = PageManager::open(path)?;
    let mut page = pages.read_page(id)?;
    page.data_mut()[..bytes.len()].copy_from_slice(bytes);
    pages.write_page(id, &page)?;
    pages.sync()?;
    println!("wrote {} bytes to page {id}", bytes.len());
    Ok(())
}

fn cmd_page_read(path: &str, page_id: &str, len: &str) -> io::Result<()> {
    let id = PageId(parse_u32(page_id)?);
    let len = parse_len(len)?;
    if len > PAGE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("length {len} exceeds page size ({PAGE_SIZE})"),
        ));
    }
    let mut pages = PageManager::open(path)?;
    let page = pages.read_page(id)?;
    println!("{}", String::from_utf8_lossy(&page.data()[..len]));
    Ok(())
}

fn cmd_rec_insert(path: &str, table_id: &str, text: &str) -> io::Result<()> {
    let table = parse_table_id(table_id)?;
    let mut records = RecordFile::open(path)?;
    let id = records.insert(table, text.as_bytes())?;
    println!("inserted record {id}");
    Ok(())
}

fn cmd_rec_get(path: &str, table_id: &str, record_id: &str) -> io::Result<()> {
    let table = parse_table_id(table_id)?;
    let id = parse_record_id(record_id)?;
    let mut records = RecordFile::open(path)?;
    let bytes = records.get(table, id)?;
    println!("{}", String::from_utf8_lossy(&bytes));
    Ok(())
}

fn cmd_rec_update(path: &str, table_id: &str, record_id: &str, text: &str) -> io::Result<()> {
    let table = parse_table_id(table_id)?;
    let id = parse_record_id(record_id)?;
    let mut records = RecordFile::open(path)?;
    records.update(table, id, text.as_bytes())?;
    println!("updated record {id}");
    Ok(())
}

fn cmd_rec_delete(path: &str, table_id: &str, record_id: &str) -> io::Result<()> {
    let table = parse_table_id(table_id)?;
    let id = parse_record_id(record_id)?;
    let mut records = RecordFile::open(path)?;
    records.delete(table, id)?;
    println!("deleted record {id}");
    Ok(())
}

fn cmd_rec_scan(path: &str, table_id: &str) -> io::Result<()> {
    let table = parse_table_id(table_id)?;
    let mut records = RecordFile::open(path)?;
    for (id, bytes) in records.scan(table)? {
        println!("{id} {}", String::from_utf8_lossy(&bytes));
    }
    Ok(())
}

fn cmd_table_create(path: &str, name: &str, columns: &[String]) -> io::Result<()> {
    let mut parsed = Vec::with_capacity(columns.len());
    for raw in columns {
        parsed.push(parse_column_def(raw)?);
    }
    let mut db = Database::open(path)?;
    let schema = db.create_table(name, &parsed)?;
    println!("created table {} (id {})", schema.name, schema.id);
    Ok(())
}

fn cmd_table_list(path: &str) -> io::Result<()> {
    let db = Database::open(path)?;
    for table in db.tables() {
        let columns = table
            .columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                if table.primary_key == Some(index) {
                    format!("{} {} PRIMARY KEY", column.name, column.column_type)
                } else {
                    format!("{} {}", column.name, column.column_type)
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        println!("{} {} ({columns})", table.id, table.name);
    }
    Ok(())
}

fn cmd_row_insert(path: &str, table: &str, raw_values: &[String]) -> io::Result<()> {
    let mut db = Database::open(path)?;
    let values = parse_row_values(&db, table, raw_values)?;
    let id = db.insert(table, &values)?;
    println!("inserted row {id}");
    Ok(())
}

fn cmd_row_scan(path: &str, table: &str) -> io::Result<()> {
    let mut db = Database::open(path)?;
    for (id, values) in db.scan(table)? {
        println!("{id} {}", format_values(&values));
    }
    Ok(())
}

fn cmd_row_update(
    path: &str,
    table: &str,
    record_id: &str,
    raw_values: &[String],
) -> io::Result<()> {
    let id = parse_record_id(record_id)?;
    let mut db = Database::open(path)?;
    let values = parse_row_values(&db, table, raw_values)?;
    let new_id = db.update(table, id, &values)?;
    if new_id == id {
        println!("updated row {id}");
    } else {
        println!("updated row {id} -> {new_id}");
    }
    Ok(())
}

fn cmd_row_delete(path: &str, table: &str, record_id: &str) -> io::Result<()> {
    let id = parse_record_id(record_id)?;
    let mut db = Database::open(path)?;
    db.delete(table, id)?;
    println!("deleted row {id}");
    Ok(())
}

fn cmd_parse(sql: &str) -> io::Result<()> {
    for statement in sqltoy::sql::parse(sql)? {
        println!("{statement};");
    }
    Ok(())
}

fn cmd_sql(path: &str, sql: &str) -> io::Result<()> {
    let mut db = Database::open(path)?;
    let result = run_sql(&mut db, sql);
    rollback_if_open(&mut db)?;
    result
}

fn run_sql(db: &mut Database, sql: &str) -> io::Result<()> {
    for statement in sqltoy::sql::parse(sql)? {
        let result = db.execute_statement(&statement)?;
        println!("{}", format_result(&result));
        io::stdout().flush()?;
    }
    Ok(())
}

struct ReplSession {
    name: String,
    id: SessionId,
}

fn cmd_repl(path: &str) -> io::Result<()> {
    let mut db = Database::open(path)?;
    let mut sessions = vec![ReplSession {
        name: "main".to_string(),
        id: db.default_session(),
    }];
    let mut current = 0usize;
    let interactive = io::stdin().is_terminal();
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut buffer = String::new();
    loop {
        if interactive {
            if buffer.trim().is_empty() {
                let open = db.session_in_transaction(sessions[current].id)?;
                print!("{}", prompt(&sessions[current].name, open));
            } else {
                print!("   ...> ");
            }
            io::stdout().flush()?;
        }
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            if !buffer.trim().is_empty() {
                run_buffer(&mut db, sessions[current].id, &buffer);
            }
            break;
        }
        // Dot commands apply only when no statement is in progress.
        if buffer.trim().is_empty() {
            let trimmed = line.trim();
            if trimmed == ".quit" || trimmed == ".exit" {
                break;
            }
            if trimmed == ".stats" {
                print_buffer_stats(&db);
                continue;
            }
            if let Some(name) = trimmed.strip_prefix(".session") {
                let name = name.trim();
                if !valid_session_name(name) {
                    eprintln!("error: invalid session name");
                    continue;
                }
                current = switch_session(&mut db, &mut sessions, name);
                continue;
            }
        }
        buffer.push_str(&line);
        if buffer.trim_end().ends_with(';') {
            run_buffer(&mut db, sessions[current].id, &buffer);
            buffer.clear();
        }
    }
    rollback_open_sessions(&mut db, &sessions)?;
    Ok(())
}

fn valid_session_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn prompt(name: &str, open: bool) -> String {
    if name == "main" {
        if open {
            "sqltoy*> ".to_string()
        } else {
            "sqltoy> ".to_string()
        }
    } else if open {
        format!("sqltoy:{name}*> ")
    } else {
        format!("sqltoy:{name}> ")
    }
}

fn switch_session(db: &mut Database, sessions: &mut Vec<ReplSession>, name: &str) -> usize {
    if let Some(index) = sessions.iter().position(|session| session.name == name) {
        return index;
    }
    let id = db.create_session();
    sessions.push(ReplSession {
        name: name.to_string(),
        id,
    });
    sessions.len() - 1
}

fn rollback_open_sessions(db: &mut Database, sessions: &[ReplSession]) -> io::Result<()> {
    for session in sessions {
        if db.session_in_transaction(session.id)? {
            if session.name == "main" {
                eprintln!("warning: transaction rolled back");
            } else {
                eprintln!(
                    "warning: transaction rolled back (session {})",
                    session.name
                );
            }
            db.execute_statement_in(session.id, &Statement::Rollback)?;
        }
    }
    Ok(())
}

fn rollback_if_open(db: &mut Database) -> io::Result<()> {
    if db.in_transaction() {
        eprintln!("warning: transaction rolled back");
        db.rollback()?;
    }
    Ok(())
}

fn print_buffer_stats(db: &Database) {
    let stats = db.buffer_stats();
    println!(
        "logical reads: {}, hits: {}, misses: {}, pages written: {}, evictions: {}",
        stats.logical_reads, stats.hits, stats.misses, stats.pages_written, stats.evictions
    );
}

fn run_buffer(db: &mut Database, session: SessionId, sql: &str) {
    let statements = match sqltoy::sql::parse(sql) {
        Ok(statements) => statements,
        Err(err) => {
            eprintln!("error: {err}");
            return;
        }
    };
    for statement in &statements {
        match db.execute_statement_in(session, statement) {
            Ok(result) => {
                println!("{}", format_result(&result));
                let _ = io::stdout().flush();
            }
            Err(err) => {
                eprintln!("error: {err}");
                return;
            }
        }
    }
}

fn parse_row_values(db: &Database, table: &str, raw_values: &[String]) -> io::Result<Vec<Value>> {
    let schema = db.table(table).ok_or_else(|| table_not_found(table))?;
    if raw_values.len() != schema.columns.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "expected {} values for table {}, got {}",
                schema.columns.len(),
                schema.name,
                raw_values.len()
            ),
        ));
    }
    let mut values = Vec::with_capacity(raw_values.len());
    for (column, raw) in schema.columns.iter().zip(raw_values) {
        values.push(parse_value(column, raw)?);
    }
    Ok(values)
}

fn parse_value(column: &Column, raw: &str) -> io::Result<Value> {
    if raw.eq_ignore_ascii_case("null") {
        return Ok(Value::Null);
    }
    match column.column_type {
        ColumnType::Integer => raw.parse::<i64>().map(Value::Integer).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid integer for column {}: {raw}", column.name),
            )
        }),
        ColumnType::Text => Ok(Value::Text(raw.to_string())),
    }
}

fn format_values(values: &[Value]) -> String {
    let body = values
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!("({body})")
}

fn table_not_found(name: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("table not found: {name}"))
}

fn parse_column_def(raw: &str) -> io::Result<ColumnSpec<'_>> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid column definition: {raw}"),
        )
    };
    let mut parts = raw.split(':');
    let Some(name) = parts.next() else {
        return Err(invalid());
    };
    let Some(ty) = parts.next() else {
        return Err(invalid());
    };
    if name.is_empty() || ty.is_empty() {
        return Err(invalid());
    }
    let primary_key = match parts.next() {
        None => false,
        Some(flag) if flag.eq_ignore_ascii_case("pk") => true,
        Some(_) => return Err(invalid()),
    };
    if parts.next().is_some() {
        return Err(invalid());
    }
    let column_type = ty.parse()?;
    Ok(ColumnSpec {
        name,
        column_type,
        primary_key,
    })
}

fn parse_table_id(raw: &str) -> io::Result<TableId> {
    raw.parse::<u16>().map(TableId).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid table id: {raw}"),
        )
    })
}

fn parse_record_id(raw: &str) -> io::Result<RecordId> {
    raw.parse()
}

fn parse_offset(raw: &str) -> io::Result<u64> {
    raw.parse::<u64>().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid offset: {raw}"),
        )
    })
}

fn parse_len(raw: &str) -> io::Result<usize> {
    raw.parse::<usize>().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid length: {raw}"),
        )
    })
}

fn parse_u32(raw: &str) -> io::Result<u32> {
    raw.parse::<u32>().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid page id: {raw}"),
        )
    })
}
