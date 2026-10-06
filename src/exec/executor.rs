//! Statement execution on top of table operations.
//!
//! `INSERT` and `UPDATE` evaluate and encode every candidate row before the
//! first write. `UPDATE` reads the old row for every assignment, so
//! `SET a = b, b = a` swaps. Rows are collected before they are changed, so
//! a row that moves to another page is not processed twice.

use std::io::{self, ErrorKind};

use crate::catalog::{ColumnType, Database, TableSchema};
use crate::row::{encode_row, Value};
use crate::sql::{
    Assignment, ColumnRef, CreateTable, Delete, Expr, Insert, Select, SelectItem, Statement, Update,
};

use super::eval::{bind_expr, column_not_found, eval, invalid, resolve_column, RowContext};

/// The outcome of one SQL statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryResult {
    /// `CREATE TABLE` succeeded.
    CreatedTable,
    /// Number of rows inserted.
    Inserted(u64),
    /// Number of rows updated.
    Updated(u64),
    /// Number of rows deleted.
    Deleted(u64),
    /// Column names and rows from a `SELECT`.
    Rows {
        /// Output names, in select-list order.
        columns: Vec<String>,
        /// One value list per row, aligned with `columns`.
        rows: Vec<Vec<Value>>,
    },
}

impl Database {
    /// Runs one statement.
    ///
    /// `WHERE` matches only `TRUE`. `FALSE` and `NULL` skip the row. Any
    /// other value is an error (`WHERE must be a boolean expression`).
    /// Column references in `SELECT`, `UPDATE`, and `DELETE` are resolved
    /// before rows are read, so a missing column fails even when the table
    /// is empty. `INSERT` values are evaluated with no row, so a column
    /// reference there is `column not found`.
    ///
    /// An unknown table is [`ErrorKind::NotFound`] (`table not found: {name}`).
    /// A bad column, value, or expression is [`ErrorKind::InvalidInput`].
    pub fn execute_statement(&mut self, statement: &Statement) -> io::Result<QueryResult> {
        match statement {
            Statement::CreateTable(statement) => self.execute_create(statement),
            Statement::Insert(statement) => self.execute_insert(statement),
            Statement::Select(statement) => self.execute_select(statement),
            Statement::Update(statement) => self.execute_update(statement),
            Statement::Delete(statement) => self.execute_delete(statement),
        }
    }

    /// Parses `sql` and runs each statement in order.
    ///
    /// Stops at the first error. Earlier statements stay applied. The vector
    /// is empty when `sql` has no statements.
    pub fn execute(&mut self, sql: &str) -> io::Result<Vec<QueryResult>> {
        let statements = crate::sql::parse(sql)?;
        let mut results = Vec::with_capacity(statements.len());
        for statement in &statements {
            results.push(self.execute_statement(statement)?);
        }
        Ok(results)
    }

    fn execute_create(&mut self, statement: &CreateTable) -> io::Result<QueryResult> {
        let columns: Vec<(&str, ColumnType)> = statement
            .columns
            .iter()
            .map(|column| (column.name.as_str(), column.column_type))
            .collect();
        self.create_table(&statement.name, &columns)?;
        Ok(QueryResult::CreatedTable)
    }

    fn execute_insert(&mut self, insert: &Insert) -> io::Result<QueryResult> {
        let schema = require_table(self, &insert.table)?;
        let targets = match &insert.columns {
            Some(names) => Some(insert_targets(&schema, names)?),
            None => None,
        };
        let expected = match &targets {
            Some(targets) => targets.len(),
            None => schema.columns.len(),
        };
        for row in &insert.rows {
            if row.len() != expected {
                return Err(invalid(format!(
                    "expected {expected} values, got {}",
                    row.len()
                )));
            }
        }
        let ctx = RowContext {
            table: &schema.name,
            columns: &schema.columns,
            values: None,
        };
        let mut pending = Vec::with_capacity(insert.rows.len());
        for row in &insert.rows {
            let mut evaluated = Vec::with_capacity(row.len());
            for expr in row {
                evaluated.push(eval(expr, &ctx)?);
            }
            let values = align_values(schema.columns.len(), targets.as_deref(), evaluated);
            encode_row(&schema, &values)?;
            pending.push(values);
        }
        for values in &pending {
            self.insert(&schema.name, values)?;
        }
        Ok(QueryResult::Inserted(pending.len() as u64))
    }

