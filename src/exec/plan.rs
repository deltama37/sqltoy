//! Query plan built before any operator runs.
//!
//! A [`Plan`] is the tree step 14 will print as `EXPLAIN` and swap out. This
//! module only chooses scans: a binding whose primary key is a constant
//! equality in a top-level `WHERE` `AND` term becomes an index lookup, except
//! the right side of a `LEFT JOIN`.

use std::io::{self, ErrorKind};

use crate::catalog::{Column, Database, TableSchema};
use crate::row::Value;
use crate::sql::{
    BinaryOp, ColumnRef, Expr, Join, JoinKind, Literal, OrderItem, Select, SelectItem, TableRef,
};

use super::eval::{bind_context, bind_expr, eval, invalid, resolve_ref, Binding, RowContext};

/// Column metadata for one `FROM` binding, owned by the plan.
#[derive(Debug, Clone)]
pub(crate) struct OwnedBinding {
    /// Alias, or the table name written in `FROM` when there is no alias.
    pub name: String,
    /// Columns in definition order.
    pub columns: Vec<Column>,
}

/// How one `ORDER BY` item is read.
#[derive(Debug, Clone)]
pub(crate) enum SortKey {
    /// 0-based index into the projected select list.
    Output {
        /// Index into the projected row.
        index: usize,
        /// `true` for `DESC`.
        descending: bool,
    },
    /// Expression evaluated on the pre-projection row.
    Expr {
        /// Source expression.
        expr: crate::sql::Expr,
        /// `true` for `DESC`.
        descending: bool,
    },
}

/// Operator tree for one statement.
#[derive(Debug, Clone)]
pub(crate) enum Plan {
    /// Read every live row of one table, one page at a time.
    SeqScan {
        /// Table name as written in `FROM`.
        table: String,
        /// Binding name.
        binding: String,
        /// Schema used to decode rows.
        schema: TableSchema,
    },
    /// Read the one row whose primary key equals `key`.
    IndexLookup {
        /// Table name as written in `FROM`.
        table: String,
        /// Binding name.
        binding: String,
        /// Schema used to decode the row.
        schema: TableSchema,
        /// Primary-key value.
        key: i64,
    },
    /// Keep rows for which `predicate` is `TRUE`.
    Filter {
        /// Input operator.
        input: Box<Plan>,
        /// `WHERE` or other predicate.
        predicate: Expr,
        /// Bindings visible to `predicate`, aligned with the input tuple.
        bindings: Vec<OwnedBinding>,
    },
    /// Nested loop. The right input is materialized on the first row.
    NestedLoopJoin {
        /// `INNER`, `LEFT`, or `CROSS`.
        kind: JoinKind,
        /// `ON` expression. `None` for `CROSS JOIN`.
        on: Option<Expr>,
        /// Left input.
        left: Box<Plan>,
        /// Right input.
        right: Box<Plan>,
        /// Bindings visible to `on`, including the right table.
        bindings: Vec<OwnedBinding>,
        /// Column count of the right table, used to null-extend a left join.
        right_columns: usize,
    },
    /// Evaluate the select list and keep the source tuple.
    Project {
        /// Input operator.
        input: Box<Plan>,
        /// One expression per output column.
        exprs: Vec<Expr>,
        /// Bindings visible to `exprs`.
        bindings: Vec<OwnedBinding>,
    },
    /// Stable sort. Reads the whole input before yielding.
    Sort {
        /// Input operator.
        input: Box<Plan>,
        /// Sort keys, left to right.
        keys: Vec<SortKey>,
        /// Bindings for [`SortKey::Expr`].
        bindings: Vec<OwnedBinding>,
    },
    /// Skip `offset` rows, then return at most `limit` rows.
    Limit {
        /// Input operator.
        input: Box<Plan>,
        /// Maximum number of rows to yield.
        limit: u64,
        /// Number of input rows to discard first.
        offset: u64,
    },
}

/// A `SELECT` plan and the output column names.
pub(crate) struct Compiled {
    /// Operator tree.
    pub plan: Plan,
    /// Header names, in select-list order after `*` expansion.
    pub columns: Vec<String>,
}

impl Plan {
    /// One line per operator, children indented by two spaces.
    pub(crate) fn describe(&self) -> String {
        let mut out = String::new();
        self.write_describe(&mut out, 0);
        if out.ends_with('\n') {
            out.pop();
        }
        out
    }

