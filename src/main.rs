use std::env;
use std::io;
use std::process::ExitCode;

use sqltoy::{PageId, PageManager, RecordFile, RecordId, Storage, PAGE_SIZE};

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
  sqltoy rec-insert <db_path> <text>
  sqltoy rec-get <db_path> <record_id>
  sqltoy rec-update <db_path> <record_id> <text>
  sqltoy rec-delete <db_path> <record_id>
  sqltoy rec-scan <db_path>
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
        [cmd, path, text] if cmd == "rec-insert" => {
            cmd_rec_insert(path, text)?;
            Ok(())
        }
        [cmd, path, record_id] if cmd == "rec-get" => {
            cmd_rec_get(path, record_id)?;
            Ok(())
        }
        [cmd, path, record_id, text] if cmd == "rec-update" => {
            cmd_rec_update(path, record_id, text)?;
            Ok(())
        }
        [cmd, path, record_id] if cmd == "rec-delete" => {
            cmd_rec_delete(path, record_id)?;
            Ok(())
        }
        [cmd, path] if cmd == "rec-scan" => {
            cmd_rec_scan(path)?;
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

fn cmd_rec_insert(path: &str, text: &str) -> io::Result<()> {
    let mut records = RecordFile::open(path)?;
    let id = records.insert(text.as_bytes())?;
    println!("inserted record {id}");
    Ok(())
}

fn cmd_rec_get(path: &str, record_id: &str) -> io::Result<()> {
    let id = parse_record_id(record_id)?;
    let mut records = RecordFile::open(path)?;
    let bytes = records.get(id)?;
    println!("{}", String::from_utf8_lossy(&bytes));
    Ok(())
}

fn cmd_rec_update(path: &str, record_id: &str, text: &str) -> io::Result<()> {
    let id = parse_record_id(record_id)?;
    let mut records = RecordFile::open(path)?;
    records.update(id, text.as_bytes())?;
    println!("updated record {id}");
    Ok(())
}

fn cmd_rec_delete(path: &str, record_id: &str) -> io::Result<()> {
    let id = parse_record_id(record_id)?;
    let mut records = RecordFile::open(path)?;
    records.delete(id)?;
    println!("deleted record {id}");
    Ok(())
}

fn cmd_rec_scan(path: &str) -> io::Result<()> {
    let mut records = RecordFile::open(path)?;
    for (id, bytes) in records.scan()? {
        println!("{id} {}", String::from_utf8_lossy(&bytes));
    }
    Ok(())
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
