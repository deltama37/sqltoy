//! Encoded rows stored as record bytes.
//!
//! A row is a null bitmap followed by the non-null values in column order.
//! The column count and types come from the table schema, not from the row.

use std::fmt;
use std::io::{self, ErrorKind};

use crate::catalog::{ColumnType, TableSchema};
use crate::record::{RecordId, MAX_RECORD_SIZE};

/// One stored column value, or a boolean produced by an expression.
///
/// Display prints `NULL`, the integer in decimal, text in SQL single quotes
/// with each embedded `'` doubled, or `TRUE` / `FALSE`. A boolean is not a
/// column type and cannot be stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// SQL NULL. Allowed in every column.
    Null,
    /// Signed 64-bit integer.
    Integer(i64),
    /// UTF-8 text.
    Text(String),
    /// `TRUE` or `FALSE`. An expression result, never a stored column.
    Boolean(bool),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("NULL"),
            Value::Integer(value) => write!(f, "{value}"),
            Value::Text(value) => write!(f, "'{}'", value.replace('\'', "''")),
            Value::Boolean(true) => f.write_str("TRUE"),
            Value::Boolean(false) => f.write_str("FALSE"),
        }
    }
}

/// Encodes `values` for `schema` into record bytes.
///
/// The value count must match the column count. NULL is allowed in every
/// column. An integer is stored only in an `INTEGER` column and text only in
/// a `TEXT` column. A boolean is a type mismatch for both. There is no
/// implicit conversion.
///
/// The bytes are a null bitmap of `ceil(n / 8)` bytes, then the non-null
/// values in column order. Column `i` is bit `i % 8` of byte `i / 8`, with
/// the least significant bit numbered 0. A set bit means NULL. An `INTEGER`
/// is an `i64` in little-endian order. A `TEXT` is a `u16` byte length in
/// little-endian order followed by UTF-8 bytes. Padding bits past the last
/// column are zero.
///
/// A count mismatch or a type mismatch is [`ErrorKind::InvalidInput`]. Text
/// longer than `u16::MAX`, or an encoded row longer than [`MAX_RECORD_SIZE`],
/// is [`ErrorKind::InvalidInput`].
pub fn encode_row(schema: &TableSchema, values: &[Value]) -> io::Result<Vec<u8>> {
    let columns = schema.columns.len();
    if values.len() != columns {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "expected {columns} values for table {}, got {}",
                schema.name,
                values.len()
            ),
        ));
    }
    for (column, value) in schema.columns.iter().zip(values) {
        if !value_matches(column.column_type, value) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "type mismatch for column {}: expected {}",
                    column.name, column.column_type
                ),
            ));
        }
    }

    let mut bytes = vec![0u8; columns.div_ceil(8)];
    for (index, value) in values.iter().enumerate() {
        if matches!(value, Value::Null) {
            set_null(&mut bytes, index);
        }
    }
    for (column, value) in schema.columns.iter().zip(values) {
        match value {
            Value::Null => {}
            Value::Integer(n) => bytes.extend_from_slice(&n.to_le_bytes()),
            Value::Text(text) => {
                let len = u16::try_from(text.len()).map_err(|_| row_too_large(&schema.name))?;
                bytes.extend_from_slice(&len.to_le_bytes());
                bytes.extend_from_slice(text.as_bytes());
            }
            // value_matches rejects booleans before this loop.
            Value::Boolean(_) => {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "type mismatch for column {}: expected {}",
                        column.name, column.column_type
                    ),
                ));
            }
        }
    }
    if bytes.len() > MAX_RECORD_SIZE {
        return Err(row_too_large(&schema.name));
    }
    Ok(bytes)
}

/// Decodes a row stored for `schema`.
///
/// Every byte of `bytes` must be consumed. Truncation, trailing bytes, bytes
/// that are not UTF-8, or a set bit in the bitmap past the last column is
/// [`ErrorKind::InvalidData`].
pub fn decode_row(schema: &TableSchema, rid: RecordId, bytes: &[u8]) -> io::Result<Vec<Value>> {
    decode_parts(schema, bytes).ok_or_else(|| invalid_row(rid))
}

