use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Byte-addressable persistent storage backed by one local file.
pub struct Storage {
    file: File,
}

impl Storage {
    /// Opens `path` for reading and writing, creating the file if it does not exist.
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Storage> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        Ok(Storage { file })
    }

    /// Writes `data` starting at byte `offset`.
    ///
    /// The write is not flushed to stable storage until [`Storage::sync`].
    pub fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(data)
    }

    /// Reads exactly `len` bytes starting at `offset`.
    pub fn read_at(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        self.file.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0; len];
        self.file.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Returns the current file size in bytes.
    pub fn len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    /// Returns whether the file contains no bytes.
    pub fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Flushes buffered data and syncs the file so writes are durable.
    pub fn sync(&mut self) -> io::Result<()> {
        self.file.flush()?;
        self.file.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::Storage;
    use std::env::temp_dir;
    use std::fs;
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
            TempDb { path }
        }

        fn path(&self) -> &PathBuf {
            &self.path
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    #[test]
    fn write_then_read_roundtrip() {
        let db = TempDb::new("roundtrip");
        let mut storage = Storage::open(db.path()).unwrap();

        storage.write_at(0, b"hello").unwrap();
        let got = storage.read_at(0, 5).unwrap();
        assert_eq!(got, b"hello");

        storage.write_at(2, b"XYZ").unwrap();
        let got = storage.read_at(0, 5).unwrap();
        assert_eq!(got, b"heXYZ");
    }

    #[test]
    fn length_grows_with_writes() {
        let db = TempDb::new("length");
        let mut storage = Storage::open(db.path()).unwrap();

        assert!(storage.is_empty().unwrap());
        assert_eq!(storage.len().unwrap(), 0);

        storage.write_at(0, b"abcd").unwrap();
        assert!(!storage.is_empty().unwrap());
        assert_eq!(storage.len().unwrap(), 4);

        storage.write_at(8, b"xyz").unwrap();
        assert_eq!(storage.len().unwrap(), 11);
    }
}
