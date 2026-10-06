//! Statement execution on top of table operations.
//!
//! `INSERT` and `UPDATE` evaluate and encode every candidate row before the
//! first write. `UPDATE` reads the old row for every assignment, so
//! `SET a = b, b = a` swaps. Rows are collected before they are changed, so
//! a row that moves to another page is not processed twice. `SELECT` pulls
//! from an operator tree.

use std::io::{self, ErrorKind};

use crate::catalog::{ColumnSpec, Database, TableSchema};
use crate::record::RecordId;
use crate::row::Value;
use crate::sql::{Assignment, CreateTable, Delete, Expr, Insert, Select, Statement, Update};

use super::eval::{bind_expr, column_not_found, eval, invalid, Binding, RowContext};
use super::operator::instantiate;
use super::plan::{compile_modify, compile_select};

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
    /// `SELECT` pulls from a scan, then joins, filter, project, sort, and
    /// limit. `ORDER BY` is stable. `LIMIT` stops the scan once it has
    /// enough rows. `UPDATE` and `DELETE` use the same scan and filter to
    /// choose rows.
    ///
    /// An unknown table is [`ErrorKind::NotFound`] (`table not found: {name}`).
    /// A bad column, value, or expression is [`ErrorKind::InvalidInput`].
    ///
    /// The buffer pool is flushed when the statement returns, including when
    /// it fails. A flush failure after that error is discarded.
    pub fn execute_statement(&mut self, statement: &Statement) -> io::Result<QueryResult> {
        let result = match statement {
            Statement::CreateTable(statement) => self.execute_create(statement),
            Statement::Insert(statement) => self.execute_insert(statement),
            Statement::Select(statement) => self.execute_select(statement),
            Statement::Update(statement) => self.execute_update(statement),
            Statement::Delete(statement) => self.execute_delete(statement),
        };
        self.persist(result)
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
        let columns: Vec<ColumnSpec> = statement
            .columns
            .iter()
            .map(|column| ColumnSpec {
                name: column.name.as_str(),
                column_type: column.column_type,
                primary_key: column.primary_key,
            })
            .collect();
        self.create_table_unflushed(&statement.name, &columns)?;
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
        let binding = Binding {
            name: schema.name.as_str(),
            columns: schema.columns.as_slice(),
            values: None,
        };
        let ctx = RowContext {
            bindings: &[binding],
        };
        let mut pending = Vec::with_capacity(insert.rows.len());
        for row in &insert.rows {
            let mut evaluated = Vec::with_capacity(row.len());
            for expr in row {
                evaluated.push(eval(expr, &ctx)?);
            }
            let values = align_values(schema.columns.len(), targets.as_deref(), evaluated);
            pending.push(values);
        }
        let inserted = self.insert_all_unflushed(&schema.name, &pending)?;
        Ok(QueryResult::Inserted(inserted.len() as u64))
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
        let scanned = self.matching_rows(&schema, update.filter.as_ref())?;
        let mut pending = Vec::new();
        for (id, old) in &scanned {
            let binding = Binding {
                name: schema.name.as_str(),
                columns: schema.columns.as_slice(),
                values: Some(old),
            };
            let ctx = RowContext {
                bindings: &[binding],
            };
            let mut new_values = old.clone();
            for (index, assignment) in targets.iter().zip(&update.assignments) {
                new_values[*index] = eval(&assignment.value, &ctx)?;
            }
            pending.push((*id, new_values));
        }
        let updated = self.apply_update_unflushed(&schema.name, &pending)?;
        Ok(QueryResult::Updated(updated.len() as u64))
    }

    fn execute_delete(&mut self, delete: &Delete) -> io::Result<QueryResult> {
        let schema = require_table(self, &delete.table)?;
        if let Some(filter) = &delete.filter {
            bind_expr(&schema.name, &schema.columns, filter)?;
        }
        let scanned = self.matching_rows(&schema, delete.filter.as_ref())?;
        let mut ids = Vec::new();
        for (id, _) in &scanned {
            ids.push(*id);
        }
        for id in &ids {
            self.delete_unflushed(&schema.name, *id)?;
        }
        Ok(QueryResult::Deleted(ids.len() as u64))
    }

    fn execute_select(&mut self, select: &Select) -> io::Result<QueryResult> {
        let compiled = compile_select(self, select)?;
        let mut operator = instantiate(compiled.plan);
        let mut rows = Vec::new();
        while let Some(tuple) = operator.next(self)? {
            rows.push(tuple.projected);
        }
        Ok(QueryResult::Rows {
            columns: compiled.columns,
            rows,
        })
    }

    /// Operator tree for a `SELECT`, one line per operator.
    ///
    /// `EXPLAIN` will print this. The tree is the same one [`Self::execute_statement`]
    /// runs.
    pub fn describe_select(&self, select: &Select) -> io::Result<String> {
        Ok(compile_select(self, select)?.plan.describe())
    }

    fn matching_rows(
        &mut self,
        schema: &TableSchema,
        filter: Option<&Expr>,
    ) -> io::Result<Vec<(RecordId, Vec<Value>)>> {
        let plan = compile_modify(schema, filter)?;
        let mut operator = instantiate(plan);
        let mut rows = Vec::new();
        while let Some(tuple) = operator.next(self)? {
            let slot = &tuple.bindings[0];
            let id = slot.id.ok_or_else(|| invalid("missing record id"))?;
            rows.push((id, slot.values.clone()));
        }
        Ok(rows)
    }
}

