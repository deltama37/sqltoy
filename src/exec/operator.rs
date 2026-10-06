//! Volcano operators. Each [`Operator::next`] returns one tuple or ends.
//!
//! `SELECT` is scan, then joins, then filter, then project, then sort, then
//! limit. Sort reads its whole input. Limit stops calling `next` once it has
//! produced enough rows, so a sequential scan does not read another page.

use std::cmp::Ordering;
use std::io;

use crate::btree::BTree;
use crate::catalog::{Database, TableSchema};
use crate::page::PageId;
use crate::record::RecordId;
use crate::row::Value;
use crate::sql::{Expr, JoinKind};

use super::eval::{eval, invalid, Binding, RowContext};
use super::plan::{OwnedBinding, Plan, SortKey};

/// Values of one `FROM` binding, plus the row's record id when it has one.
///
/// A null-extended `LEFT JOIN` side has values and no record id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BindingSlot {
    /// Column values in definition order.
    pub values: Vec<Value>,
    /// Record id, when this side is a stored row.
    pub id: Option<RecordId>,
}

/// One row moving between operators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tuple {
    /// Bindings in `FROM` order.
    pub bindings: Vec<BindingSlot>,
    /// Select-list values. Empty until [`Project`] runs.
    pub projected: Vec<Value>,
}

/// Pull-based operator.
pub(crate) trait Operator {
    /// The next tuple, or `Ok(None)` when the input is exhausted.
    fn next(&mut self, db: &mut Database) -> io::Result<Option<Tuple>>;
}

/// Builds the operator tree for `plan`.
pub(crate) fn instantiate(plan: Plan) -> Box<dyn Operator> {
    match plan {
        Plan::SeqScan { schema, .. } => Box::new(SeqScan::new(schema)),
        Plan::IndexLookup { schema, key, .. } => Box::new(IndexLookup::new(schema, key)),
        Plan::Filter {
            input,
            predicate,
            bindings,
        } => Box::new(Filter {
            input: instantiate(*input),
            predicate,
            bindings,
        }),
        Plan::NestedLoopJoin {
            kind,
            on,
            left,
            right,
            bindings,
            right_columns,
        } => Box::new(NestedLoopJoin {
            kind,
            on,
            left: instantiate(*left),
            right: instantiate(*right),
            bindings,
            right_columns,
            right_rows: None,
            left_row: None,
            right_index: 0,
            matched: false,
        }),
        Plan::Project {
            input,
            exprs,
            bindings,
        } => Box::new(Project {
            input: instantiate(*input),
            exprs,
            bindings,
        }),
        Plan::Sort {
            input,
            keys,
            bindings,
        } => Box::new(Sort {
            input: instantiate(*input),
            keys,
            bindings,
            pending: Vec::new(),
            index: 0,
            loaded: false,
        }),
        Plan::Limit {
            input,
            limit,
            offset,
        } => Box::new(Limit {
            input: instantiate(*input),
            limit,
            offset,
            skipped: 0,
            emitted: 0,
        }),
    }
}

struct SeqScan {
    schema: TableSchema,
    next_page: u32,
    buffer: Vec<(RecordId, Vec<Value>)>,
    buffer_index: usize,
    done: bool,
}

impl SeqScan {
    fn new(schema: TableSchema) -> SeqScan {
        SeqScan {
            schema,
            next_page: 1,
            buffer: Vec::new(),
            buffer_index: 0,
            done: false,
        }
    }
}

impl Operator for SeqScan {
    fn next(&mut self, db: &mut Database) -> io::Result<Option<Tuple>> {
        loop {
            if self.buffer_index < self.buffer.len() {
                let (id, values) = self.buffer[self.buffer_index].clone();
                self.buffer_index += 1;
                return Ok(Some(one_binding(values, Some(id))));
            }
            if self.done {
                return Ok(None);
            }
            match db
                .records
                .scan_page(self.schema.id, PageId(self.next_page))?
            {
                None => {
                    self.done = true;
                    return Ok(None);
                }
                Some(records) => {
                    self.next_page += 1;
                    self.buffer_index = 0;
                    let mut decoded = Vec::with_capacity(records.len());
                    for (id, bytes) in records {
                        if let Some(values) = db.visible_values(&self.schema, id, &bytes)? {
                            decoded.push((id, values));
                        }
                    }
                    self.buffer = decoded;
                }
            }
        }
    }
}