    fn execute_update(&mut self, update: &Update) -> io::Result<QueryResult> {
        let schema = require_table(self, &update.table)?;
        let targets = assignment_targets(&schema, &update.assignments)?;
        for assignment in &update.assignments {
            bind_expr(&schema.name, &schema.columns, &assignment.value)?;
        }
        if let Some(filter) = &update.filter {
            bind_expr(&schema.name, &schema.columns, filter)?;
        }
        let scanned = self.scan(&schema.name)?;
        let mut pending = Vec::new();
        for (id, old) in &scanned {
            let ctx = RowContext {
                table: &schema.name,
                columns: &schema.columns,
                values: Some(old),
            };
            if !matches_where(update.filter.as_ref(), &ctx)? {
                continue;
            }
            let mut new_values = old.clone();
            for (index, assignment) in targets.iter().zip(&update.assignments) {
                new_values[*index] = eval(&assignment.value, &ctx)?;
            }
            encode_row(&schema, &new_values)?;
            pending.push((*id, new_values));
        }
        for (id, values) in &pending {
            self.update(&schema.name, *id, values)?;
        }
        Ok(QueryResult::Updated(pending.len() as u64))
    }

    fn execute_delete(&mut self, delete: &Delete) -> io::Result<QueryResult> {
        let schema = require_table(self, &delete.table)?;
        if let Some(filter) = &delete.filter {
            bind_expr(&schema.name, &schema.columns, filter)?;
        }
        let scanned = self.scan(&schema.name)?;
        let mut ids = Vec::new();
        for (id, values) in &scanned {
            let ctx = RowContext {
                table: &schema.name,
                columns: &schema.columns,
                values: Some(values),
            };
            if matches_where(delete.filter.as_ref(), &ctx)? {
                ids.push(*id);
            }
        }
        for id in &ids {
            self.delete(&schema.name, *id)?;
        }
        Ok(QueryResult::Deleted(ids.len() as u64))
    }

    fn execute_select(&mut self, select: &Select) -> io::Result<QueryResult> {
        let schema = require_table(self, &select.from)?;
        let plan = select_plan(&schema, &select.items)?;
        for (_, expr) in &plan {
            bind_expr(&schema.name, &schema.columns, expr)?;
        }
        if let Some(filter) = &select.filter {
            bind_expr(&schema.name, &schema.columns, filter)?;
        }
        let scanned = self.scan(&schema.name)?;
        let mut rows = Vec::new();
        for (_, values) in &scanned {
            let ctx = RowContext {
                table: &schema.name,
                columns: &schema.columns,
                values: Some(values),
            };
            if !matches_where(select.filter.as_ref(), &ctx)? {
                continue;
            }
            let mut projected = Vec::with_capacity(plan.len());
            for (_, expr) in &plan {
                projected.push(eval(expr, &ctx)?);
            }
            rows.push(projected);
        }
        let columns = plan.into_iter().map(|(name, _)| name).collect();
        Ok(QueryResult::Rows { columns, rows })
    }
}

fn require_table(db: &Database, name: &str) -> io::Result<TableSchema> {
    db.table(name)
        .cloned()
        .ok_or_else(|| io::Error::new(ErrorKind::NotFound, format!("table not found: {name}")))
}

fn matches_where(filter: Option<&Expr>, ctx: &RowContext<'_>) -> io::Result<bool> {
    let Some(filter) = filter else {
        return Ok(true);
    };
    match eval(filter, ctx)? {
        Value::Boolean(true) => Ok(true),
        Value::Boolean(false) | Value::Null => Ok(false),
        _ => Err(invalid("WHERE must be a boolean expression")),
    }
}

fn insert_targets(schema: &TableSchema, names: &[String]) -> io::Result<Vec<usize>> {
    let mut targets = Vec::with_capacity(names.len());
    for name in names {
        let index = schema
            .columns
            .iter()
            .position(|column| column.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| column_not_found(name))?;
        if targets.contains(&index) {
            return Err(invalid(format!("duplicate column in INSERT: {name}")));
        }
        targets.push(index);
    }
    Ok(targets)
}