fn require_table(db: &Database, name: &str) -> io::Result<TableSchema> {
    db.table(name)
        .cloned()
        .ok_or_else(|| io::Error::new(ErrorKind::NotFound, format!("table not found: {name}")))
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

    #[test]
    fn indexed_queries_match_a_table_without_a_primary_key() {
        let (_db, mut database) = open("cmp");
        exec(
            &mut database,
            "CREATE TABLE pked (id INTEGER PRIMARY KEY, name TEXT); \
             CREATE TABLE plain (id INTEGER, name TEXT)",
        );
        let ids = [-20_i64, -1, 0, 1, 2, 5, 8, 9, 10, 11, 40, i64::MAX];
        let values = ids
            .iter()
            .map(|id| format!("({id}, 'n{id}')"))
            .collect::<Vec<_>>()
            .join(", ");
        exec(
            &mut database,
            &format!("INSERT INTO pked VALUES {values}; INSERT INTO plain VALUES {values}"),
        );
        let queries = [
            "SELECT * FROM {t} WHERE id = 5",
            "SELECT * FROM {t} WHERE 5 = id",
            "SELECT name FROM {t} WHERE {t}.id = 5",
            "SELECT * FROM {t} WHERE id = 2 + 3",
            "SELECT * FROM {t} WHERE id = - 1",
            "SELECT * FROM {t} WHERE id = 5 AND name = 'n5'",
            "SELECT * FROM {t} WHERE name = 'n5' AND id = 5",
            "SELECT * FROM {t} WHERE id = 5 OR name = 'n1'",
            "SELECT * FROM {t} WHERE id > 3 AND id < 11",
            "SELECT * FROM {t} WHERE id = NULL",
            "SELECT * FROM {t} WHERE id = 999",
            "SELECT * FROM {t}",
            "SELECT * FROM {t} WHERE id = 1 + 2 * 2",
            "SELECT id FROM {t} WHERE name = 'n10' AND id = 10 AND 1 = 1",
        ];
        for sql in queries {
            let pk_sql = sql.replace("{t}", "pked");
            let plain_sql = sql.replace("{t}", "plain");
            let (_, pk_rows) = query(&mut database, &pk_sql);
            let (_, plain_rows) = query(&mut database, &plain_sql);
            assert_eq!(pk_rows, plain_rows, "{pk_sql}");
        }
        for sql in [
            "SELECT * FROM {t} WHERE id = 'x'",
            "SELECT * FROM {t} WHERE id = TRUE",
            "SELECT * FROM {t} WHERE id = 1 / 0",
            "UPDATE {t} SET name = 'z' WHERE id = 'x'",
            "DELETE FROM {t} WHERE id = TRUE",
        ] {
            let pk_err = exec_err(&mut database, &sql.replace("{t}", "pked"));
            let plain_err = exec_err(&mut database, &sql.replace("{t}", "plain"));
            assert_eq!(pk_err, plain_err, "{sql}");
        }

        exec(
            &mut database,
            "UPDATE pked SET name = 'z' WHERE id = 5; \
             UPDATE plain SET name = 'z' WHERE id = 5; \
             DELETE FROM pked WHERE id = 8; \
             DELETE FROM plain WHERE id = 8",
        );
        let (_, pk_rows) = query(&mut database, "SELECT * FROM pked");
        let (_, plain_rows) = query(&mut database, "SELECT * FROM plain");
        assert_eq!(pk_rows, plain_rows);
    }

    #[test]
    fn primary_key_insert_update_and_delete_rules() {
        let db = TempDb::new("pkrules");
        {
            let mut database = Database::open(db.path()).unwrap();
            exec(
                &mut database,
                "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)",
            );
            assert_eq!(
                exec_err(&mut database, "INSERT INTO users VALUES (1, 'a'), (1, 'b')"),
                (
                    ErrorKind::InvalidInput,
                    "duplicate primary key: 1".to_string()
                )
            );
            assert_eq!(
                exec_err(&mut database, "INSERT INTO users VALUES (NULL, 'a')"),
                (
                    ErrorKind::InvalidInput,
                    "NULL in primary key column: id".to_string()
                )
            );
            assert_eq!(
                exec_err(&mut database, "INSERT INTO users (name) VALUES ('a')"),
                (
                    ErrorKind::InvalidInput,
                    "NULL in primary key column: id".to_string()
                )
            );
            let (_, rows) = query(&mut database, "SELECT * FROM users");
            assert!(rows.is_empty());

            exec(&mut database, "INSERT INTO users VALUES (1, 'a')");
            assert_eq!(
                exec_err(&mut database, "INSERT INTO users VALUES (2, 'b'), (1, 'c')"),
                (
                    ErrorKind::InvalidInput,
                    "duplicate primary key: 1".to_string()
                )
            );
            let (_, rows) = query(&mut database, "SELECT * FROM users");
            assert_eq!(rows, vec![vec![int(1), text("a")]]);

            exec(
                &mut database,
                "INSERT INTO users VALUES (2, 'b'), (3, 'c'), (4, 'd')",
            );
            assert_eq!(
                exec(&mut database, "UPDATE users SET id = id + 1"),
                vec![QueryResult::Updated(4)]
            );
            let (_, rows) = query(&mut database, "SELECT id FROM users");
            assert_eq!(
                rows,
                vec![vec![int(2)], vec![int(3)], vec![int(4)], vec![int(5)]]
            );

            assert_eq!(
                exec_err(&mut database, "UPDATE users SET id = 2 WHERE id = 5"),
                (
                    ErrorKind::InvalidInput,
                    "duplicate primary key: 2".to_string()
                )
            );
            assert_eq!(
                exec_err(&mut database, "UPDATE users SET id = 9"),
                (
                    ErrorKind::InvalidInput,
                    "duplicate primary key: 9".to_string()
                )
            );
            let (_, rows) = query(&mut database, "SELECT id, name FROM users");
            assert_eq!(
                rows,
                vec![
                    vec![int(2), text("a")],
                    vec![int(3), text("b")],
                    vec![int(4), text("c")],
                    vec![int(5), text("d")],
                ]
            );

            exec(
                &mut database,
                "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)",
            );
            let wide = "n".repeat(2000);
            exec(
                &mut database,
                &format!("INSERT INTO notes VALUES (1, '{wide}'), (2, '{wide}')"),
            );
            let before = database.scan("notes").unwrap()[0].0;
            let grown = "g".repeat(2500);
            exec(
                &mut database,
                &format!("UPDATE notes SET body = '{grown}' WHERE id = 1"),
            );
            let schema = database.table("notes").unwrap().clone();
            let notes = crate::btree::BTree::open(schema.index_root.unwrap(), schema.id);
            let indexed = notes.get(database.records.pages_mut(), 1).unwrap().unwrap();
            assert_ne!(indexed, before);
            assert_eq!(database.get("notes", indexed).unwrap()[1], text(&grown));
            let (_, rows) = query(&mut database, "SELECT body FROM notes WHERE id = 1");
            assert_eq!(rows, vec![vec![text(&grown)]]);

            let schema = database.table("users").unwrap().clone();
            let tree = crate::btree::BTree::open(schema.index_root.unwrap(), schema.id);
            exec(&mut database, "DELETE FROM users WHERE id = 2");
            assert!(tree.get(database.records.pages_mut(), 2).unwrap().is_none());
            exec(&mut database, "INSERT INTO users VALUES (2, 'again')");
            let (_, rows) = query(&mut database, "SELECT name FROM users WHERE id = 2");
            assert_eq!(rows, vec![vec![text("again")]]);
        }

        let mut database = Database::open(db.path()).unwrap();
        let (_, rows) = query(&mut database, "SELECT name FROM users WHERE users.id = 2");
        assert_eq!(rows, vec![vec![text("again")]]);
        assert!(database.index_height("users").unwrap().unwrap() >= 1);
    }

    #[test]
    fn primary_key_lookup_reads_few_pages_on_a_wide_table() {
        let (_db, mut database) = open("wide");
        exec(
            &mut database,
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)",
        );
        let note = "n".repeat(3000);
        for id in 1..=260 {
            exec(
                &mut database,
                &format!("INSERT INTO users VALUES ({id}, '{note}')"),
            );
        }
        let height = database.index_height("users").unwrap().unwrap();
        assert!(height >= 2, "height {height}");

        let before = database.pages_read();
        let (_, rows) = query(&mut database, "SELECT id FROM users WHERE id = 123");
        let lookup = database.pages_read() - before;
        assert_eq!(rows, vec![vec![int(123)]]);
        assert!(
            lookup <= u64::from(height) + 2,
            "lookup read {lookup} pages at height {height}"
        );

        let before = database.pages_read();
        let (_, rows) = query(
            &mut database,
            "SELECT id FROM users WHERE users.id = 200 AND name = 'missing'",
        );
        let filtered = database.pages_read() - before;
        assert!(rows.is_empty());
        assert!(filtered <= u64::from(height) + 2);

        let before = database.pages_read();
        let _ = query(&mut database, "SELECT id FROM users");
        let scan = database.pages_read() - before;
        assert!(scan > 200, "scan read {scan}");
        assert!(lookup * 20 < scan);

        let before = database.pages_read();
        let err = database.execute("SELECT * FROM users WHERE id = NULL");
        assert!(err.is_ok());
        let null_lookup = database.pages_read() - before;
        assert!(
            null_lookup > 200,
            "NULL constant should scan, read {null_lookup}"
        );

        let before = database.pages_read();
        let err = database
            .execute("SELECT * FROM users WHERE id = 'x'")
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        let text_lookup = database.pages_read() - before;
        assert!(
            text_lookup > 200,
            "text constant should scan, read {text_lookup}"
        );

        let before = database.pages_read();
        exec(&mut database, "DELETE FROM users WHERE id = 50");
        let deleted = database.pages_read() - before;
        assert!(
            deleted < scan / 2,
            "delete read {deleted}, scan read {scan}"
        );
        exec(&mut database, "INSERT INTO users VALUES (50, 'back')");
        let (_, rows) = query(&mut database, "SELECT name FROM users WHERE id = 50");
        assert_eq!(rows, vec![vec![text("back")]]);
    }

    #[test]
    fn order_by_nulls_alias_position_expression_and_stability() {
        let (_db, mut database) = open("order");
        exec(
            &mut database,
            "CREATE TABLE items (id INTEGER, name TEXT, n INTEGER); \
             INSERT INTO items VALUES \
               (1, 'a', 2), \
               (2, 'b', 1), \
               (3, 'a', NULL), \
               (4, NULL, 1), \
               (5, 'a', 2)",
        );
        let (_, rows) = query(
            &mut database,
            "SELECT id FROM items ORDER BY name ASC, n DESC, id ASC",
        );
        assert_eq!(
            rows,
            vec![
                vec![int(4)],
                vec![int(1)],
                vec![int(5)],
                vec![int(3)],
                vec![int(2)],
            ]
        );
        let (_, rows) = query(&mut database, "SELECT id FROM items ORDER BY name DESC, id");
        assert_eq!(
            rows,
            vec![
                vec![int(2)],
                vec![int(1)],
                vec![int(3)],
                vec![int(5)],
                vec![int(4)],
            ]
        );

        let (_, rows) = query(
            &mut database,
            "SELECT name AS n, id FROM items ORDER BY n, id",
        );
        assert_eq!(rows[0], vec![Value::Null, int(4)]);
        assert_eq!(rows[1][1], int(1));

        let (_, rows) = query(&mut database, "SELECT name, id FROM items ORDER BY 2 DESC");
        assert_eq!(rows[0][1], int(5));
        assert_eq!(rows[4][1], int(1));

        let (_, rows) = query(&mut database, "SELECT name FROM items ORDER BY id DESC");
        assert_eq!(
            rows,
            vec![
                vec![text("a")],
                vec![Value::Null],
                vec![text("a")],
                vec![text("b")],
                vec![text("a")],
            ]
        );

        let (_, rows) = query(&mut database, "SELECT - id AS id FROM items ORDER BY id");
        assert_eq!(
            rows,
            vec![
                vec![int(-5)],
                vec![int(-4)],
                vec![int(-3)],
                vec![int(-2)],
                vec![int(-1)]
            ]
        );

        let (_, rows) = query(
            &mut database,
            "SELECT id FROM items WHERE name = 'a' ORDER BY name",
        );
        assert_eq!(rows, vec![vec![int(1)], vec![int(3)], vec![int(5)]]);

        assert_eq!(
            exec_err(&mut database, "SELECT id FROM items ORDER BY 0"),
            (
                ErrorKind::InvalidInput,
                "ORDER BY position out of range: 0".to_string()
            )
        );
        assert_eq!(
            exec_err(&mut database, "SELECT id FROM items ORDER BY 3"),
            (
                ErrorKind::InvalidInput,
                "ORDER BY position out of range: 3".to_string()
            )
        );
        assert_eq!(
            exec_err(&mut database, "SELECT id FROM items ORDER BY missing"),
            (
                ErrorKind::InvalidInput,
                "column not found: missing".to_string()
            )
        );
    }

    #[test]
    fn limit_offset_and_early_stop() {
        let (_db, mut database) = open("limit");
        exec(
            &mut database,
            "CREATE TABLE t (id INTEGER, name TEXT); \
             INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd')",
        );
        let (columns, rows) = query(
            &mut database,
            "SELECT id FROM t ORDER BY id DESC LIMIT 2 OFFSET 1",
        );
        assert_eq!(columns, vec!["id".to_string()]);
        assert_eq!(rows, vec![vec![int(3)], vec![int(2)]]);

        let (_, rows) = query(&mut database, "SELECT id FROM t LIMIT 0");
        assert!(rows.is_empty());
        let (_, rows) = query(&mut database, "SELECT id FROM t LIMIT 10 OFFSET 100");
        assert!(rows.is_empty());
        let (_, rows) = query(&mut database, "SELECT id FROM t LIMIT 1 OFFSET 0");
        assert_eq!(rows, vec![vec![int(1)]]);
        let (_, rows) = query(&mut database, "SELECT id FROM t LIMIT 1 + 1 OFFSET 2");
        assert_eq!(rows, vec![vec![int(3)], vec![int(4)]]);

        assert_eq!(
            exec_err(&mut database, "SELECT id FROM t LIMIT - 1"),
            (
                ErrorKind::InvalidInput,
                "LIMIT must be a non-negative integer".to_string()
            )
        );
        assert_eq!(
            exec_err(&mut database, "SELECT id FROM t LIMIT 'a'"),
            (
                ErrorKind::InvalidInput,
                "LIMIT must be a non-negative integer".to_string()
            )
        );
        assert_eq!(
            exec_err(&mut database, "SELECT id FROM t LIMIT TRUE"),
            (
                ErrorKind::InvalidInput,
                "LIMIT must be a non-negative integer".to_string()
            )
        );
        assert_eq!(
            exec_err(&mut database, "SELECT id FROM t LIMIT 1 OFFSET - 1"),
            (
                ErrorKind::InvalidInput,
                "OFFSET must be a non-negative integer".to_string()
            )
        );
        assert_eq!(
            exec_err(&mut database, "SELECT id FROM empty LIMIT 1 / 0"),
            (ErrorKind::NotFound, "table not found: empty".to_string())
        );
        exec(&mut database, "CREATE TABLE empty (id INTEGER)");
        assert_eq!(
            exec_err(&mut database, "SELECT id FROM empty LIMIT 1 / 0"),
            (ErrorKind::InvalidInput, "division by zero".to_string())
        );

        let (_wide, mut wide_db) = open("wide-limit");
        exec(&mut wide_db, "CREATE TABLE wide (id INTEGER, note TEXT)");
        let note = "n".repeat(3000);
        for id in 1..=40 {
            exec(
                &mut wide_db,
                &format!("INSERT INTO wide VALUES ({id}, '{note}')"),
            );
        }
        let before = wide_db.pages_read();
        let (_, rows) = query(&mut wide_db, "SELECT id FROM wide LIMIT 1");
        let limited = wide_db.pages_read() - before;
        assert_eq!(rows, vec![vec![int(1)]]);
        assert!(limited <= 2, "LIMIT 1 read {limited} pages");

        let before = wide_db.pages_read();
        let (_, rows) = query(&mut wide_db, "SELECT id FROM wide");
        let scanned = wide_db.pages_read() - before;
        assert_eq!(rows.len(), 40);
        assert!(scanned > 30, "full scan read {scanned}");
        assert!(limited * 10 < scanned, "limit {limited}, scan {scanned}");
    }

    #[test]
    fn joins_names_and_left_join_index_rule() {
        let (_db, mut database) = open("join");
        exec(
            &mut database,
            "CREATE TABLE a (id INTEGER PRIMARY KEY, x INTEGER, n TEXT); \
             CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER, n TEXT); \
             CREATE TABLE bplain (id INTEGER, a_id INTEGER, n TEXT); \
             INSERT INTO a VALUES (1, 5, 'a1'), (2, 9, 'a2'), (3, NULL, 'a3'); \
             INSERT INTO b VALUES (5, 1, 'b5'), (6, 1, 'b6'), (7, 2, 'b7'); \
             INSERT INTO bplain VALUES (5, 1, 'b5'), (6, 1, 'b6'), (7, 2, 'b7')",
        );

        let (_, inner) = query(
            &mut database,
            "SELECT a.n, b.n FROM a INNER JOIN b ON a.id = b.a_id ORDER BY b.id",
        );
        assert_eq!(
            inner,
            vec![
                vec![text("a1"), text("b5")],
                vec![text("a1"), text("b6")],
                vec![text("a2"), text("b7")],
            ]
        );
        let (_, comma) = query(
            &mut database,
            "SELECT a.n, b.n FROM a, b WHERE a.id = b.a_id ORDER BY b.id",
        );
        assert_eq!(comma, inner);
        let (_, cross) = query(
            &mut database,
            "SELECT a.n, b.n FROM a CROSS JOIN b WHERE a.id = b.a_id ORDER BY b.id",
        );
        assert_eq!(cross, inner);

        let (_, left) = query(
            &mut database,
            "SELECT a.n, b.n FROM a LEFT JOIN b ON a.id = b.a_id ORDER BY a.id, b.id",
        );
        assert_eq!(
            left,
            vec![
                vec![text("a1"), text("b5")],
                vec![text("a1"), text("b6")],
                vec![text("a2"), text("b7")],
                vec![text("a3"), Value::Null],
            ]
        );
        let (_, plain_left) = query(
            &mut database,
            "SELECT a.n, bplain.n FROM a LEFT JOIN bplain ON a.id = bplain.a_id ORDER BY a.id, bplain.id",
        );
        assert_eq!(plain_left, left);

        let (_, filtered) = query(
            &mut database,
            "SELECT a.id, b.id FROM a LEFT JOIN b ON a.x = b.id WHERE b.id = 5",
        );
        let (_, filtered_plain) = query(
            &mut database,
            "SELECT a.id, bplain.id FROM a LEFT JOIN bplain ON a.x = bplain.id WHERE bplain.id = 5",
        );
        assert_eq!(filtered, vec![vec![int(1), int(5)]]);
        assert_eq!(filtered_plain, filtered);

        let (_, unmatched) = query(
            &mut database,
            "SELECT a.id FROM a LEFT JOIN b ON a.x = b.id WHERE b.id IS NULL ORDER BY a.id",
        );
        assert_eq!(unmatched, vec![vec![int(2)], vec![int(3)]]);

        let described = plan_text(
            &database,
            "SELECT a.id, b.id FROM a LEFT JOIN b ON a.x = b.id WHERE b.id = 5",
        );
        assert!(
            !described.contains("IndexLookup"),
            "left side of a left join must not use the index:\n{described}"
        );

        exec(
            &mut database,
            "CREATE TABLE c (id INTEGER, b_id INTEGER, n TEXT); \
             INSERT INTO c VALUES (1, 5, 'c1'), (2, 7, 'c2')",
        );
        let (columns, rows) = query(
            &mut database,
            "SELECT a.n, b.n, c.n \
             FROM a \
             INNER JOIN b ON a.id = b.a_id \
             INNER JOIN c ON b.id = c.b_id \
             ORDER BY c.id",
        );
        assert_eq!(
            columns,
            vec!["n".to_string(), "n".to_string(), "n".to_string()]
        );
        assert_eq!(
            rows,
            vec![
                vec![text("a1"), text("b5"), text("c1")],
                vec![text("a2"), text("b7"), text("c2")],
            ]
        );

        let (_, rows) = query(
            &mut database,
            "SELECT e.n, m.n FROM a e LEFT JOIN a m ON e.x = m.id ORDER BY e.id",
        );
        assert_eq!(
            rows,
            vec![
                vec![text("a1"), Value::Null],
                vec![text("a2"), Value::Null],
                vec![text("a3"), Value::Null],
            ]
        );
        exec(
            &mut database,
            "CREATE TABLE emp (id INTEGER PRIMARY KEY, name TEXT, mgr INTEGER); \
             INSERT INTO emp VALUES (1, 'Ann', NULL), (2, 'Bob', 1), (3, 'Cam', 1)",
        );
        let (_, rows) = query(
            &mut database,
            "SELECT e.name, m.name FROM emp e LEFT JOIN emp m ON e.mgr = m.id ORDER BY e.id",
        );
        assert_eq!(
            rows,
            vec![
                vec![text("Ann"), Value::Null],
                vec![text("Bob"), text("Ann")],
                vec![text("Cam"), text("Ann")],
            ]
        );

        let (columns, rows) = query(
            &mut database,
            "SELECT a.*, b.n FROM a INNER JOIN b ON a.id = b.a_id WHERE a.id = 2",
        );
        assert_eq!(
            columns,
            vec![
                "id".to_string(),
                "x".to_string(),
                "n".to_string(),
                "n".to_string()
            ]
        );
        assert_eq!(rows, vec![vec![int(2), int(9), text("a2"), text("b7")]]);

        assert_eq!(
            exec_err(
                &mut database,
                "SELECT id FROM a INNER JOIN b ON a.id = b.a_id"
            ),
            (ErrorKind::InvalidInput, "ambiguous column: id".to_string())
        );
        assert_eq!(
            exec_err(&mut database, "SELECT z FROM a"),
            (ErrorKind::InvalidInput, "column not found: z".to_string())
        );
        assert_eq!(
            exec_err(&mut database, "SELECT a.id FROM a u"),
            (
                ErrorKind::InvalidInput,
                "column not found: a.id".to_string()
            )
        );
        assert_eq!(
            exec_err(&mut database, "SELECT * FROM a, a"),
            (
                ErrorKind::InvalidInput,
                "duplicate table name in FROM: a".to_string()
            )
        );
        assert_eq!(
            exec_err(&mut database, "SELECT * FROM a u JOIN b u ON u.id = u.id"),
            (
                ErrorKind::InvalidInput,
                "duplicate table name in FROM: u".to_string()
            )
        );
        assert_eq!(
            exec_err(&mut database, "SELECT z.* FROM a"),
            (
                ErrorKind::InvalidInput,
                "table not found in FROM: z".to_string()
            )
        );
        assert_eq!(
            exec_err(&mut database, "SELECT * FROM a INNER JOIN b ON a.id = c.id"),
            (
                ErrorKind::InvalidInput,
                "column not found: c.id".to_string()
            )
        );
        assert_eq!(
            exec_err(&mut database, "SELECT * FROM a INNER JOIN b ON 1"),
            (
                ErrorKind::InvalidInput,
                "ON must be a boolean expression".to_string()
            )
        );

        let indexed = plan_text(
            &database,
            "SELECT a.n, b.n FROM a INNER JOIN b ON a.id = b.a_id WHERE a.id = 1",
        );
        assert!(indexed.contains("IndexLookup a key=1"), "{indexed}");
        assert!(indexed.contains("SeqScan b"), "{indexed}");

        let note = "n".repeat(3000);
        for id in 10..=40 {
            exec(
                &mut database,
                &format!("INSERT INTO a VALUES ({id}, NULL, '{note}')"),
            );
        }
        let before = database.pages_read();
        let (_, rows) = query(
            &mut database,
            "SELECT a.n, b.n FROM a INNER JOIN b ON a.id = b.a_id WHERE a.id = 1",
        );
        let indexed_pages = database.pages_read() - before;
        assert_eq!(
            rows,
            vec![vec![text("a1"), text("b5")], vec![text("a1"), text("b6")]]
        );

        let before = database.pages_read();
        let _ = query(
            &mut database,
            "SELECT a.n FROM a INNER JOIN b ON a.id = b.a_id WHERE a.id > 0",
        );
        let scanned_pages = database.pages_read() - before;
        assert!(
            scanned_pages > indexed_pages + 20,
            "indexed {indexed_pages}, scanned {scanned_pages}"
        );
    }

    fn plan_text(db: &Database, sql: &str) -> String {
        let statement = crate::sql::parse(sql).unwrap().pop().unwrap();
        let crate::sql::Statement::Select(select) = statement else {
            panic!("select");
        };
        crate::exec::plan::compile_select(db, &select)
            .unwrap()
            .plan
            .describe()
    }
}