    fn write_describe(&self, out: &mut String, indent: usize) {
        for _ in 0..indent {
            out.push_str("  ");
        }
        match self {
            Plan::SeqScan { table, binding, .. } => {
                out.push_str(&scan_label("SeqScan", table, binding));
            }
            Plan::IndexLookup {
                table,
                binding,
                key,
                ..
            } => {
                out.push_str(&scan_label("IndexLookup", table, binding));
                out.push_str(&format!(" key={key}"));
            }
            Plan::Filter { input, .. } => {
                out.push_str("Filter");
                out.push('\n');
                input.write_describe(out, indent + 1);
                return;
            }
            Plan::NestedLoopJoin {
                kind, left, right, ..
            } => {
                out.push_str("NestedLoopJoin ");
                out.push_str(join_label(*kind));
                out.push('\n');
                left.write_describe(out, indent + 1);
                out.push('\n');
                right.write_describe(out, indent + 1);
                return;
            }
            Plan::Project { input, .. } => {
                out.push_str("Project");
                out.push('\n');
                input.write_describe(out, indent + 1);
                return;
            }
            Plan::Sort { input, .. } => {
                out.push_str("Sort");
                out.push('\n');
                input.write_describe(out, indent + 1);
                return;
            }
            Plan::Limit {
                input,
                limit,
                offset,
            } => {
                out.push_str(&format!("Limit limit={limit} offset={offset}"));
                out.push('\n');
                input.write_describe(out, indent + 1);
                return;
            }
        }
        out.push('\n');
    }
}

fn scan_label(op: &str, table: &str, binding: &str) -> String {
    if binding == table {
        format!("{op} {binding}")
    } else {
        format!("{op} {table} AS {binding}")
    }
}

fn join_label(kind: JoinKind) -> &'static str {
    match kind {
        JoinKind::Inner => "inner",
        JoinKind::Left => "left",
        JoinKind::Cross => "cross",
    }
}

#[derive(Clone)]
struct Bound {
    name: String,
    table: String,
    schema: TableSchema,
    null_side: bool,
}

struct OutputColumn {
    name: String,
    expr: Expr,
    alias: Option<String>,
}

/// Plan for `SELECT`. Names are resolved before any page is read.
pub(crate) fn compile_select(db: &Database, select: &Select) -> io::Result<Compiled> {
    let bounds = bind_from(db, &select.from.first, &select.from.joins)?;
    let outputs = expand_select(&bounds, &select.items)?;
    let owned = owned_bindings(&bounds);
    for column in &outputs {
        bind_owned(&owned, &column.expr)?;
    }
    bind_join_conditions(&bounds, &select.from.joins)?;
    if let Some(filter) = &select.filter {
        bind_owned(&owned, filter)?;
    }
    let keys = sort_keys(&select.order_by, &outputs, &owned)?;

    let mut plan = scan_tree(&bounds, &select.from.joins, select.filter.as_ref());
    if let Some(filter) = &select.filter {
        plan = Plan::Filter {
            input: Box::new(plan),
            predicate: filter.clone(),
            bindings: owned.clone(),
        };
    }
    plan = Plan::Project {
        input: Box::new(plan),
        exprs: outputs.iter().map(|column| column.expr.clone()).collect(),
        bindings: owned.clone(),
    };
    if !keys.is_empty() {
        plan = Plan::Sort {
            input: Box::new(plan),
            keys,
            bindings: owned,
        };
    }
    if let Some(limit) = &select.limit {
        let limit = non_negative(limit, "LIMIT")?;
        let offset = match &select.offset {
            Some(offset) => non_negative(offset, "OFFSET")?,
            None => 0,
        };
        plan = Plan::Limit {
            input: Box::new(plan),
            limit,
            offset,
        };
    }
    let columns = outputs.into_iter().map(|column| column.name).collect();
    Ok(Compiled { plan, columns })
}

/// Scan plus optional filter for `UPDATE` and `DELETE`.
///
/// The binding name is the schema's stored spelling, so `WHERE users.id = 1`
/// matches the same way it did before joins existed.
pub(crate) fn compile_modify(schema: &TableSchema, filter: Option<&Expr>) -> io::Result<Plan> {
    if let Some(filter) = filter {
        bind_expr(&schema.name, &schema.columns, filter)?;
    }
    let bound = Bound {
        name: schema.name.clone(),
        table: schema.name.clone(),
        schema: schema.clone(),
        null_side: false,
    };
    let mut plan = leaf(&bound, filter, std::slice::from_ref(&bound));
    if let Some(filter) = filter {
        plan = Plan::Filter {
            input: Box::new(plan),
            predicate: filter.clone(),
            bindings: owned_bindings(std::slice::from_ref(&bound)),
        };
    }
    Ok(plan)
}