fn assignment_targets(schema: &TableSchema, assignments: &[Assignment]) -> io::Result<Vec<usize>> {
    let mut targets = Vec::with_capacity(assignments.len());
    for assignment in assignments {
        let index = schema
            .columns
            .iter()
            .position(|column| column.name.eq_ignore_ascii_case(&assignment.column))
            .ok_or_else(|| column_not_found(&assignment.column))?;
        if targets.contains(&index) {
            return Err(invalid(format!(
                "duplicate column in SET: {}",
                assignment.column
            )));
        }
        targets.push(index);
    }
    Ok(targets)
}

fn align_values(
    column_count: usize,
    targets: Option<&[usize]>,
    evaluated: Vec<Value>,
) -> Vec<Value> {
    let Some(targets) = targets else {
        return evaluated;
    };
    let mut values = vec![Value::Null; column_count];
    for (index, value) in targets.iter().zip(evaluated) {
        values[*index] = value;
    }
    values
}

fn select_plan(schema: &TableSchema, items: &[SelectItem]) -> io::Result<Vec<(String, Expr)>> {
    let mut plan = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard => {
                for column in &schema.columns {
                    plan.push((
                        column.name.clone(),
                        Expr::Column(ColumnRef {
                            table: None,
                            column: column.name.clone(),
                        }),
                    ));
                }
            }
            SelectItem::Expr { expr, alias } => {
                plan.push((output_name(schema, expr, alias)?, expr.clone()));
            }
        }
    }
    Ok(plan)
}

fn output_name(schema: &TableSchema, expr: &Expr, alias: &Option<String>) -> io::Result<String> {
    if let Some(alias) = alias {
        return Ok(alias.clone());
    }
    if let Expr::Column(reference) = expr {
        let index = resolve_column(&schema.name, &schema.columns, reference)?;
        return Ok(schema.columns[index].name.clone());
    }
    Ok(expr.to_string())
}

#[cfg(test)]
mod tests {
    use super::{Database, QueryResult};
    use crate::page::PageId;
    use crate::row::Value;
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
            path.push(format!("sqltoy-exec-{label}-{}-{nanos}", process::id()));
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

    fn open(label: &str) -> (TempDb, Database) {
        let db = TempDb::new(label);
        let database = Database::open(db.path()).unwrap();
        (db, database)
    }

    fn exec(db: &mut Database, sql: &str) -> Vec<QueryResult> {
        db.execute(sql).unwrap()
    }

    fn exec_err(db: &mut Database, sql: &str) -> (ErrorKind, String) {
        let err = db.execute(sql).unwrap_err();
        (err.kind(), err.to_string())
    }

    fn query(db: &mut Database, sql: &str) -> (Vec<String>, Vec<Vec<Value>>) {
        let mut results = exec(db, sql);
        match results.pop().unwrap() {
            QueryResult::Rows { columns, rows } => (columns, rows),
            other => panic!("expected rows, got {other:?}"),
        }
    }

    fn int(value: i64) -> Value {
        Value::Integer(value)
    }

    fn text(value: &str) -> Value {
        Value::Text(value.to_string())
    }

    #[test]
    fn create_insert_and_select() {
        let (_db, mut database) = open("milestone");
        let results = exec(
            &mut database,
            "CREATE TABLE users (id INTEGER, name TEXT); INSERT INTO users VALUES (1, 'Alice'); SELECT * FROM users",
        );
        assert_eq!(
            results,
            vec![
                QueryResult::CreatedTable,
                QueryResult::Inserted(1),
                QueryResult::Rows {
                    columns: vec!["id".to_string(), "name".to_string()],
                    rows: vec![vec![int(1), text("Alice")]],
                },
            ]
        );
    }

