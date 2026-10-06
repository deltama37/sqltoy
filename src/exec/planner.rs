//! Rule-based planner.
//!
//! `WHERE` and `ON` are split into `AND` conjuncts. A constant is an
//! expression with no column reference that evaluates to an integer with no
//! row. Single-binding `WHERE` conjuncts sit in a filter directly above that
//! binding's access path, except a conjunct that references the nullable side
//! of a `LEFT JOIN`, which stays above that join. A primary-key equality
//! becomes an index lookup; primary-key inequalities become one index range.
//! Those conjuncts stay in the filter. A single-table `ORDER BY` of the
//! primary key ascending drops the sort and reads an index scan when the
//! access path would otherwise be a sequential scan. An inner or left join
//! whose `ON` has `right.pk = <left-only expr>` looks up the right table per
//! left row.
//!
//! With the planner disabled, every binding is a sequential scan, every join
//! is a nested loop, and `WHERE` is one filter above the joins.

use std::io;

use crate::btree::KeyBound;
use crate::catalog::{Database, TableSchema};
use crate::row::Value;
use crate::sql::{
    BinaryOp, ColumnRef, Expr, Join, JoinKind, Literal, OrderItem, Select, SelectItem, TableRef,
};

use super::eval::{bind_context, bind_expr, eval, invalid, resolve_ref, Binding, RowContext};
use super::plan::{Compiled, OwnedBinding, Plan, SortKey};

#[derive(Clone)]
struct Bound {
    name: String,
    table: String,
    schema: TableSchema,
    /// Right-hand side of a `LEFT JOIN`. `WHERE` on this binding stays above that join.
    null_side: bool,
}

struct OutputColumn {
    name: String,
    expr: Expr,
    alias: Option<String>,
}