struct IndexLookup {
    schema: TableSchema,
    key: i64,
    pending: Vec<(RecordId, Vec<Value>)>,
    pending_index: usize,
    loaded: bool,
}

impl IndexLookup {
    fn new(schema: TableSchema, key: i64) -> IndexLookup {
        IndexLookup {
            schema,
            key,
            pending: Vec::new(),
            pending_index: 0,
            loaded: false,
        }
    }
}

impl Operator for IndexLookup {
    fn next(&mut self, db: &mut Database) -> io::Result<Option<Tuple>> {
        if !self.loaded {
            self.loaded = true;
            let root = self
                .schema
                .index_root
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing index root"))?;
            let tree = BTree::open(root, self.schema.id);
            let ids = tree.lookup(db.records.pages_mut(), self.key)?;
            for id in ids {
                let bytes = db.records.get(self.schema.id, id)?;
                if let Some(values) = db.visible_values(&self.schema, id, &bytes)? {
                    self.pending.push((id, values));
                }
            }
        }
        if self.pending_index >= self.pending.len() {
            return Ok(None);
        }
        let (id, values) = self.pending[self.pending_index].clone();
        self.pending_index += 1;
        Ok(Some(one_binding(values, Some(id))))
    }
}

struct Filter {
    input: Box<dyn Operator>,
    predicate: Expr,
    bindings: Vec<OwnedBinding>,
}

impl Operator for Filter {
    fn next(&mut self, db: &mut Database) -> io::Result<Option<Tuple>> {
        loop {
            let Some(tuple) = self.input.next(db)? else {
                return Ok(None);
            };
            match predicate_true(&self.predicate, &self.bindings, &tuple, "WHERE") {
                Ok(true) => return Ok(Some(tuple)),
                Ok(false) => continue,
                Err(err) => {
                    // A non-integer primary-key constant falls back to a scan
                    // and then fails here. Drain the scan so that failure
                    // still reads every page, matching a full scan.
                    while self.input.next(db)?.is_some() {}
                    return Err(err);
                }
            }
        }
    }
}

struct NestedLoopJoin {
    kind: JoinKind,
    on: Option<Expr>,
    left: Box<dyn Operator>,
    right: Box<dyn Operator>,
    bindings: Vec<OwnedBinding>,
    right_columns: usize,
    right_rows: Option<Vec<Tuple>>,
    left_row: Option<Tuple>,
    right_index: usize,
    matched: bool,
}

impl Operator for NestedLoopJoin {
    fn next(&mut self, db: &mut Database) -> io::Result<Option<Tuple>> {
        if self.right_rows.is_none() {
            let mut rows = Vec::new();
            while let Some(row) = self.right.next(db)? {
                rows.push(row);
            }
            self.right_rows = Some(rows);
        }
        loop {
            if self.left_row.is_none() {
                match self.left.next(db)? {
                    Some(row) => {
                        self.left_row = Some(row);
                        self.right_index = 0;
                        self.matched = false;
                    }
                    None => return Ok(None),
                }
            }
            let right_len = self.right_rows.as_ref().map(Vec::len).unwrap_or(0);
            if self.right_index < right_len {
                let right = self.right_rows.as_ref().unwrap()[self.right_index].clone();
                self.right_index += 1;
                let left = self.left_row.as_ref().unwrap().clone();
                let combined = combine(&left, &right);
                let pass = match self.kind {
                    JoinKind::Cross => true,
                    JoinKind::Inner | JoinKind::Left => {
                        let on = self
                            .on
                            .as_ref()
                            .ok_or_else(|| invalid("ON must be a boolean expression"))?;
                        predicate_true(on, &self.bindings, &combined, "ON")?
                    }
                };
                if pass {
                    self.matched = true;
                    return Ok(Some(combined));
                }
                continue;
            }
            if self.kind == JoinKind::Left && !self.matched {
                let left = self.left_row.take().unwrap();
                return Ok(Some(combine(&left, &self.null_right())));
            }
            self.left_row = None;
        }
    }
}