fn decode_parts(schema: &TableSchema, bytes: &[u8]) -> Option<Vec<Value>> {
    let columns = schema.columns.len();
    let bitmap_len = columns.div_ceil(8);
    if bytes.len() < bitmap_len {
        return None;
    }
    let bitmap = &bytes[..bitmap_len];
    if !padding_clear(bitmap, columns) {
        return None;
    }
    let mut index = bitmap_len;
    let mut values = Vec::with_capacity(columns);
    for (column_index, column) in schema.columns.iter().enumerate() {
        if is_null(bitmap, column_index) {
            values.push(Value::Null);
            continue;
        }
        match column.column_type {
            ColumnType::Integer => {
                let end = index.checked_add(8)?;
                if end > bytes.len() {
                    return None;
                }
                let mut buf = [0u8; 8];
                buf.copy_from_slice(&bytes[index..end]);
                values.push(Value::Integer(i64::from_le_bytes(buf)));
                index = end;
            }
            ColumnType::Text => {
                let len_end = index.checked_add(2)?;
                if len_end > bytes.len() {
                    return None;
                }
                let len = u16::from_le_bytes([bytes[index], bytes[index + 1]]) as usize;
                let end = len_end.checked_add(len)?;
                if end > bytes.len() {
                    return None;
                }
                let text = std::str::from_utf8(&bytes[len_end..end]).ok()?;
                values.push(Value::Text(text.to_string()));
                index = end;
            }
        }
    }
    if index != bytes.len() {
        None
    } else {
        Some(values)
    }
}

fn value_matches(column_type: ColumnType, value: &Value) -> bool {
    matches!(
        (column_type, value),
        (_, Value::Null)
            | (ColumnType::Integer, Value::Integer(_))
            | (ColumnType::Text, Value::Text(_))
    )
}

fn set_null(bitmap: &mut [u8], index: usize) {
    bitmap[index / 8] |= 1u8 << (index % 8);
}

fn is_null(bitmap: &[u8], index: usize) -> bool {
    (bitmap[index / 8] & (1u8 << (index % 8))) != 0
}

fn padding_clear(bitmap: &[u8], columns: usize) -> bool {
    let spare = columns % 8;
    if spare == 0 {
        return true;
    }
    let used = (1u8 << spare) - 1;
    (bitmap[columns / 8] & !used) == 0
}

fn row_too_large(table: &str) -> io::Error {
    io::Error::new(
        ErrorKind::InvalidInput,
        format!("row too large for table {table}"),
    )
}

fn invalid_row(rid: RecordId) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, format!("invalid row: {rid}"))
}

#[cfg(test)]
mod tests {
    use super::{decode_row, encode_row, Value};
    use crate::catalog::{Column, ColumnType, TableSchema};
    use crate::page::PageId;
    use crate::record::{RecordId, TableId, MAX_RECORD_SIZE};
    use std::io::ErrorKind;

    fn rid() -> RecordId {
        RecordId {
            page_id: PageId(1),
            slot_id: 4,
        }
    }

    fn column(name: &str, column_type: ColumnType) -> Column {
        Column {
            name: name.to_string(),
            column_type,
        }
    }

    fn schema(name: &str, columns: Vec<Column>) -> TableSchema {
        TableSchema {
            id: TableId(2),
            name: name.to_string(),
            columns,
            primary_key: None,
            index_root: None,
        }
    }

    fn users() -> TableSchema {
        schema(
            "users",
            vec![
                column("id", ColumnType::Integer),
                column("name", ColumnType::Text),
                column("age", ColumnType::Integer),
            ],
        )
    }

    fn wide(n: usize) -> TableSchema {
        let columns = (0..n)
            .map(|index| {
                let column_type = if index % 2 == 0 {
                    ColumnType::Integer
                } else {
                    ColumnType::Text
                };
                column(&format!("c{index}"), column_type)
            })
            .collect();
        schema("wide", columns)
    }