    #[test]
    fn insert_column_list_nulls_counts_and_duplicates() {
        let (_db, mut database) = open("insert");
        exec(
            &mut database,
            "CREATE TABLE users (id INTEGER, name TEXT, age INTEGER)",
        );
        assert_eq!(
            exec(
                &mut database,
                "INSERT INTO users (name, ID) VALUES ('Bob', 2), ('Carol', 1 + 2)"
            ),
            vec![QueryResult::Inserted(2)]
        );
        assert_eq!(
            exec(&mut database, "INSERT INTO users (name) VALUES ('Dana')"),
            vec![QueryResult::Inserted(1)]
        );
        let (_, rows) = query(&mut database, "SELECT id, name, age FROM users");
        assert_eq!(
            rows,
            vec![
                vec![int(2), text("Bob"), Value::Null],
                vec![int(3), text("Carol"), Value::Null],
                vec![Value::Null, text("Dana"), Value::Null],
            ]
        );

        assert_eq!(
            exec_err(&mut database, "INSERT INTO users (nope) VALUES (1)"),
            (
                ErrorKind::InvalidInput,
                "column not found: nope".to_string()
            )
        );
        assert_eq!(
            exec_err(&mut database, "INSERT INTO users (id, ID) VALUES (1, 2)"),
            (
                ErrorKind::InvalidInput,
                "duplicate column in INSERT: ID".to_string()
            )
        );
        assert_eq!(
            exec_err(&mut database, "INSERT INTO users (id) VALUES (1, 2)"),
            (
                ErrorKind::InvalidInput,
                "expected 1 values, got 2".to_string()
            )
        );
        assert_eq!(
            exec_err(&mut database, "INSERT INTO users VALUES (1)"),
            (
                ErrorKind::InvalidInput,
                "expected 3 values, got 1".to_string()
            )
        );
        assert_eq!(
            exec_err(&mut database, "INSERT INTO users VALUES (id, 'a', 1)"),
            (ErrorKind::InvalidInput, "column not found: id".to_string())
        );
        let (_, rows) = query(&mut database, "SELECT * FROM users");
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn insert_is_atomic_when_a_later_row_fails() {
        let (_db, mut database) = open("atomic");
        exec(&mut database, "CREATE TABLE users (id INTEGER, name TEXT)");
        exec(&mut database, "INSERT INTO users VALUES (1, 'ok')");
        assert_eq!(
            exec_err(
                &mut database,
                "INSERT INTO users VALUES (2, 'kept'), (3, 4)"
            ),
            (
                ErrorKind::InvalidInput,
                "type mismatch for column name: expected TEXT".to_string()
            )
        );
        assert_eq!(
            exec_err(
                &mut database,
                "INSERT INTO users VALUES (4, 'kept'), (TRUE, 'no')"
            ),
            (
                ErrorKind::InvalidInput,
                "type mismatch for column id: expected INTEGER".to_string()
            )
        );
        assert_eq!(
            exec_err(&mut database, "INSERT INTO users VALUES (5, 'kept'), (6)"),
            (
                ErrorKind::InvalidInput,
                "expected 2 values, got 1".to_string()
            )
        );
        let (_, rows) = query(&mut database, "SELECT * FROM users");
        assert_eq!(rows, vec![vec![int(1), text("ok")]]);
    }

    #[test]
    fn update_swaps_and_reads_old_values() {
        let (_db, mut database) = open("swap");
        exec(
            &mut database,
            "CREATE TABLE pair (a INTEGER, b INTEGER); INSERT INTO pair VALUES (1, 2), (3, 4)",
        );
        assert_eq!(
            exec(&mut database, "UPDATE pair SET a = b, b = a WHERE a = 1"),
            vec![QueryResult::Updated(1)]
        );
        assert_eq!(
            exec(
                &mut database,
                "UPDATE pair SET a = a + 1, b = a WHERE a = 3"
            ),
            vec![QueryResult::Updated(1)]
        );
        let (_, rows) = query(&mut database, "SELECT * FROM pair");
        assert_eq!(rows, vec![vec![int(2), int(1)], vec![int(4), int(3)]]);

        assert_eq!(
            exec_err(&mut database, "UPDATE pair SET a = 1, A = 2"),
            (
                ErrorKind::InvalidInput,
                "duplicate column in SET: A".to_string()
            )
        );
        assert_eq!(
            exec_err(&mut database, "UPDATE pair SET nope = 1"),
            (
                ErrorKind::InvalidInput,
                "column not found: nope".to_string()
            )
        );
        let (_, rows) = query(&mut database, "SELECT * FROM pair");
        assert_eq!(rows, vec![vec![int(2), int(1)], vec![int(4), int(3)]]);
    }

    #[test]
    fn update_is_atomic_when_a_later_row_overflows() {
        let (_db, mut database) = open("overflow");
        exec(
            &mut database,
            "CREATE TABLE nums (n INTEGER); INSERT INTO nums VALUES (1), (9223372036854775807)",
        );
        assert_eq!(
            exec_err(&mut database, "UPDATE nums SET n = n + 1"),
            (ErrorKind::InvalidInput, "integer overflow".to_string())
        );
        let (_, rows) = query(&mut database, "SELECT n FROM nums");
        assert_eq!(rows, vec![vec![int(1)], vec![int(i64::MAX)]]);
    }

    #[test]
    fn update_increments_each_row_once_including_rows_that_move() {
        let (_db, mut database) = open("move");
        exec(&mut database, "CREATE TABLE items (a INTEGER, note TEXT)");
        let long = "n".repeat(2000);
        for a in 1..=6 {
            let note = if a <= 2 { long.as_str() } else { "short" };
            exec(
                &mut database,
                &format!("INSERT INTO items VALUES ({a}, '{note}')"),
            );
        }
        let before = database.scan("items").unwrap();
        assert_eq!(before.len(), 6);
        assert_eq!(before[0].0.page_id, PageId(2));
        assert_eq!(before[1].0.page_id, PageId(2));

        let grown = "g".repeat(2500);
        assert_eq!(
            exec(
                &mut database,
                &format!("UPDATE items SET a = a + 1, note = '{grown}'")
            ),
            vec![QueryResult::Updated(6)]
        );
        let after = database.scan("items").unwrap();
        assert_eq!(after.len(), 6);
        let mut values: Vec<i64> = after
            .iter()
            .map(|(_, row)| match row[0] {
                Value::Integer(value) => value,
                _ => panic!("expected integer"),
            })
            .collect();
        values.sort();
        assert_eq!(values, vec![2, 3, 4, 5, 6, 7]);
        assert!(after.iter().all(|(_, row)| row[1] == text(&grown)));
        let before_ids: Vec<_> = before.iter().map(|(id, _)| *id).collect();
        assert!(after.iter().any(|(id, _)| !before_ids.contains(id)));
    }

    #[test]
    fn delete_where_and_null_filters() {
        let (_db, mut database) = open("delete");
        exec(
            &mut database,
            "CREATE TABLE t (id INTEGER, name TEXT); \
             INSERT INTO t VALUES (1, 'a'), (NULL, 'b'), (3, NULL), (4, 'd')",
        );
        assert_eq!(
            exec(&mut database, "DELETE FROM t WHERE id = NULL"),
            vec![QueryResult::Deleted(0)]
        );
        assert_eq!(
            exec(&mut database, "DELETE FROM t WHERE id IS NULL"),
            vec![QueryResult::Deleted(1)]
        );
        assert_eq!(
            exec(&mut database, "DELETE FROM t WHERE id > 1"),
            vec![QueryResult::Deleted(2)]
        );
        let (_, rows) = query(&mut database, "SELECT * FROM t");
        assert_eq!(rows, vec![vec![int(1), text("a")]]);

        exec(&mut database, "INSERT INTO t VALUES (2, 'b'), (NULL, 'c')");
        assert_eq!(
            exec(&mut database, "DELETE FROM t WHERE id = NULL OR name = 'a'"),
            vec![QueryResult::Deleted(1)]
        );
        let (_, rows) = query(&mut database, "SELECT name FROM t");
        assert_eq!(rows, vec![vec![text("b")], vec![text("c")]]);

        assert_eq!(
            exec_err(&mut database, "DELETE FROM t WHERE name"),
            (
                ErrorKind::InvalidInput,
                "WHERE must be a boolean expression".to_string()
            )
        );
        assert_eq!(
            exec_err(&mut database, "DELETE FROM t WHERE 1"),
            (
                ErrorKind::InvalidInput,
                "WHERE must be a boolean expression".to_string()
            )
        );
        let (_, rows) = query(&mut database, "SELECT * FROM t");
        assert_eq!(rows.len(), 2);
        assert_eq!(
            exec(&mut database, "DELETE FROM t"),
            vec![QueryResult::Deleted(2)]
        );
        let (_, rows) = query(&mut database, "SELECT * FROM t");
        assert!(rows.is_empty());
    }

    #[test]
    fn select_expressions_aliases_booleans_and_names() {
        let (_db, mut database) = open("select");
        exec(
            &mut database,
            "CREATE TABLE Users (Id INTEGER, Name TEXT); \
             INSERT INTO Users VALUES (1, 'Alice'), (NULL, NULL)",
        );
        let (columns, rows) = query(
            &mut database,
            "SELECT Id AS user_id, Name n, id + 1, 1 = 1, name IS NULL, Users.Id \
             FROM users WHERE Id IS NOT NULL OR Name IS NULL",
        );
        assert_eq!(
            columns,
            vec![
                "user_id".to_string(),
                "n".to_string(),
                "(id + 1)".to_string(),
                "(1 = 1)".to_string(),
                "(name IS NULL)".to_string(),
                "Id".to_string(),
            ]
        );
        assert_eq!(
            rows,
            vec![
                vec![
                    int(1),
                    text("Alice"),
                    int(2),
                    Value::Boolean(true),
                    Value::Boolean(false),
                    int(1),
                ],
                vec![
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Boolean(true),
                    Value::Boolean(true),
                    Value::Null,
                ],
            ]
        );

        let (columns, rows) = query(&mut database, "SELECT * FROM users WHERE Id = 1");
        assert_eq!(columns, vec!["Id".to_string(), "Name".to_string()]);
        assert_eq!(rows, vec![vec![int(1), text("Alice")]]);

        let (columns, rows) = query(&mut database, "SELECT id + 1, * FROM Users WHERE FALSE");
        assert_eq!(
            columns,
            vec!["(id + 1)".to_string(), "Id".to_string(), "Name".to_string()]
        );
        assert!(rows.is_empty());

        exec(&mut database, "CREATE TABLE empty (id INTEGER, name TEXT)");
        let (columns, rows) = query(&mut database, "SELECT * FROM empty");
        assert_eq!(columns, vec!["id".to_string(), "name".to_string()]);
        assert!(rows.is_empty());
        assert_eq!(
            exec_err(&mut database, "SELECT missing FROM empty"),
            (
                ErrorKind::InvalidInput,
                "column not found: missing".to_string()
            )
        );
        assert_eq!(
            exec_err(&mut database, "SELECT other.id FROM Users"),
            (
                ErrorKind::InvalidInput,
                "column not found: other.id".to_string()
            )
        );
        assert_eq!(
            exec_err(&mut database, "DELETE FROM empty WHERE missing = 1"),
            (
                ErrorKind::InvalidInput,
                "column not found: missing".to_string()
            )
        );
    }

    #[test]
    fn multi_statement_stops_at_the_first_error() {
        let (_db, mut database) = open("multi");
        let (kind, message) = exec_err(
            &mut database,
            "CREATE TABLE t (id INTEGER, name TEXT); \
             INSERT INTO t VALUES (1, 'ok'); \
             INSERT INTO t VALUES (2, 'no'), (3, 4); \
             INSERT INTO t VALUES (9, 'skipped')",
        );
        assert_eq!(kind, ErrorKind::InvalidInput);
        assert_eq!(message, "type mismatch for column name: expected TEXT");
        let (_, rows) = query(&mut database, "SELECT * FROM t");
        assert_eq!(rows, vec![vec![int(1), text("ok")]]);

        assert!(database.table("u").is_none());
        let err = database
            .execute("CREATE TABLE u (id INTEGER); SELECT")
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert!(err.to_string().starts_with("syntax error"));
        assert!(database.table("u").is_none());

        assert_eq!(
            exec_err(&mut database, "SELECT * FROM missing"),
            (ErrorKind::NotFound, "table not found: missing".to_string())
        );
        assert_eq!(exec(&mut database, ""), Vec::<QueryResult>::new());
        assert_eq!(exec(&mut database, ";"), Vec::<QueryResult>::new());
    }

    #[test]
    fn where_null_skips_every_row() {
        let (_db, mut database) = open("where");
        exec(
            &mut database,
            "CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1), (2)",
        );
        assert_eq!(
            exec(&mut database, "UPDATE t SET id = 0 WHERE NULL"),
            vec![QueryResult::Updated(0)]
        );
        assert_eq!(
            exec(&mut database, "DELETE FROM t WHERE NULL OR FALSE"),
            vec![QueryResult::Deleted(0)]
        );
        let (_, rows) = query(&mut database, "SELECT * FROM t WHERE TRUE OR NULL");
        assert_eq!(rows, vec![vec![int(1)], vec![int(2)]]);
    }
}