enum PkCmp {
    Eq(i64),
    Lower(KeyBound),
    Upper(KeyBound),
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
    let planner = db.planner_enabled();
    let mut plan = plan_from(&bounds, &select.from.joins, select.filter.as_ref(), planner)?;
    let pk_order = planner && bounds.len() == 1 && is_pk_asc(&keys, &outputs, &bounds[0]);
    if pk_order {
        plan = order_by_index(plan);
    }
    plan = Plan::Project {
        input: Box::new(plan),
        exprs: outputs.iter().map(|column| column.expr.clone()).collect(),
        bindings: owned.clone(),
    };
    if !keys.is_empty() && !pk_order {
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
/// matches the same way it did before joins existed. `planner` applies the
/// single-table access-path rules; otherwise the scan is sequential.
pub(crate) fn compile_modify(
    schema: &TableSchema,
    filter: Option<&Expr>,
    planner: bool,
) -> io::Result<Plan> {
    if let Some(filter) = filter {
        bind_expr(&schema.name, &schema.columns, filter)?;
    }
    let bound = Bound {
        name: schema.name.clone(),
        table: schema.name.clone(),
        schema: schema.clone(),
        null_side: false,
    };
    plan_from(std::slice::from_ref(&bound), &[], filter, planner)
}

fn plan_from(
    bounds: &[Bound],
    joins: &[Join],
    filter: Option<&Expr>,
    planner: bool,
) -> io::Result<Plan> {
    if !planner {
        return Ok(plan_unoptimized(bounds, joins, filter));
    }
    let owned = owned_bindings(bounds);
    let conjuncts = filter.map(and_conjuncts_owned).unwrap_or_default();
    let mut local = vec![Vec::new(); bounds.len()];
    let mut above = vec![Vec::new(); joins.len()];
    let mut top = Vec::new();
    for conjunct in &conjuncts {
        place(conjunct, bounds, &owned, &mut local, &mut above, &mut top)?;
    }
    if joins.is_empty() {
        local[0].extend(top);
    } else if let Some(last) = above.last_mut() {
        last.extend(top);
    }

    let mut visible = vec![bounds[0].clone()];
    let mut plan = access_path(&bounds[0], &local[0]);
    plan = filter_plan(plan, &local[0], &visible);
    for (index, join) in joins.iter().enumerate() {
        let right = &bounds[index + 1];
        let left_owned = owned_bindings(&visible);
        visible.push(right.clone());
        let full_owned = owned_bindings(&visible);
        if let Some(key_expr) = index_join_key(join, right, &left_owned, &full_owned) {
            plan = Plan::IndexNestedLoopJoin {
                kind: join.kind,
                on: join
                    .on
                    .clone()
                    .ok_or_else(|| invalid("ON must be a boolean expression"))?,
                left: Box::new(plan),
                bindings: full_owned,
                right_columns: right.schema.columns.len(),
                right_schema: right.schema.clone(),
                right_table: right.table.clone(),
                right_binding: right.name.clone(),
                pk: pk_name(right).unwrap_or_default(),
                key_expr,
            };
            // The right access path is the lookup inside the join, so a
            // filter that would have sat on a right scan is applied to the
            // joined row instead. Left-join null sides are not in `local`.
            let mut preds = local[index + 1].clone();
            preds.extend(above[index].clone());
            plan = filter_plan(plan, &preds, &visible);
        } else {
            let mut right_plan = access_path(right, &local[index + 1]);
            right_plan = filter_plan(right_plan, &local[index + 1], std::slice::from_ref(right));
            plan = Plan::NestedLoopJoin {
                kind: join.kind,
                on: join.on.clone(),
                left: Box::new(plan),
                right: Box::new(right_plan),
                bindings: full_owned,
                right_columns: right.schema.columns.len(),
            };
            plan = filter_plan(plan, &above[index], &visible);
        }
    }
    Ok(plan)
}

fn plan_unoptimized(bounds: &[Bound], joins: &[Join], filter: Option<&Expr>) -> Plan {
    let mut plan = seq_scan(&bounds[0]);
    let mut visible = vec![bounds[0].clone()];
    for (index, join) in joins.iter().enumerate() {
        let right = &bounds[index + 1];
        visible.push(right.clone());
        plan = Plan::NestedLoopJoin {
            kind: join.kind,
            on: join.on.clone(),
            left: Box::new(plan),
            right: Box::new(seq_scan(right)),
            bindings: owned_bindings(&visible),
            right_columns: right.schema.columns.len(),
        };
    }
    if let Some(filter) = filter {
        plan = Plan::Filter {
            input: Box::new(plan),
            predicate: filter.clone(),
            bindings: owned_bindings(&visible),
        };
    }
    plan
}

fn place(
    conjunct: &Expr,
    bounds: &[Bound],
    owned: &[OwnedBinding],
    local: &mut [Vec<Expr>],
    above: &mut [Vec<Expr>],
    top: &mut Vec<Expr>,
) -> io::Result<()> {
    let refs = referenced(conjunct, owned)?;
    if refs.is_empty() {
        top.push(conjunct.clone());
        return Ok(());
    }
    if refs.len() == 1 {
        let index = refs[0];
        if bounds[index].null_side {
            above[index - 1].push(conjunct.clone());
        } else {
            local[index].push(conjunct.clone());
        }
        return Ok(());
    }
    let mut join_index = refs.iter().copied().max().expect("binding") - 1;
    for index in &refs {
        if bounds[*index].null_side {
            join_index = join_index.max(index - 1);
        }
    }
    above[join_index].push(conjunct.clone());
    Ok(())
}

fn access_path(bound: &Bound, conjuncts: &[Expr]) -> Plan {
    let Some(pk) = pk_name(bound) else {
        return seq_scan(bound);
    };
    let mut equality = None;
    let mut lower = None;
    let mut upper = None;
    for conjunct in conjuncts {
        match pk_cmp(conjunct, bound, &pk) {
            Some(PkCmp::Eq(value)) if equality.is_none() => equality = Some(value),
            Some(PkCmp::Lower(bound)) => lower = Some(tighten_lower(lower, bound)),
            Some(PkCmp::Upper(bound)) => upper = Some(tighten_upper(upper, bound)),
            _ => {}
        }
    }
    if let Some(key) = equality {
        return Plan::IndexLookup {
            table: bound.table.clone(),
            binding: bound.name.clone(),
            schema: bound.schema.clone(),
            pk,
            key,
        };
    }
    if lower.is_some() || upper.is_some() {
        let empty = range_empty(lower, upper);
        return Plan::IndexScan {
            table: bound.table.clone(),
            binding: bound.name.clone(),
            schema: bound.schema.clone(),
            pk,
            lower,
            upper,
            empty,
        };
    }
    seq_scan(bound)
}

fn seq_scan(bound: &Bound) -> Plan {
    Plan::SeqScan {
        table: bound.table.clone(),
        binding: bound.name.clone(),
        schema: bound.schema.clone(),
    }
}

fn filter_plan(plan: Plan, conjuncts: &[Expr], bounds: &[Bound]) -> Plan {
    if conjuncts.is_empty() {
        return plan;
    }
    Plan::Filter {
        input: Box::new(plan),
        predicate: and_all(conjuncts),
        bindings: owned_bindings(bounds),
    }
}

fn and_all(conjuncts: &[Expr]) -> Expr {
    let (first, rest) = conjuncts.split_first().expect("conjuncts");
    rest.iter()
        .cloned()
        .fold(first.clone(), |left, right| Expr::Binary {
            op: BinaryOp::And,
            left: Box::new(left),
            right: Box::new(right),
        })
}

fn order_by_index(plan: Plan) -> Plan {
    match plan {
        Plan::Filter {
            input,
            predicate,
            bindings,
        } => Plan::Filter {
            input: Box::new(order_by_index(*input)),
            predicate,
            bindings,
        },
        Plan::SeqScan {
            table,
            binding,
            schema,
        } => {
            let Some(pk_index) = schema.primary_key else {
                return Plan::SeqScan {
                    table,
                    binding,
                    schema,
                };
            };
            let pk = schema.columns[pk_index].name.clone();
            Plan::IndexScan {
                table,
                binding,
                schema,
                pk,
                lower: None,
                upper: None,
                empty: false,
            }
        }
        other => other,
    }
}

fn index_join_key(
    join: &Join,
    right: &Bound,
    left_owned: &[OwnedBinding],
    full_owned: &[OwnedBinding],
) -> Option<Expr> {
    if join.kind == JoinKind::Cross {
        return None;
    }
    let pk_index = right.schema.primary_key?;
    let on = join.on.as_ref()?;
    for conjunct in and_conjuncts(on) {
        if let Some(expr) = eq_right_pk(conjunct, pk_index, left_owned, full_owned) {
            return Some(expr);
        }
    }
    None
}

fn eq_right_pk(
    expr: &Expr,
    pk_index: usize,
    left_owned: &[OwnedBinding],
    full_owned: &[OwnedBinding],
) -> Option<Expr> {
    let Expr::Binary {
        op: BinaryOp::Eq,
        left,
        right,
    } = expr
    else {
        return None;
    };
    if is_right_pk(left, pk_index, full_owned) && columns_within(right, left_owned) {
        Some((**right).clone())
    } else if is_right_pk(right, pk_index, full_owned) && columns_within(left, left_owned) {
        Some((**left).clone())
    } else {
        None
    }
}

fn is_right_pk(expr: &Expr, pk_index: usize, full: &[OwnedBinding]) -> bool {
    let Expr::Column(reference) = expr else {
        return false;
    };
    let view = bind_view(full);
    match resolve_ref(&RowContext { bindings: &view }, reference) {
        Ok((binding, column)) => binding + 1 == full.len() && column == pk_index,
        Err(_) => false,
    }
}

fn columns_within(expr: &Expr, allowed: &[OwnedBinding]) -> bool {
    let view = bind_view(allowed);
    columns_resolve(expr, &RowContext { bindings: &view })
}

fn columns_resolve(expr: &Expr, ctx: &RowContext<'_>) -> bool {
    match expr {
        Expr::Literal(_) => true,
        Expr::Column(reference) => resolve_ref(ctx, reference).is_ok(),
        Expr::Unary { expr, .. } => columns_resolve(expr, ctx),
        Expr::Binary { left, right, .. } => {
            columns_resolve(left, ctx) && columns_resolve(right, ctx)
        }
        Expr::IsNull { expr, .. } => columns_resolve(expr, ctx),
    }
}

fn pk_name(bound: &Bound) -> Option<String> {
    let index = bound.schema.primary_key?;
    Some(bound.schema.columns[index].name.clone())
}

fn pk_cmp(expr: &Expr, bound: &Bound, pk: &str) -> Option<PkCmp> {
    let Expr::Binary { op, left, right } = expr else {
        return None;
    };
    let (pk_on_left, other) = if is_this_pk(left, bound, pk) && !contains_column(right) {
        (true, right.as_ref())
    } else if is_this_pk(right, bound, pk) && !contains_column(left) {
        (false, left.as_ref())
    } else {
        return None;
    };
    let value = const_integer(other)?;
    let op = if pk_on_left {
        op.clone()
    } else {
        flip_op(op.clone())
    };
    match op {
        BinaryOp::Eq => Some(PkCmp::Eq(value)),
        BinaryOp::Lt => Some(PkCmp::Upper(KeyBound {
            value,
            inclusive: false,
        })),
        BinaryOp::LtEq => Some(PkCmp::Upper(KeyBound {
            value,
            inclusive: true,
        })),
        BinaryOp::Gt => Some(PkCmp::Lower(KeyBound {
            value,
            inclusive: false,
        })),
        BinaryOp::GtEq => Some(PkCmp::Lower(KeyBound {
            value,
            inclusive: true,
        })),
        _ => None,
    }
}

fn flip_op(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Lt => BinaryOp::Gt,
        BinaryOp::LtEq => BinaryOp::GtEq,
        BinaryOp::Gt => BinaryOp::Lt,
        BinaryOp::GtEq => BinaryOp::LtEq,
        other => other,
    }
}

fn const_integer(expr: &Expr) -> Option<i64> {
    if contains_column(expr) {
        return None;
    }
    match eval(expr, &RowContext { bindings: &[] }) {
        Ok(Value::Integer(value)) => Some(value),
        _ => None,
    }
}

fn tighten_lower(current: Option<KeyBound>, next: KeyBound) -> KeyBound {
    match current {
        None => next,
        Some(current)
            if next.value > current.value
                || (next.value == current.value && !next.inclusive && current.inclusive) =>
        {
            next
        }
        Some(current) => current,
    }
}

fn tighten_upper(current: Option<KeyBound>, next: KeyBound) -> KeyBound {
    match current {
        None => next,
        Some(current)
            if next.value < current.value
                || (next.value == current.value && !next.inclusive && current.inclusive) =>
        {
            next
        }
        Some(current) => current,
    }
}

fn range_empty(lower: Option<KeyBound>, upper: Option<KeyBound>) -> bool {
    match (lower, upper) {
        (Some(lower), Some(upper)) => {
            lower.value > upper.value
                || (lower.value == upper.value && !(lower.inclusive && upper.inclusive))
        }
        _ => false,
    }
}

fn is_pk_asc(keys: &[SortKey], outputs: &[OutputColumn], bound: &Bound) -> bool {
    if keys.len() != 1 {
        return false;
    }
    let Some(pk) = pk_name(bound) else {
        return false;
    };
    match &keys[0] {
        SortKey::Output {
            index, descending, ..
        } => !descending && output_is_pk(&outputs[*index], bound, &pk),
        SortKey::Expr { expr, descending } => !descending && is_this_pk(expr, bound, &pk),
    }
}

fn output_is_pk(output: &OutputColumn, bound: &Bound, pk: &str) -> bool {
    is_this_pk(&output.expr, bound, pk)
}

fn is_this_pk(expr: &Expr, bound: &Bound, pk: &str) -> bool {
    let Expr::Column(column) = expr else {
        return false;
    };
    if !column.column.eq_ignore_ascii_case(pk) {
        return false;
    }
    match &column.table {
        Some(qualifier) => qualifier.eq_ignore_ascii_case(&bound.name),
        None => true,
    }
}

fn referenced(expr: &Expr, owned: &[OwnedBinding]) -> io::Result<Vec<usize>> {
    let mut found = Vec::new();
    let view = bind_view(owned);
    collect_refs(expr, &RowContext { bindings: &view }, &mut found)?;
    found.sort_unstable();
    found.dedup();
    Ok(found)
}

fn collect_refs(expr: &Expr, ctx: &RowContext<'_>, found: &mut Vec<usize>) -> io::Result<()> {
    match expr {
        Expr::Literal(_) => Ok(()),
        Expr::Column(reference) => {
            let (index, _) = resolve_ref(ctx, reference)?;
            found.push(index);
            Ok(())
        }
        Expr::Unary { expr, .. } => collect_refs(expr, ctx, found),
        Expr::Binary { left, right, .. } => {
            collect_refs(left, ctx, found)?;
            collect_refs(right, ctx, found)
        }
        Expr::IsNull { expr, .. } => collect_refs(expr, ctx, found),
    }
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

fn and_conjuncts_owned(expr: &Expr) -> Vec<Expr> {
    and_conjuncts(expr).into_iter().cloned().collect()
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
            io::ErrorKind::NotFound,
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
        let label = item.expr.to_string();
        if let Expr::Literal(Literal::Integer(position)) = &item.expr {
            if *position < 1 || *position as u64 > outputs.len() as u64 {
                return Err(invalid(format!(
                    "ORDER BY position out of range: {position}"
                )));
            }
            keys.push(SortKey::Output {
                index: (*position as usize) - 1,
                descending: item.descending,
                label,
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
                    label,
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