    #[test]
    fn value_display_quotes_text() {
        assert_eq!(Value::Null.to_string(), "NULL");
        assert_eq!(Value::Integer(0).to_string(), "0");
        assert_eq!(Value::Integer(-42).to_string(), "-42");
        assert_eq!(Value::Text("it's".to_string()).to_string(), "'it''s'");
        assert_eq!(Value::Text(String::new()).to_string(), "''");
        assert_eq!(Value::Text("'".to_string()).to_string(), "''''");
        assert_eq!(Value::Text("a'b'c".to_string()).to_string(), "'a''b''c'");
        assert_eq!(Value::Text("NULL".to_string()).to_string(), "'NULL'");
        assert_eq!(Value::Boolean(true).to_string(), "TRUE");
        assert_eq!(Value::Boolean(false).to_string(), "FALSE");
    }

    #[test]
    fn null_in_the_middle_is_byte_exact() {
        let values = vec![Value::Integer(1), Value::Null, Value::Integer(30)];
        let bytes = encode_row(&users(), &values).unwrap();
        let expected = [
            0x02, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1e, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00,
        ];
        assert_eq!(bytes, expected);
        assert_eq!(decode_row(&users(), rid(), &bytes).unwrap(), values);
    }

    #[test]
    fn nine_columns_use_a_two_byte_bitmap() {
        let schema = wide(9);
        let values = vec![
            Value::Null,
            Value::Text("a".to_string()),
            Value::Integer(2),
            Value::Null,
            Value::Integer(4),
            Value::Text(String::new()),
            Value::Integer(6),
            Value::Text("xy".to_string()),
            Value::Null,
        ];
        let bytes = encode_row(&schema, &values).unwrap();
        let expected = [
            0x09, 0x01, 0x01, 0x00, b'a', 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x02, 0x00, b'x', b'y',
        ];
        assert_eq!(bytes, expected);
        assert_eq!(decode_row(&schema, rid(), &bytes).unwrap(), values);

        let eight = wide(8);
        let nulls = vec![Value::Null; 8];
        assert_eq!(encode_row(&eight, &nulls).unwrap(), vec![0xff]);
        assert_eq!(decode_row(&eight, rid(), &[0xff]).unwrap(), nulls);

        let nine_nulls = vec![Value::Null; 9];
        assert_eq!(encode_row(&schema, &nine_nulls).unwrap(), vec![0xff, 0x01]);
        assert_eq!(
            decode_row(&schema, rid(), &[0xff, 0x01]).unwrap(),
            nine_nulls
        );
    }

    #[test]
    fn roundtrip_preserves_values() {
        let values = vec![
            Value::Integer(-1),
            Value::Text("it's 雪".to_string()),
            Value::Null,
        ];
        let bytes = encode_row(&users(), &values).unwrap();
        assert_eq!(decode_row(&users(), rid(), &bytes).unwrap(), values);

        let all_null = vec![Value::Null, Value::Null, Value::Null];
        let bytes = encode_row(&users(), &all_null).unwrap();
        assert_eq!(bytes, vec![0x07]);
        assert_eq!(decode_row(&users(), rid(), &bytes).unwrap(), all_null);

        let text_only = schema("notes", vec![column("body", ColumnType::Text)]);
        let fitting = "x".repeat(MAX_RECORD_SIZE - 3);
        let values = vec![Value::Text(fitting.clone())];
        let bytes = encode_row(&text_only, &values).unwrap();
        assert_eq!(bytes.len(), MAX_RECORD_SIZE);
        assert_eq!(decode_row(&text_only, rid(), &bytes).unwrap(), values);

        let many = schema(
            "nums",
            (0..500)
                .map(|index| column(&format!("c{index}"), ColumnType::Integer))
                .collect(),
        );
        let values = vec![Value::Integer(1); 500];
        let bytes = encode_row(&many, &values).unwrap();
        assert!(bytes.len() <= MAX_RECORD_SIZE);
        assert_eq!(decode_row(&many, rid(), &bytes).unwrap(), values);

        let nulls = schema(
            "nulls",
            (0..511)
                .map(|index| column(&format!("c{index}"), ColumnType::Integer))
                .collect(),
        );
        let values = vec![Value::Null; 511];
        let bytes = encode_row(&nulls, &values).unwrap();
        assert_eq!(bytes.len(), 64);
        assert_eq!(decode_row(&nulls, rid(), &bytes).unwrap(), values);
    }