fn bind_from(db: &Database, first: &TableRef, joins: &[Join]) -> io::Result<Vec<Bound>> {
    let mut bounds = vec![load_bound(db, first, false)?];
    for join in joins {
        let bound = load_bound(db, &join.table, join.kind == JoinKind::Left)?;
        if bounds
            .iter()
            .any(|existing| existing.name.eq_ignore_ascii_case(&bound.name))
        {
            return Err(invalid(format!(
                "duplicate table name in FROM: {}",
                bound.name
            )));
        }
        bounds.push(bound);
    }
    Ok(bounds)
}

fn load_bound(db: &Database, table: &TableRef, null_side: bool) -> io::Result<Bound> {
    let schema = db.table(&table.table).cloned().ok_or_else(|| {
        io::Error::new(
            ErrorKind::NotFound,
            format!("table not found: {}", table.table),
        )
    })?;
    let name = table.alias.clone().unwrap_or_else(|| table.table.clone());
    Ok(Bound {
        name,
        table: table.table.clone(),
        schema,
        null_side,
    })
}

fn expand_select(bounds: &[Bound], items: &[SelectItem]) -> io::Result<Vec<OutputColumn>> {
    let mut outputs = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard => {
                for bound in bounds {
                    push_columns(&mut outputs, bound);
                }
            }
            SelectItem::QualifiedWildcard(name) => {
                let bound = bounds
                    .iter()
                    .find(|bound| bound.name.eq_ignore_ascii_case(name))
                    .ok_or_else(|| invalid(format!("table not found in FROM: {name}")))?;
                push_columns(&mut outputs, bound);
            }
            SelectItem::Expr { expr, alias } => {
                let name = output_name(bounds, expr, alias)?;
                outputs.push(OutputColumn {
                    name,
                    expr: expr.clone(),
                    alias: alias.clone(),
                });
            }
        }
    }
    Ok(outputs)
}

fn push_columns(outputs: &mut Vec<OutputColumn>, bound: &Bound) {
    for column in &bound.schema.columns {
        outputs.push(OutputColumn {
            name: column.name.clone(),
            expr: Expr::Column(ColumnRef {
                table: Some(bound.name.clone()),
                column: column.name.clone(),
            }),
            alias: None,
        });
    }
}

fn output_name(bounds: &[Bound], expr: &Expr, alias: &Option<String>) -> io::Result<String> {
    if let Some(alias) = alias {
        return Ok(alias.clone());
    }
    if let Expr::Column(reference) = expr {
        let owned = owned_bindings(bounds);
        let view = bind_view(&owned);
        let (binding_index, column_index) =
            resolve_ref(&RowContext { bindings: &view }, reference)?;
        return Ok(bounds[binding_index].schema.columns[column_index]
            .name
            .clone());
    }
    Ok(expr.to_string())
}

fn bind_join_conditions(bounds: &[Bound], joins: &[Join]) -> io::Result<()> {
    let mut visible = vec![bounds[0].clone()];
    for (index, join) in joins.iter().enumerate() {
        visible.push(bounds[index + 1].clone());
        if let Some(on) = &join.on {
            bind_owned(&owned_bindings(&visible), on)?;
        }
    }
    Ok(())
}

fn sort_keys(
    items: &[OrderItem],
    outputs: &[OutputColumn],
    owned: &[OwnedBinding],
) -> io::Result<Vec<SortKey>> {
    let mut keys = Vec::new();
    for item in items {
        if let Expr::Literal(Literal::Integer(position)) = &item.expr {
            if *position < 1 || *position as u64 > outputs.len() as u64 {
                return Err(invalid(format!(
                    "ORDER BY position out of range: {position}"
                )));
            }
            keys.push(SortKey::Output {
                index: (*position as usize) - 1,
                descending: item.descending,
            });
            continue;
        }
        if let Some(name) = bare_name(&item.expr) {
            if let Some(index) = outputs.iter().position(|output| {
                output
                    .alias
                    .as_ref()
                    .is_some_and(|alias| alias.eq_ignore_ascii_case(name))
            }) {
                keys.push(SortKey::Output {
                    index,
                    descending: item.descending,
                });
                continue;
            }
        }
        bind_owned(owned, &item.expr)?;
        keys.push(SortKey::Expr {
            expr: item.expr.clone(),
            descending: item.descending,
        });
    }
    Ok(keys)
}

fn bare_name(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Column(ColumnRef {
            table: None,
            column,
        }) => Some(column),
        _ => None,
    }
}

fn non_negative(expr: &Expr, label: &str) -> io::Result<u64> {
    let ctx = RowContext { bindings: &[] };
    match eval(expr, &ctx)? {
        Value::Integer(value) if value >= 0 => Ok(value as u64),
        _ => Err(invalid(format!("{label} must be a non-negative integer"))),
    }
}