impl NestedLoopJoin {
    fn null_right(&self) -> Tuple {
        one_binding(vec![Value::Null; self.right_columns], None)
    }
}

struct Project {
    input: Box<dyn Operator>,
    exprs: Vec<Expr>,
    bindings: Vec<OwnedBinding>,
}

impl Operator for Project {
    fn next(&mut self, db: &mut Database) -> io::Result<Option<Tuple>> {
        let Some(mut tuple) = self.input.next(db)? else {
            return Ok(None);
        };
        let mut projected = Vec::with_capacity(self.exprs.len());
        for expr in &self.exprs {
            projected.push(eval_on(&self.bindings, &tuple, expr)?);
        }
        tuple.projected = projected;
        Ok(Some(tuple))
    }
}

struct Sort {
    input: Box<dyn Operator>,
    keys: Vec<SortKey>,
    bindings: Vec<OwnedBinding>,
    pending: Vec<Tuple>,
    index: usize,
    loaded: bool,
}

impl Operator for Sort {
    fn next(&mut self, db: &mut Database) -> io::Result<Option<Tuple>> {
        if !self.loaded {
            let mut keyed = Vec::new();
            while let Some(tuple) = self.input.next(db)? {
                let values = sort_values(&self.keys, &self.bindings, &tuple)?;
                keyed.push((tuple, values));
            }
            let keys = self.keys.clone();
            keyed.sort_by(|left, right| compare_keys(&left.1, &right.1, &keys));
            self.pending = keyed.into_iter().map(|(tuple, _)| tuple).collect();
            self.loaded = true;
        }
        if self.index >= self.pending.len() {
            return Ok(None);
        }
        let tuple = self.pending[self.index].clone();
        self.index += 1;
        Ok(Some(tuple))
    }
}

struct Limit {
    input: Box<dyn Operator>,
    limit: u64,
    offset: u64,
    skipped: u64,
    emitted: u64,
}

impl Operator for Limit {
    fn next(&mut self, db: &mut Database) -> io::Result<Option<Tuple>> {
        while self.skipped < self.offset {
            match self.input.next(db)? {
                Some(_) => self.skipped += 1,
                None => return Ok(None),
            }
        }
        if self.emitted >= self.limit {
            return Ok(None);
        }
        match self.input.next(db)? {
            Some(tuple) => {
                self.emitted += 1;
                Ok(Some(tuple))
            }
            None => Ok(None),
        }
    }
}

fn one_binding(values: Vec<Value>, id: Option<RecordId>) -> Tuple {
    Tuple {
        bindings: vec![BindingSlot { values, id }],
        projected: Vec::new(),
    }
}

fn combine(left: &Tuple, right: &Tuple) -> Tuple {
    let mut bindings = left.bindings.clone();
    bindings.extend(right.bindings.iter().cloned());
    Tuple {
        bindings,
        projected: Vec::new(),
    }
}

fn predicate_true(
    expr: &Expr,
    bindings: &[OwnedBinding],
    tuple: &Tuple,
    label: &str,
) -> io::Result<bool> {
    match eval_on(bindings, tuple, expr)? {
        Value::Boolean(true) => Ok(true),
        Value::Boolean(false) | Value::Null => Ok(false),
        _ => Err(invalid(format!("{label} must be a boolean expression"))),
    }
}

fn eval_on(bindings: &[OwnedBinding], tuple: &Tuple, expr: &Expr) -> io::Result<Value> {
    let view: Vec<Binding<'_>> = bindings
        .iter()
        .zip(tuple.bindings.iter())
        .map(|(meta, slot)| Binding {
            name: meta.name.as_str(),
            columns: meta.columns.as_slice(),
            values: Some(slot.values.as_slice()),
        })
        .collect();
    eval(expr, &RowContext { bindings: &view })
}

fn sort_values(
    keys: &[SortKey],
    bindings: &[OwnedBinding],
    tuple: &Tuple,
) -> io::Result<Vec<Value>> {
    let mut values = Vec::with_capacity(keys.len());
    for key in keys {
        let value = match key {
            SortKey::Output { index, .. } => {
                tuple.projected.get(*index).cloned().ok_or_else(|| {
                    invalid(format!("ORDER BY position out of range: {}", index + 1))
                })?
            }
            SortKey::Expr { expr, .. } => eval_on(bindings, tuple, expr)?,
        };
        values.push(value);
    }
    Ok(values)
}