    #[test]
    fn encode_rejects_wrong_count() {
        let err = encode_row(&users(), &[]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "expected 3 values for table users, got 0");

        let err = encode_row(&users(), &[Value::Integer(1)]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "expected 3 values for table users, got 1");

        let err = encode_row(
            &users(),
            &[
                Value::Text("nope".to_string()),
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "expected 3 values for table users, got 4");
    }

    #[test]
    fn encode_rejects_type_mismatch() {
        let err = encode_row(
            &users(),
            &[
                Value::Text("x".to_string()),
                Value::Text("y".to_string()),
                Value::Integer(1),
            ],
        )
        .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            err.to_string(),
            "type mismatch for column id: expected INTEGER"
        );

        let err = encode_row(
            &users(),
            &[Value::Integer(1), Value::Integer(2), Value::Null],
        )
        .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            err.to_string(),
            "type mismatch for column name: expected TEXT"
        );

        let err = encode_row(
            &users(),
            &[Value::Null, Value::Null, Value::Text("x".to_string())],
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "type mismatch for column age: expected INTEGER"
        );

        let err = encode_row(
            &users(),
            &[
                Value::Boolean(true),
                Value::Text("y".to_string()),
                Value::Integer(1),
            ],
        )
        .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            err.to_string(),
            "type mismatch for column id: expected INTEGER"
        );

        let err = encode_row(
            &users(),
            &[Value::Integer(1), Value::Boolean(false), Value::Integer(1)],
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "type mismatch for column name: expected TEXT"
        );

        let err = encode_row(
            &users(),
            &[
                Value::Null,
                Value::Text("y".to_string()),
                Value::Boolean(true),
            ],
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "type mismatch for column age: expected INTEGER"
        );
    }

    #[test]
    fn encode_rejects_row_too_large() {
        let text_only = schema("notes", vec![column("body", ColumnType::Text)]);
        let too_long = "x".repeat(u16::MAX as usize + 1);
        let err = encode_row(&text_only, &[Value::Text(too_long)]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "row too large for table notes");

        let just_over = "x".repeat(MAX_RECORD_SIZE - 2);
        let err = encode_row(&text_only, &[Value::Text(just_over)]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "row too large for table notes");

        let many = schema(
            "nums",
            (0..511)
                .map(|index| column(&format!("c{index}"), ColumnType::Integer))
                .collect(),
        );
        let err = encode_row(&many, &vec![Value::Integer(1); 511]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "row too large for table nums");
    }

    #[test]
    fn decode_rejects_corrupt_rows() {
        let schema = users();
        let valid = encode_row(
            &schema,
            &[
                Value::Integer(1),
                Value::Text("hi".to_string()),
                Value::Integer(30),
            ],
        )
        .unwrap();

        let mut cases = vec![
            Vec::new(),
            vec![0x00],
            valid[..valid.len() - 1].to_vec(),
            {
                let mut trailing = valid.clone();
                trailing.push(0);
                trailing
            },
            {
                let mut padded = valid.clone();
                padded[0] |= 0x80;
                padded
            },
            vec![0x0f],
            {
                let mut short_text = valid.clone();
                // name length claims more bytes than remain after it.
                let name_len_at = 1 + 8;
                short_text[name_len_at] = 50;
                short_text[name_len_at + 1] = 0;
                short_text
            },
            vec![0x00, 0x01, 0x00, 0xff],
            {
                let mut bad_utf8 = valid.clone();
                let name_at = 1 + 8 + 2;
                bad_utf8[name_at] = 0xff;
                bad_utf8
            },
        ];

        let nine = wide(9);
        let nine_bytes = encode_row(&nine, &vec![Value::Null; 9]).unwrap();
        let mut high_pad = nine_bytes;
        high_pad[1] |= 0x80;
        cases.push(high_pad);
        cases.push(vec![0xff, 0x03]);

        for bytes in cases {
            let err = decode_row(&schema, rid(), &bytes).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidData, "{bytes:?}");
            assert_eq!(err.to_string(), "invalid row: 1:4");
        }

        let nine_err = decode_row(&nine, rid(), &[0xff, 0x03]).unwrap_err();
        assert_eq!(nine_err.kind(), ErrorKind::InvalidData);
        assert_eq!(nine_err.to_string(), "invalid row: 1:4");
    }
}
