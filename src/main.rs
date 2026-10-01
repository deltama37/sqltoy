use std::env;
use std::io;
use std::process::ExitCode;

use sqltoy::Storage;

const USAGE: &str = "\
usage: sqltoy <command> [args]

commands:
  sqltoy write <db_path> <offset> <text>
  sqltoy read <db_path> <offset> <len>
  sqltoy len <db_path>
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