fn compare_keys(left: &[Value], right: &[Value], keys: &[SortKey]) -> Ordering {
    for (index, key) in keys.iter().enumerate() {
        let descending = match key {
            SortKey::Output { descending, .. } | SortKey::Expr { descending, .. } => *descending,
        };
        let mut ordering = compare_values(&left[index], &right[index]);
        if descending {
            ordering = ordering.reverse();
        }
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

/// `NULL` < boolean < integer < text. Same-type order matches comparisons.
pub(crate) fn compare_values(left: &Value, right: &Value) -> Ordering {
    fn rank(value: &Value) -> u8 {
        match value {
            Value::Null => 0,
            Value::Boolean(_) => 1,
            Value::Integer(_) => 2,
            Value::Text(_) => 3,
        }
    }
    match rank(left).cmp(&rank(right)) {
        Ordering::Equal => {}
        other => return other,
    }
    match (left, right) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Boolean(left), Value::Boolean(right)) => left.cmp(right),
        (Value::Integer(left), Value::Integer(right)) => left.cmp(right),
        (Value::Text(left), Value::Text(right)) => left.as_bytes().cmp(right.as_bytes()),
        _ => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::{compare_values, instantiate, BindingSlot, Operator, Tuple};
    use crate::catalog::{ColumnSpec, ColumnType, Database};
    use crate::exec::plan::{compile_select, OwnedBinding, Plan, SortKey};
    use crate::row::Value;
    use crate::sql::{parse, Expr, JoinKind, Literal, Statement};
    use std::cell::Cell;
    use std::cmp::Ordering;
    use std::env::temp_dir;
    use std::fs;
    use std::io;
    use std::path::PathBuf;
    use std::process;
    use std::rc::Rc;
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
            path.push(format!("sqltoy-op-{label}-{}-{nanos}", process::id()));
            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(crate::wal::wal_path(&path));
            TempDb { path }
        }

        fn path(&self) -> &PathBuf {
            &self.path
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
            let _ = fs::remove_file(crate::wal::wal_path(&self.path));
        }
    }

    fn open(label: &str) -> (TempDb, Database) {
        let db = TempDb::new(label);
        let database = Database::open(db.path()).unwrap();
        (db, database)
    }

    fn lit(value: Value) -> Expr {
        let literal = match value {
            Value::Null => Literal::Null,
            Value::Boolean(value) => Literal::Boolean(value),
            Value::Integer(value) => Literal::Integer(value),
            Value::Text(value) => Literal::Text(value),
        };
        Expr::Literal(literal)
    }

    struct ListOp {
        rows: Vec<Tuple>,
        index: usize,
        pulls: Rc<Cell<usize>>,
    }

    impl Operator for ListOp {
        fn next(&mut self, _db: &mut Database) -> io::Result<Option<Tuple>> {
            self.pulls.set(self.pulls.get() + 1);
            if self.index >= self.rows.len() {
                return Ok(None);
            }
            let row = self.rows[self.index].clone();
            self.index += 1;
            Ok(Some(row))
        }
    }

    fn slot(values: Vec<Value>, id: bool) -> Tuple {
        Tuple {
            bindings: vec![BindingSlot {
                values,
                id: id.then_some(crate::record::RecordId {
                    page_id: crate::page::PageId(1),
                    slot_id: 0,
                }),
            }],
            projected: Vec::new(),
        }
    }

    fn collect(op: &mut dyn Operator, db: &mut Database) -> Vec<Tuple> {
        let mut rows = Vec::new();
        while let Some(tuple) = op.next(db).unwrap() {
            rows.push(tuple);
        }
        rows
    }

    #[test]
    fn compare_orders_null_then_boolean_integer_text() {
        let values = [
            Value::Text("a".to_string()),
            Value::Null,
            Value::Integer(1),
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Integer(0),
            Value::Text(String::new()),
        ];
        let mut sorted = values.to_vec();
        sorted.sort_by(compare_values);
        assert_eq!(
            sorted,
            vec![
                Value::Null,
                Value::Boolean(false),
                Value::Boolean(true),
                Value::Integer(0),
                Value::Integer(1),
                Value::Text(String::new()),
                Value::Text("a".to_string()),
            ]
        );
        let mut reversed = values.to_vec();
        reversed.sort_by(|left, right| compare_values(left, right).reverse());
        assert_eq!(reversed.first(), Some(&Value::Text("a".to_string())));
        assert_eq!(reversed.last(), Some(&Value::Null));
        assert_eq!(compare_values(&Value::Null, &Value::Null), Ordering::Equal);
    }

    #[test]
    fn limit_stops_pulling_and_skips_offset() {
        let (_db, mut database) = open("limit");
        let pulls = Rc::new(Cell::new(0));
        let rows: Vec<Tuple> = (0..5)
            .map(|n| slot(vec![Value::Integer(n)], true))
            .collect();
        let input = ListOp {
            rows,
            index: 0,
            pulls: pulls.clone(),
        };
        let mut limit = super::Limit {
            input: Box::new(input),
            limit: 2,
            offset: 1,
            skipped: 0,
            emitted: 0,
        };
        let got = collect(&mut limit, &mut database);
        assert_eq!(
            got.iter()
                .map(|tuple| tuple.bindings[0].values[0].clone())
                .collect::<Vec<_>>(),
            vec![Value::Integer(1), Value::Integer(2)]
        );
        assert_eq!(pulls.get(), 3);

        let pulls = Rc::new(Cell::new(0));
        let input = ListOp {
            rows: (0..5)
                .map(|n| slot(vec![Value::Integer(n)], true))
                .collect(),
            index: 0,
            pulls: pulls.clone(),
        };
        let mut limit = super::Limit {
            input: Box::new(input),
            limit: 0,
            offset: 0,
            skipped: 0,
            emitted: 0,
        };
        assert!(limit.next(&mut database).unwrap().is_none());
        assert_eq!(pulls.get(), 0);

        let pulls = Rc::new(Cell::new(0));
        let input = ListOp {
            rows: (0..5)
                .map(|n| slot(vec![Value::Integer(n)], true))
                .collect(),
            index: 0,
            pulls: pulls.clone(),
        };
        let mut limit = super::Limit {
            input: Box::new(input),
            limit: 0,
            offset: 2,
            skipped: 0,
            emitted: 0,
        };
        assert!(limit.next(&mut database).unwrap().is_none());
        assert_eq!(pulls.get(), 2);
    }

    #[test]
    fn filter_project_sort_and_cross_join() {
        let (_db, mut database) = open("pipe");
        let rows = vec![
            slot(vec![Value::Integer(2)], true),
            slot(vec![Value::Integer(1)], true),
            slot(vec![Value::Null], true),
        ];
        let pulls = Rc::new(Cell::new(0));
        let input = ListOp {
            rows,
            index: 0,
            pulls,
        };
        let bindings = vec![OwnedBinding {
            name: "t".to_string(),
            columns: vec![crate::catalog::Column {
                name: "n".to_string(),
                column_type: ColumnType::Integer,
            }],
        }];
        let mut filter = super::Filter {
            input: Box::new(input),
            predicate: parse_expr("n IS NOT NULL"),
            bindings: bindings.clone(),
        };
        let filtered = collect(&mut filter, &mut database);
        assert_eq!(filtered.len(), 2);

        let pulls = Rc::new(Cell::new(0));
        let input = ListOp {
            rows: vec![slot(vec![Value::Integer(1)], true)],
            index: 0,
            pulls,
        };
        let mut project = super::Project {
            input: Box::new(input),
            exprs: vec![parse_expr("n + 1")],
            bindings: bindings.clone(),
        };
        let projected = project.next(&mut database).unwrap().unwrap();
        assert_eq!(projected.projected, vec![Value::Integer(2)]);
        assert_eq!(projected.bindings[0].values, vec![Value::Integer(1)]);

        let pulls = Rc::new(Cell::new(0));
        let input = ListOp {
            rows: vec![
                projected_row(Value::Integer(2), Value::Integer(1)),
                projected_row(Value::Null, Value::Integer(2)),
                projected_row(Value::Integer(2), Value::Integer(3)),
            ],
            index: 0,
            pulls,
        };
        let mut sort = super::Sort {
            input: Box::new(input),
            keys: vec![
                SortKey::Output {
                    index: 0,
                    descending: false,
                },
                SortKey::Output {
                    index: 1,
                    descending: true,
                },
            ],
            bindings: Vec::new(),
            pending: Vec::new(),
            index: 0,
            loaded: false,
        };
        let sorted = collect(&mut sort, &mut database);
        let pairs: Vec<(Value, Value)> = sorted
            .iter()
            .map(|tuple| (tuple.projected[0].clone(), tuple.projected[1].clone()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                (Value::Null, Value::Integer(2)),
                (Value::Integer(2), Value::Integer(3)),
                (Value::Integer(2), Value::Integer(1)),
            ]
        );

        let left_pulls = Rc::new(Cell::new(0));
        let right_pulls = Rc::new(Cell::new(0));
        let left = ListOp {
            rows: vec![
                slot(vec![Value::Integer(1)], true),
                slot(vec![Value::Integer(2)], true),
            ],
            index: 0,
            pulls: left_pulls.clone(),
        };
        let right = ListOp {
            rows: vec![
                slot(vec![Value::Text("a".to_string())], true),
                slot(vec![Value::Text("b".to_string())], true),
            ],
            index: 0,
            pulls: right_pulls.clone(),
        };
        let mut join = super::NestedLoopJoin {
            kind: JoinKind::Cross,
            on: None,
            left: Box::new(left),
            right: Box::new(right),
            bindings: Vec::new(),
            right_columns: 1,
            right_rows: None,
            left_row: None,
            right_index: 0,
            matched: false,
        };
        let joined = collect(&mut join, &mut database);
        assert_eq!(joined.len(), 4);
        assert_eq!(right_pulls.get(), 3);
        assert_eq!(left_pulls.get(), 3);
        assert_eq!(
            joined[0].bindings[1].values,
            vec![Value::Text("a".to_string())]
        );

        let left = ListOp {
            rows: vec![
                slot(vec![Value::Integer(1)], true),
                slot(vec![Value::Integer(2)], true),
            ],
            index: 0,
            pulls: Rc::new(Cell::new(0)),
        };
        let right = ListOp {
            rows: Vec::new(),
            index: 0,
            pulls: Rc::new(Cell::new(0)),
        };
        let mut join = super::NestedLoopJoin {
            kind: JoinKind::Left,
            on: Some(lit(Value::Boolean(false))),
            left: Box::new(left),
            right: Box::new(right),
            bindings: Vec::new(),
            right_columns: 1,
            right_rows: None,
            left_row: None,
            right_index: 0,
            matched: false,
        };
        let joined = collect(&mut join, &mut database);
        assert_eq!(joined.len(), 2);
        assert_eq!(joined[0].bindings[1].values, vec![Value::Null]);
        assert!(joined[0].bindings[1].id.is_none());
        assert!(joined[0].bindings[0].id.is_some());

        let left = ListOp {
            rows: vec![slot(vec![Value::Integer(1)], true)],
            index: 0,
            pulls: Rc::new(Cell::new(0)),
        };
        let right = ListOp {
            rows: vec![slot(vec![Value::Integer(2)], true)],
            index: 0,
            pulls: Rc::new(Cell::new(0)),
        };
        let mut join = super::NestedLoopJoin {
            kind: JoinKind::Inner,
            on: Some(lit(Value::Integer(1))),
            left: Box::new(left),
            right: Box::new(right),
            bindings: Vec::new(),
            right_columns: 1,
            right_rows: None,
            left_row: None,
            right_index: 0,
            matched: false,
        };
        let err = join.next(&mut database).unwrap_err();
        assert_eq!(err.to_string(), "ON must be a boolean expression");
    }

    fn projected_row(key: Value, tie: Value) -> Tuple {
        Tuple {
            bindings: Vec::new(),
            projected: vec![key, tie],
        }
    }

    fn parse_expr(sql: &str) -> Expr {
        let statements = parse(&format!("SELECT {sql} FROM t")).unwrap();
        let Statement::Select(select) = statements.into_iter().next().unwrap() else {
            panic!("select");
        };
        let crate::sql::SelectItem::Expr { expr, .. } = select.items.into_iter().next().unwrap()
        else {
            panic!("expr");
        };
        expr
    }

    #[test]
    fn seq_scan_reads_one_page_at_a_time_and_index_lookup_reads_one_row() {
        let (_db, mut database) = open("scan");
        database
            .create_table(
                "wide",
                &[
                    ColumnSpec::new("id", ColumnType::Integer),
                    ColumnSpec::new("note", ColumnType::Text),
                ],
            )
            .unwrap();
        let note = "n".repeat(3000);
        for id in 1..=3 {
            database
                .execute(&format!("INSERT INTO wide VALUES ({id}, '{note}')"))
                .unwrap();
        }
        let select = match parse("SELECT id FROM wide").unwrap().pop().unwrap() {
            Statement::Select(select) => select,
            _ => panic!("select"),
        };
        let compiled = compile_select(&database, &select).unwrap();
        assert!(compiled
            .plan
            .describe()
            .starts_with("Project\n  SeqScan wide"));
        let before = database.pages_read();
        let mut op = instantiate(compiled.plan);
        let first = op.next(&mut database).unwrap().unwrap();
        let after_first = database.pages_read() - before;
        assert_eq!(first.projected, vec![Value::Integer(1)]);
        assert!(after_first <= 2, "first row read {after_first} pages");
        let second = op.next(&mut database).unwrap().unwrap();
        assert_eq!(second.projected, vec![Value::Integer(2)]);
        let after_second = database.pages_read() - before;
        assert!(
            after_second > after_first,
            "second row should read another page ({after_first} then {after_second})"
        );

        database
            .execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT); INSERT INTO users VALUES (1, 'Ada'), (2, 'Bea')")
            .unwrap();
        let select = match parse("SELECT name FROM users WHERE id = 2")
            .unwrap()
            .pop()
            .unwrap()
        {
            Statement::Select(select) => select,
            _ => panic!("select"),
        };
        let compiled = compile_select(&database, &select).unwrap();
        let described = compiled.plan.describe();
        assert!(described.contains("IndexLookup users key=2"), "{described}");
        let mut op = instantiate(compiled.plan);
        let row = op.next(&mut database).unwrap().unwrap();
        assert_eq!(row.projected, vec![Value::Text("Bea".to_string())]);
        assert!(op.next(&mut database).unwrap().is_none());
    }

    #[test]
    fn plan_label_covers_each_operator() {
        let plan = Plan::Limit {
            limit: 1,
            offset: 2,
            input: Box::new(Plan::Sort {
                keys: Vec::new(),
                bindings: Vec::new(),
                input: Box::new(Plan::Project {
                    exprs: Vec::new(),
                    bindings: Vec::new(),
                    input: Box::new(Plan::Filter {
                        predicate: lit(Value::Boolean(true)),
                        bindings: Vec::new(),
                        input: Box::new(Plan::NestedLoopJoin {
                            kind: JoinKind::Left,
                            on: None,
                            bindings: Vec::new(),
                            right_columns: 0,
                            left: Box::new(Plan::SeqScan {
                                table: "a".to_string(),
                                binding: "a".to_string(),
                                schema: empty_schema(),
                            }),
                            right: Box::new(Plan::IndexLookup {
                                table: "b".to_string(),
                                binding: "bb".to_string(),
                                schema: empty_schema(),
                                key: 5,
                            }),
                        }),
                    }),
                }),
            }),
        };
        let text = plan.describe();
        assert!(text.contains("Limit limit=1 offset=2"), "{text}");
        assert!(text.contains("Sort"), "{text}");
        assert!(text.contains("Project"), "{text}");
        assert!(text.contains("Filter"), "{text}");
        assert!(text.contains("NestedLoopJoin left"), "{text}");
        assert!(text.contains("SeqScan a"), "{text}");
        assert!(text.contains("IndexLookup b AS bb key=5"), "{text}");
    }

    fn empty_schema() -> crate::catalog::TableSchema {
        crate::catalog::TableSchema {
            id: crate::record::TableId(2),
            name: "a".to_string(),
            columns: Vec::new(),
            primary_key: None,
            index_root: None,
        }
    }
}