fn scan_tree(bounds: &[Bound], joins: &[Join], filter: Option<&Expr>) -> Plan {
    let mut plan = leaf(&bounds[0], filter, bounds);
    let mut visible = vec![bounds[0].clone()];
    for (index, join) in joins.iter().enumerate() {
        let right_bound = &bounds[index + 1];
        let right = leaf(right_bound, filter, bounds);
        visible.push(right_bound.clone());
        plan = Plan::NestedLoopJoin {
            kind: join.kind,
            on: join.on.clone(),
            left: Box::new(plan),
            right: Box::new(right),
            bindings: owned_bindings(&visible),
            right_columns: right_bound.schema.columns.len(),
        };
    }
    plan
}

fn leaf(bound: &Bound, filter: Option<&Expr>, all: &[Bound]) -> Plan {
    if !bound.null_side {
        if let Some(filter) = filter {
            if let Some(key) = index_key(bound, all, filter) {
                return Plan::IndexLookup {
                    table: bound.table.clone(),
                    binding: bound.name.clone(),
                    schema: bound.schema.clone(),
                    key,
                };
            }
        }
    }
    Plan::SeqScan {
        table: bound.table.clone(),
        binding: bound.name.clone(),
        schema: bound.schema.clone(),
    }
}

/// Integer from the first top-level `pk = const` conjunct for `bound`.
///
/// `None` means scan. A non-integer constant, or a constant that fails to
/// evaluate, also returns `None` so the scan reports the same error.
fn index_key(bound: &Bound, all: &[Bound], filter: &Expr) -> Option<i64> {
    let pk_index = bound.schema.primary_key?;
    let pk_name = bound.schema.columns[pk_index].name.as_str();
    let const_expr = first_pk_const(filter, bound, all, pk_name)?;
    let ctx = RowContext { bindings: &[] };
    match eval(const_expr, &ctx) {
        Ok(Value::Integer(key)) => Some(key),
        _ => None,
    }
}

fn first_pk_const<'a>(
    filter: &'a Expr,
    bound: &Bound,
    all: &[Bound],
    pk_name: &str,
) -> Option<&'a Expr> {
    for conjunct in and_conjuncts(filter) {
        if let Some(expr) = eq_pk_const(conjunct, bound, all, pk_name) {
            return Some(expr);
        }
    }
    None
}

fn and_conjuncts(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            let mut parts = and_conjuncts(left);
            parts.extend(and_conjuncts(right));
            parts
        }
        other => vec![other],
    }
}

fn eq_pk_const<'a>(
    expr: &'a Expr,
    bound: &Bound,
    all: &[Bound],
    pk_name: &str,
) -> Option<&'a Expr> {
    let Expr::Binary {
        op: BinaryOp::Eq,
        left,
        right,
    } = expr
    else {
        return None;
    };
    if is_pk_ref(left, bound, all, pk_name) && !contains_column(right) {
        Some(right)
    } else if is_pk_ref(right, bound, all, pk_name) && !contains_column(left) {
        Some(left)
    } else {
        None
    }
}

fn is_pk_ref(expr: &Expr, bound: &Bound, all: &[Bound], pk_name: &str) -> bool {
    let Expr::Column(reference) = expr else {
        return false;
    };
    if !reference.column.eq_ignore_ascii_case(pk_name) {
        return false;
    }
    if let Some(qualifier) = &reference.table {
        return qualifier.eq_ignore_ascii_case(&bound.name);
    }
    let owners = all
        .iter()
        .filter(|other| {
            other
                .schema
                .columns
                .iter()
                .any(|column| column.name.eq_ignore_ascii_case(&reference.column))
        })
        .count();
    owners == 1
}

fn contains_column(expr: &Expr) -> bool {
    match expr {
        Expr::Literal(_) => false,
        Expr::Column(_) => true,
        Expr::Unary { expr, .. } => contains_column(expr),
        Expr::Binary { left, right, .. } => contains_column(left) || contains_column(right),
        Expr::IsNull { expr, .. } => contains_column(expr),
    }
}

fn owned_bindings(bounds: &[Bound]) -> Vec<OwnedBinding> {
    bounds
        .iter()
        .map(|bound| OwnedBinding {
            name: bound.name.clone(),
            columns: bound.schema.columns.clone(),
        })
        .collect()
}

fn bind_owned(bindings: &[OwnedBinding], expr: &Expr) -> io::Result<()> {
    let view = bind_view(bindings);
    bind_context(&RowContext { bindings: &view }, expr)
}

fn bind_view(bindings: &[OwnedBinding]) -> Vec<Binding<'_>> {
    bindings
        .iter()
        .map(|binding| Binding {
            name: binding.name.as_str(),
            columns: binding.columns.as_slice(),
            values: None,
        })
        .collect()
}
