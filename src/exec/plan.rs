//! Operator tree for one statement, and the text `EXPLAIN` prints.
//!
//! [`super::planner`] chooses the tree. This module describes it: one line
//! per operator, children indented by two spaces.

use std::fmt::Write as _;

use crate::btree::KeyBound;
use crate::catalog::{Column, TableSchema};
use crate::sql::{Expr, JoinKind};

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
        /// Source expression, printed by `EXPLAIN`.
        label: String,
    },
    /// Expression evaluated on the pre-projection row.
    Expr {
        /// Source expression.
        expr: Expr,
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
    /// Read the visible versions whose primary key equals `key`.
    IndexLookup {
        /// Table name as written in `FROM`.
        table: String,
        /// Binding name.
        binding: String,
        /// Schema used to decode the row.
        schema: TableSchema,
        /// Primary-key column name, as stored.
        pk: String,
        /// Primary-key value.
        key: i64,
    },
    /// Walk the primary-key leaves in ascending key order.
    ///
    /// `empty` is a range whose bounds cannot contain a key. The operator
    /// then returns no rows and reads no pages.
    IndexScan {
        /// Table name as written in `FROM`.
        table: String,
        /// Binding name.
        binding: String,
        /// Schema used to decode rows.
        schema: TableSchema,
        /// Primary-key column name, as stored.
        pk: String,
        /// Lower bound, when the scan does not start at the first key.
        lower: Option<KeyBound>,
        /// Upper bound, when the scan stops before the last key.
        upper: Option<KeyBound>,
        /// `true` when `lower` and `upper` exclude every key.
        empty: bool,
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
    /// For each left row, look up the right table's primary key.
    IndexNestedLoopJoin {
        /// `INNER` or `LEFT`.
        kind: JoinKind,
        /// Full `ON` expression, evaluated on each candidate.
        on: Expr,
        /// Left input.
        left: Box<Plan>,
        /// Bindings visible to `on`, including the right table.
        bindings: Vec<OwnedBinding>,
        /// Column count of the right table, used to null-extend a left join.
        right_columns: usize,
        /// Right table schema. The index root is the lookup target.
        right_schema: TableSchema,
        /// Right table name as written in `FROM`.
        right_table: String,
        /// Right binding name.
        right_binding: String,
        /// Right primary-key column name, as stored.
        pk: String,
        /// Expression compared with the right primary key. It references only the left.
        key_expr: Expr,
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
        self.describe_with(None)
    }

    /// [`Self::describe`], with ` (rows={n})` taken from `counts` in preorder.
    ///
    /// `counts` follows the same walk as [`super::operator::instantiate`]:
    /// the node, then its children left to right.
    pub(crate) fn describe_with(&self, counts: Option<&[u64]>) -> String {
        let mut lines = Vec::new();
        let mut index = 0;
        self.write_lines(&mut lines, 0, counts, &mut index);
        lines.join("\n")
    }

    fn write_lines(
        &self,
        lines: &mut Vec<String>,
        indent: usize,
        counts: Option<&[u64]>,
        index: &mut usize,
    ) {
        let mut line = String::new();
        for _ in 0..indent {
            line.push_str("  ");
        }
        line.push_str(&self.label());
        if let Some(counts) = counts {
            let _ = write!(line, " (rows={})", counts[*index]);
        }
        *index += 1;
        lines.push(line);
        match self {
            Plan::Filter { input, .. }
            | Plan::Project { input, .. }
            | Plan::Sort { input, .. }
            | Plan::Limit { input, .. }
            | Plan::IndexNestedLoopJoin { left: input, .. } => {
                input.write_lines(lines, indent + 1, counts, index);
            }
            Plan::NestedLoopJoin { left, right, .. } => {
                left.write_lines(lines, indent + 1, counts, index);
                right.write_lines(lines, indent + 1, counts, index);
            }
            Plan::SeqScan { .. } | Plan::IndexLookup { .. } | Plan::IndexScan { .. } => {}
        }
    }

    fn label(&self) -> String {
        match self {
            Plan::SeqScan { table, binding, .. } => scan_label("SeqScan", table, binding),
            Plan::IndexLookup {
                table,
                binding,
                pk,
                key,
                ..
            } => format!(
                "{} ({pk} = {key})",
                scan_label("IndexLookup", table, binding)
            ),
            Plan::IndexScan {
                table,
                binding,
                pk,
                lower,
                upper,
                ..
            } => index_scan_label(table, binding, pk, *lower, *upper),
            Plan::Filter { predicate, .. } => format!("Filter {predicate}"),
            Plan::NestedLoopJoin { kind, on, .. } => match on {
                Some(on) => format!("NestedLoopJoin {} ON {on}", join_label(*kind)),
                None => format!("NestedLoopJoin {}", join_label(*kind)),
            },
            Plan::IndexNestedLoopJoin {
                kind,
                right_table,
                right_binding,
                pk,
                key_expr,
                ..
            } => format!(
                "IndexNestedLoopJoin {} {} ({right_binding}.{pk} = {key_expr})",
                join_label(*kind),
                binding_name(right_table, right_binding)
            ),
            Plan::Project { exprs, .. } => {
                if exprs.is_empty() {
                    "Project".to_string()
                } else {
                    let list = exprs
                        .iter()
                        .map(|expr| expr.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("Project {list}")
                }
            }
            Plan::Sort { keys, .. } => sort_label(keys),
            Plan::Limit { limit, offset, .. } => {
                if *offset == 0 {
                    format!("Limit {limit}")
                } else {
                    format!("Limit {limit} OFFSET {offset}")
                }
            }
        }
    }
}

fn scan_label(op: &str, table: &str, binding: &str) -> String {
    format!("{op} {}", binding_name(table, binding))
}

fn binding_name(table: &str, binding: &str) -> String {
    if binding == table {
        binding.to_string()
    } else {
        format!("{table} AS {binding}")
    }
}

fn index_scan_label(
    table: &str,
    binding: &str,
    pk: &str,
    lower: Option<KeyBound>,
    upper: Option<KeyBound>,
) -> String {
    let mut label = scan_label("IndexScan", table, binding);
    let mut parts = Vec::new();
    if let Some(bound) = lower {
        let op = if bound.inclusive { ">=" } else { ">" };
        parts.push(format!("{pk} {op} {}", bound.value));
    }
    if let Some(bound) = upper {
        let op = if bound.inclusive { "<=" } else { "<" };
        parts.push(format!("{pk} {op} {}", bound.value));
    }
    if !parts.is_empty() {
        label.push_str(" (");
        label.push_str(&parts.join(" AND "));
        label.push(')');
    }
    label
}

fn sort_label(keys: &[SortKey]) -> String {
    if keys.is_empty() {
        return "Sort".to_string();
    }
    let list = keys
        .iter()
        .map(|key| {
            let (label, descending) = match key {
                SortKey::Output {
                    label, descending, ..
                } => (label.clone(), *descending),
                SortKey::Expr { expr, descending } => (expr.to_string(), *descending),
            };
            if descending {
                format!("{label} DESC")
            } else {
                label
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("Sort {list}")
}

fn join_label(kind: JoinKind) -> &'static str {
    match kind {
        JoinKind::Inner => "inner",
        JoinKind::Left => "left",
        JoinKind::Cross => "cross",
    }
}

pub(crate) use super::planner::{compile_modify, compile_select};
