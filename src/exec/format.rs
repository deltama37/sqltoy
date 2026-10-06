//! Text form of a [`QueryResult`](super::QueryResult).
//!
//! Select results are a fixed-width table. Headers are left-aligned. Integers
//! are right-aligned and every other value is left-aligned. Text is printed
//! as stored, with no quotes and no escaping, so a newline inside a value
//! splits that row across lines. `NULL` is `NULL` and booleans are `TRUE` or
//! `FALSE`.

use crate::row::Value;

use super::executor::QueryResult;

/// Formats `result` as the text the CLI prints, without a trailing newline.
///
/// Status results are `CREATE TABLE`, `INSERT n`, `UPDATE n`, `DELETE n`,
/// `BEGIN`, `COMMIT`, and `ROLLBACK`.
/// A select result is a table. Each cell is one leading space, the value
/// padded to the column width, and one trailing space. Cells are joined with
/// `|`. The separator under the header is `-` repeated `width + 2` times,
/// joined with `+`. Widths count Unicode scalar values, not bytes. The last
/// line is `(0 rows)`, `(1 row)`, or `(n rows)`. Zero rows still include the
/// header and the separator.
pub fn format_result(result: &QueryResult) -> String {
    match result {
        QueryResult::CreatedTable => "CREATE TABLE".to_string(),
        QueryResult::Inserted(count) => format!("INSERT {count}"),
        QueryResult::Updated(count) => format!("UPDATE {count}"),
        QueryResult::Deleted(count) => format!("DELETE {count}"),
        QueryResult::Begin => "BEGIN".to_string(),
        QueryResult::Commit => "COMMIT".to_string(),
        QueryResult::Rollback => "ROLLBACK".to_string(),
        QueryResult::Vacuumed(count) => format!("VACUUM {count}"),
        QueryResult::Rows { columns, rows } => format_table(columns, rows),
    }
}

fn format_table(columns: &[String], rows: &[Vec<Value>]) -> String {
    let widths = column_widths(columns, rows);
    let mut lines = Vec::with_capacity(rows.len() + 3);
    let header: Vec<Cell> = columns
        .iter()
        .map(|name| Cell {
            text: name.clone(),
            align_right: false,
        })
        .collect();
    lines.push(render(&header, &widths));
    lines.push(separator(&widths));
    for row in rows {
        let cells = (0..columns.len())
            .map(|index| match row.get(index) {
                Some(value) => Cell {
                    text: cell_text(value),
                    align_right: matches!(value, Value::Integer(_)),
                },
                None => Cell {
                    text: String::new(),
                    align_right: false,
                },
            })
            .collect::<Vec<_>>();
        lines.push(render(&cells, &widths));
    }
    lines.push(row_count(rows.len()));
    lines.join("\n")
}

struct Cell {
    text: String,
    align_right: bool,
}

fn column_widths(columns: &[String], rows: &[Vec<Value>]) -> Vec<usize> {
    let mut widths: Vec<usize> = columns.iter().map(|name| name.chars().count()).collect();
    for row in rows {
        for (index, value) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(index) {
                *width = (*width).max(cell_text(value).chars().count());
            }
        }
    }
    widths
}

fn cell_text(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Boolean(true) => "TRUE".to_string(),
        Value::Boolean(false) => "FALSE".to_string(),
        Value::Integer(value) => value.to_string(),
        Value::Text(value) => value.clone(),
    }
}

fn render(cells: &[Cell], widths: &[usize]) -> String {
    cells
        .iter()
        .zip(widths)
        .map(|(cell, width)| {
            let padded = pad(&cell.text, *width, cell.align_right);
            format!(" {padded} ")
        })
        .collect::<Vec<_>>()
        .join("|")
}

fn separator(widths: &[usize]) -> String {
    widths
        .iter()
        .map(|width| "-".repeat(width + 2))
        .collect::<Vec<_>>()
        .join("+")
}

fn pad(text: &str, width: usize, align_right: bool) -> String {
    let extra = width.saturating_sub(text.chars().count());
    let spaces = " ".repeat(extra);
    if align_right {
        format!("{spaces}{text}")
    } else {
        format!("{text}{spaces}")
    }
}

fn row_count(count: usize) -> String {
    if count == 1 {
        "(1 row)".to_string()
    } else {
        format!("({count} rows)")
    }
}

#[cfg(test)]
mod tests {
    use super::format_result;
    use crate::exec::QueryResult;
    use crate::row::Value;

    fn rows(columns: &[&str], rows: Vec<Vec<Value>>) -> QueryResult {
        QueryResult::Rows {
            columns: columns.iter().map(|name| (*name).to_string()).collect(),
            rows,
        }
    }

    #[test]
    fn status_lines() {
        assert_eq!(format_result(&QueryResult::CreatedTable), "CREATE TABLE");
        assert_eq!(format_result(&QueryResult::Inserted(0)), "INSERT 0");
        assert_eq!(format_result(&QueryResult::Inserted(1)), "INSERT 1");
        assert_eq!(format_result(&QueryResult::Updated(2)), "UPDATE 2");
        assert_eq!(format_result(&QueryResult::Deleted(3)), "DELETE 3");
        assert_eq!(format_result(&QueryResult::Begin), "BEGIN");
        assert_eq!(format_result(&QueryResult::Commit), "COMMIT");
        assert_eq!(format_result(&QueryResult::Rollback), "ROLLBACK");
        assert_eq!(format_result(&QueryResult::Vacuumed(4)), "VACUUM 4");
    }

    #[test]
    fn table_matches_the_milestone_layout() {
        let result = rows(
            &["id", "name"],
            vec![
                vec![Value::Integer(1), Value::Text("Alice".to_string())],
                vec![Value::Integer(2), Value::Null],
            ],
        );
        assert_eq!(
            format_result(&result),
            " id | name  \n----+-------\n  1 | Alice \n  2 | NULL  \n(2 rows)"
        );
    }

    #[test]
    fn alignment_counts_chars_and_keeps_padding() {
        let mixed = rows(
            &["n"],
            vec![
                vec![Value::Integer(12)],
                vec![Value::Integer(-7)],
                vec![Value::Null],
            ],
        );
        assert_eq!(
            format_result(&mixed),
            " n    \n------\n   12 \n   -7 \n NULL \n(3 rows)"
        );

        let flag = rows(
            &["flag"],
            vec![
                vec![Value::Boolean(true)],
                vec![Value::Boolean(false)],
                vec![Value::Null],
            ],
        );
        assert_eq!(
            format_result(&flag),
            " flag  \n-------\n TRUE  \n FALSE \n NULL  \n(3 rows)"
        );

        let text = rows(
            &["name"],
            vec![
                vec![Value::Text("it's".to_string())],
                vec![Value::Text("NULL".to_string())],
                vec![Value::Text("雪".to_string())],
            ],
        );
        assert_eq!(
            format_result(&text),
            " name \n------\n it's \n NULL \n 雪    \n(3 rows)"
        );

        let broken = rows(&["note"], vec![vec![Value::Text("a\nb".to_string())]]);
        assert_eq!(format_result(&broken), " note \n------\n a\nb  \n(1 row)");
    }

    #[test]
    fn zero_rows_still_print_the_header() {
        let result = rows(&["id", "name"], Vec::new());
        assert_eq!(format_result(&result), " id | name \n----+------\n(0 rows)");
        let one = rows(&["id"], vec![vec![Value::Integer(1)]]);
        assert_eq!(format_result(&one), " id \n----\n  1 \n(1 row)");
    }
}
