//! Table schemas stored in the catalog.
//!
//! The catalog is the record set of [`TableId::CATALOG`]. Each user table is
//! one record. Opening a database reads those records back into memory.

use std::fmt;
use std::io::{self, ErrorKind};
use std::path::Path;
use std::str::FromStr;

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

/// Name and columns of one user table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSchema {
    /// Table id assigned at creation.
    pub id: TableId,
    /// Table name, stored with the spelling from the definition.
    pub name: String,
    /// Columns in definition order.
    pub columns: Vec<Column>,
}

/// An open database file and the schemas loaded from its catalog.
pub struct Database {
    records: RecordFile,
    tables: Vec<TableSchema>,
}

impl Database {
    /// Opens the database at `path` and loads every catalog record.
    ///
    /// An empty file is created. Schemas are ordered by table id. A record
    /// that does not match the catalog layout, an unknown column type, a
    /// table id below [`TableId::FIRST_USER`], or a duplicate id or name is
    /// [`ErrorKind::InvalidData`].
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Database> {
        let mut records = RecordFile::open(path)?;
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
        Ok(Database { records, tables })
    }

    /// Creates a user table and syncs its catalog record.
    ///
    /// The assigned id is one greater than the highest user-table id already
    /// in the catalog, or [`TableId::FIRST_USER`] when there is none. `name`
    /// and each column name must be identifiers. Comparison ignores ASCII
    /// case; the stored spelling is the one passed in.
    ///
    /// An empty column list, a bad identifier, a duplicate table or column
    /// name, a definition longer than [`MAX_RECORD_SIZE`], or an id that
    /// would not fit in `u16` is [`ErrorKind::InvalidInput`].
    pub fn create_table(
        &mut self,
        name: &str,
        columns: &[(&str, ColumnType)],
    ) -> io::Result<&TableSchema> {
        check_identifier(name)?;
        if columns.is_empty() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "table needs at least one column",
            ));
        }
        let mut defined = Vec::with_capacity(columns.len());
        for &(column_name, column_type) in columns {
            check_identifier(column_name)?;
            if defined
                .iter()
                .any(|column: &Column| names_eq(&column.name, column_name))
            {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    format!("duplicate column: {column_name}"),
                ));
            }
            defined.push(Column {
                name: column_name.to_string(),
                column_type,
            });
        }
        if self.tables.iter().any(|table| names_eq(&table.name, name)) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("table already exists: {name}"),
            ));
        }
        let id = next_table_id(&self.tables)?;
        let schema = TableSchema {
            id,
            name: name.to_string(),
            columns: defined,
        };
        let bytes = encode_schema(&schema)?;
        self.records.insert(TableId::CATALOG, &bytes)?;
        self.tables.push(schema);
        Ok(&self.tables[self.tables.len() - 1])
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
    if !reader.is_empty() {
        return None;
    }
    Some(TableSchema {
        id: TableId(table_id),
        name,
        columns,
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
    use super::{decode_catalog_record, encode_schema, Column, ColumnType, Database, TableSchema};
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
            b'm', b'e', 2,
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
        *bad_type.last_mut().unwrap() = 9;
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
                    &[("id", ColumnType::Integer), ("name", ColumnType::Text)],
                )
                .unwrap()
                .id;
            let posts_id = database
                .create_table(
                    "posts",
                    &[("id", ColumnType::Integer), ("title", ColumnType::Text)],
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
                .create_table("Users", &[("id", ColumnType::Integer)])
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
                .create_table(name, &[("id", ColumnType::Integer)])
                .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidInput);
            assert_eq!(err.to_string(), format!("invalid identifier: {name}"));
        }
        let err = database
            .create_table("users", &[("1id", ColumnType::Integer)])
            .unwrap_err();
        assert_eq!(err.to_string(), "invalid identifier: 1id");
        let err = database.create_table("users", &[]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "table needs at least one column");
        let err = database
            .create_table(
                "users",
                &[("id", ColumnType::Integer), ("ID", ColumnType::Text)],
            )
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "duplicate column: ID");
        assert!(database.tables().is_empty());

        let long = "a".repeat(64);
        let id = database
            .create_table(&long, &[("_id", ColumnType::Integer)])
            .unwrap()
            .id;
        assert_eq!(id, TableId::FIRST_USER);
        assert_eq!(database.table(&long).unwrap().columns[0].name, "_id");
        database
            .create_table("_t", &[("_c", ColumnType::Text)])
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
        let columns: Vec<(&str, ColumnType)> = wide
            .iter()
            .map(|(name, column_type)| (name.as_str(), *column_type))
            .collect();
        let err = database.create_table("t", &columns).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "table definition too large: t");
        assert!(database.tables().is_empty());

        let fitting: Vec<(String, ColumnType)> = (0..60)
            .map(|index| (format!("c{index:063}"), ColumnType::Text))
            .collect();
        let columns: Vec<(&str, ColumnType)> = fitting
            .iter()
            .map(|(name, column_type)| (name.as_str(), *column_type))
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
                .create_table("users", &[("id", ColumnType::Integer)])
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
            .create_table("comments", &[("id", ColumnType::Integer)])
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
            .create_table("another", &[("id", ColumnType::Integer)])
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "too many tables");
        assert_eq!(database.tables().len(), 1);
    }

    fn error_of<T>(result: std::io::Result<T>) -> std::io::Error {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(err) => err,
        }
    }
}
