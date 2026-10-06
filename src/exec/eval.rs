//! Expression evaluation.
//!
//! Comparisons yield `NULL` when either side is `NULL`, before the types are
//! checked. `AND`, `OR`, and `NOT` follow SQL's three-valued truth tables.
//! Both operands are evaluated; a later error is not skipped. Arithmetic
//! accepts only integers. `NULL` propagates when every operand is an integer
//! or `NULL`, including `NULL / 0`. Overflow and division by zero are errors.

use std::cmp::Ordering;
use std::io::{self, ErrorKind};

use crate::catalog::Column;
use crate::row::Value;
use crate::sql::{BinaryOp, ColumnRef, Expr, Literal, UnaryOp};

/// Names and values visible to an expression.
pub struct RowContext<'a> {
    /// Table name as stored in the schema.
    pub table: &'a str,
    /// Columns in definition order.
    pub columns: &'a [Column],
    /// Current row, aligned with `columns`.
    ///
    /// `None` when the expression is not evaluated against a row, as in
    /// `INSERT` values. A column reference is then `column not found`,
    /// including a name that exists on the table.
    pub values: Option<&'a [Value]>,
}

/// Evaluates `expr` against `ctx`.
///
/// A failure is [`ErrorKind::InvalidInput`].
pub fn eval(expr: &Expr, ctx: &RowContext<'_>) -> io::Result<Value> {
    match expr {
        Expr::Literal(literal) => Ok(literal_value(literal)),
        Expr::Column(reference) => eval_column(ctx, reference),
        Expr::Unary { op, expr } => {
            let value = eval(expr, ctx)?;
            match op {
                UnaryOp::Not => eval_not(value),
                UnaryOp::Neg => eval_neg(value),
            }
        }
        Expr::Binary { op, left, right } => {
            let left = eval(left, ctx)?;
            let right = eval(right, ctx)?;
            eval_binary(op, left, right)
        }
        Expr::IsNull { expr, negated } => {
            let value = eval(expr, ctx)?;
            let is_null = matches!(value, Value::Null);
            Ok(Value::Boolean(if *negated { !is_null } else { is_null }))
        }
    }
}

/// Resolves every column reference in `expr` against the schema.
///
/// An unknown column, or a qualifier other than `table`, is
/// `column not found`. No row is read.
pub(crate) fn bind_expr(table: &str, columns: &[Column], expr: &Expr) -> io::Result<()> {
    match expr {
        Expr::Literal(_) => Ok(()),
        Expr::Column(reference) => {
            resolve_column(table, columns, reference)?;
            Ok(())
        }
        Expr::Unary { expr, .. } => bind_expr(table, columns, expr),
        Expr::Binary { left, right, .. } => {
            bind_expr(table, columns, left)?;
            bind_expr(table, columns, right)
        }
        Expr::IsNull { expr, .. } => bind_expr(table, columns, expr),
    }
}

pub(crate) fn resolve_column(
    table: &str,
    columns: &[Column],
    reference: &ColumnRef,
) -> io::Result<usize> {
    if let Some(qualifier) = &reference.table {
        if !qualifier.eq_ignore_ascii_case(table) {
            return Err(column_not_found(&reference.to_string()));
        }
    }
    columns
        .iter()
        .position(|column| column.name.eq_ignore_ascii_case(&reference.column))
        .ok_or_else(|| column_not_found(&reference.to_string()))
}

pub(crate) fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(ErrorKind::InvalidInput, message.into())
}

pub(crate) fn column_not_found(name: &str) -> io::Error {
    invalid(format!("column not found: {name}"))
}

fn literal_value(literal: &Literal) -> Value {
    match literal {
        Literal::Null => Value::Null,
        Literal::Boolean(value) => Value::Boolean(*value),
        Literal::Integer(value) => Value::Integer(*value),
        Literal::Text(value) => Value::Text(value.clone()),
    }
}

fn eval_column(ctx: &RowContext<'_>, reference: &ColumnRef) -> io::Result<Value> {
    let index = resolve_column(ctx.table, ctx.columns, reference)?;
    match ctx.values {
        Some(values) => Ok(values[index].clone()),
        None => Err(column_not_found(&reference.to_string())),
    }
}

fn eval_binary(op: &BinaryOp, left: Value, right: Value) -> io::Result<Value> {
    match op {
        BinaryOp::And => eval_and(left, right),
        BinaryOp::Or => eval_or(left, right),
        BinaryOp::Eq
        | BinaryOp::NotEq
        | BinaryOp::Lt
        | BinaryOp::LtEq
        | BinaryOp::Gt
        | BinaryOp::GtEq => compare(op, &left, &right),
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
            eval_arith(op, left, right)
        }
    }
}

fn eval_not(value: Value) -> io::Result<Value> {
    match value {
        Value::Boolean(value) => Ok(Value::Boolean(!value)),
        Value::Null => Ok(Value::Null),
        other => Err(type_mismatch_unary(&UnaryOp::Not, &other)),
    }
}

fn eval_neg(value: Value) -> io::Result<Value> {
    match value {
        Value::Null => Ok(Value::Null),
        Value::Integer(value) => value
            .checked_neg()
            .map(Value::Integer)
            .ok_or_else(|| invalid("integer overflow")),
        other => Err(type_mismatch_unary(&UnaryOp::Neg, &other)),
    }
}

fn eval_and(left: Value, right: Value) -> io::Result<Value> {
    match (&left, &right) {
        (Value::Boolean(false), Value::Boolean(_) | Value::Null) => Ok(Value::Boolean(false)),
        (Value::Boolean(true), Value::Boolean(false)) | (Value::Null, Value::Boolean(false)) => {
            Ok(Value::Boolean(false))
        }
        (Value::Boolean(true), Value::Boolean(true)) => Ok(Value::Boolean(true)),
        (Value::Boolean(true), Value::Null)
        | (Value::Null, Value::Boolean(true))
        | (Value::Null, Value::Null) => Ok(Value::Null),
        _ => Err(type_mismatch_bin(&left, &BinaryOp::And, &right)),
    }
}

fn eval_or(left: Value, right: Value) -> io::Result<Value> {
    match (&left, &right) {
        (Value::Boolean(true), Value::Boolean(_) | Value::Null) => Ok(Value::Boolean(true)),
        (Value::Boolean(false), Value::Boolean(true)) | (Value::Null, Value::Boolean(true)) => {
            Ok(Value::Boolean(true))
        }
        (Value::Boolean(false), Value::Boolean(false)) => Ok(Value::Boolean(false)),
        (Value::Boolean(false), Value::Null)
        | (Value::Null, Value::Boolean(false))
        | (Value::Null, Value::Null) => Ok(Value::Null),
        _ => Err(type_mismatch_bin(&left, &BinaryOp::Or, &right)),
    }
}

fn compare(op: &BinaryOp, left: &Value, right: &Value) -> io::Result<Value> {
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return Ok(Value::Null);
    }
    let ordering = match (left, right) {
        (Value::Integer(left), Value::Integer(right)) => left.cmp(right),
        // Byte order, not a Unicode collation.
        (Value::Text(left), Value::Text(right)) => left.as_bytes().cmp(right.as_bytes()),
        (Value::Boolean(left), Value::Boolean(right)) => left.cmp(right),
        _ => return Err(type_mismatch_bin(left, op, right)),
    };
    let result = match op {
        BinaryOp::Eq => ordering == Ordering::Equal,
        BinaryOp::NotEq => ordering != Ordering::Equal,
        BinaryOp::Lt => ordering == Ordering::Less,
        BinaryOp::LtEq => ordering != Ordering::Greater,
        BinaryOp::Gt => ordering == Ordering::Greater,
        BinaryOp::GtEq => ordering != Ordering::Less,
        _ => unreachable!("comparison operator"),
    };
    Ok(Value::Boolean(result))
}

fn eval_arith(op: &BinaryOp, left: Value, right: Value) -> io::Result<Value> {
    match (&left, &right) {
        (Value::Integer(left_n), Value::Integer(right_n)) => {
            apply_arith(op, *left_n, *right_n).map(Value::Integer)
        }
        (Value::Null, Value::Null)
        | (Value::Null, Value::Integer(_))
        | (Value::Integer(_), Value::Null) => Ok(Value::Null),
        _ => Err(type_mismatch_bin(&left, op, &right)),
    }
}

fn apply_arith(op: &BinaryOp, left: i64, right: i64) -> io::Result<i64> {
    if matches!(op, BinaryOp::Div | BinaryOp::Mod) && right == 0 {
        return Err(invalid("division by zero"));
    }
    let value = match op {
        BinaryOp::Add => left.checked_add(right),
        BinaryOp::Sub => left.checked_sub(right),
        BinaryOp::Mul => left.checked_mul(right),
        BinaryOp::Div => left.checked_div(right),
        BinaryOp::Mod => left.checked_rem(right),
        _ => unreachable!("arithmetic operator"),
    };
    value.ok_or_else(|| invalid("integer overflow"))
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NULL",
        Value::Integer(_) => "INTEGER",
        Value::Text(_) => "TEXT",
        Value::Boolean(_) => "BOOLEAN",
    }
}

fn type_mismatch_bin(left: &Value, op: &BinaryOp, right: &Value) -> io::Error {
    invalid(format!(
        "type mismatch: {} {op} {}",
        type_name(left),
        type_name(right)
    ))
}

fn type_mismatch_unary(op: &UnaryOp, value: &Value) -> io::Error {
    invalid(format!("type mismatch: {op} {}", type_name(value)))
}

#[cfg(test)]
mod tests {
    use super::{eval, RowContext};
    use crate::catalog::{Column, ColumnType};
    use crate::row::Value;
    use crate::sql::{parse, Expr, SelectItem, Statement};
    use std::io::ErrorKind;

    fn parse_expr(sql: &str) -> Expr {
        let statements = parse(&format!("SELECT {sql} FROM t")).unwrap();
        let statement = statements.into_iter().next().unwrap();
        let Statement::Select(select) = statement else {
            panic!("expected select");
        };
        let item = select.items.into_iter().next().unwrap();
        let SelectItem::Expr { expr, .. } = item else {
            panic!("expected expression");
        };
        expr
    }

    fn eval_sql(sql: &str, ctx: &RowContext<'_>) -> Value {
        eval(&parse_expr(sql), ctx).unwrap()
    }

    fn eval_err(sql: &str, ctx: &RowContext<'_>) -> String {
        let err = eval(&parse_expr(sql), ctx).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        err.to_string()
    }

    fn bare<'a>() -> RowContext<'a> {
        RowContext {
            table: "t",
            columns: &[],
            values: None,
        }
    }

    fn column(name: &str, column_type: ColumnType) -> Column {
        Column {
            name: name.to_string(),
            column_type,
        }
    }

    #[test]
    fn and_or_not_follow_sql_truth_tables() {
        let ctx = bare();
        let cases = [
            ("TRUE AND TRUE", Value::Boolean(true)),
            ("TRUE AND FALSE", Value::Boolean(false)),
            ("TRUE AND NULL", Value::Null),
            ("FALSE AND TRUE", Value::Boolean(false)),
            ("FALSE AND FALSE", Value::Boolean(false)),
            ("FALSE AND NULL", Value::Boolean(false)),
            ("NULL AND TRUE", Value::Null),
            ("NULL AND FALSE", Value::Boolean(false)),
            ("NULL AND NULL", Value::Null),
            ("TRUE OR TRUE", Value::Boolean(true)),
            ("TRUE OR FALSE", Value::Boolean(true)),
            ("TRUE OR NULL", Value::Boolean(true)),
            ("FALSE OR TRUE", Value::Boolean(true)),
            ("FALSE OR FALSE", Value::Boolean(false)),
            ("FALSE OR NULL", Value::Null),
            ("NULL OR TRUE", Value::Boolean(true)),
            ("NULL OR FALSE", Value::Null),
            ("NULL OR NULL", Value::Null),
            ("NOT TRUE", Value::Boolean(false)),
            ("NOT FALSE", Value::Boolean(true)),
            ("NOT NULL", Value::Null),
            ("NOT NOT TRUE", Value::Boolean(true)),
            ("TRUE OR FALSE AND FALSE", Value::Boolean(true)),
            ("NOT FALSE AND FALSE", Value::Boolean(false)),
        ];
        for (sql, expected) in cases {
            assert_eq!(eval_sql(sql, &ctx), expected, "{sql}");
        }
    }

    #[test]
    fn comparisons_nulls_and_order() {
        let ctx = bare();
        let cases = [
            ("1 = 1", Value::Boolean(true)),
            ("1 = 2", Value::Boolean(false)),
            ("1 <> 2", Value::Boolean(true)),
            ("1 != 1", Value::Boolean(false)),
            ("1 < 2", Value::Boolean(true)),
            ("2 <= 2", Value::Boolean(true)),
            ("2 > 1", Value::Boolean(true)),
            ("2 >= 3", Value::Boolean(false)),
            ("- 1 < 0", Value::Boolean(true)),
            ("'a' = 'a'", Value::Boolean(true)),
            ("'a' < 'b'", Value::Boolean(true)),
            ("'b' < 'a'", Value::Boolean(false)),
            ("'ab' < 'b'", Value::Boolean(true)),
            ("'ab' > 'a'", Value::Boolean(true)),
            ("'' < 'a'", Value::Boolean(true)),
            ("'A' < 'a'", Value::Boolean(true)),
            ("'e' < 'é'", Value::Boolean(true)),
            ("FALSE = FALSE", Value::Boolean(true)),
            ("FALSE = TRUE", Value::Boolean(false)),
            ("FALSE < TRUE", Value::Boolean(true)),
            ("TRUE > FALSE", Value::Boolean(true)),
            ("TRUE <= FALSE", Value::Boolean(false)),
            ("FALSE <> TRUE", Value::Boolean(true)),
            ("FALSE >= TRUE", Value::Boolean(false)),
            ("1 = NULL", Value::Null),
            ("NULL = 1", Value::Null),
            ("NULL = NULL", Value::Null),
            ("NULL <> NULL", Value::Null),
            ("NULL < 1", Value::Null),
            ("'a' = NULL", Value::Null),
            ("NULL = 'a'", Value::Null),
            ("TRUE = NULL", Value::Null),
            ("NULL < FALSE", Value::Null),
            ("1 IS NULL", Value::Boolean(false)),
            ("NULL IS NULL", Value::Boolean(true)),
            ("NULL IS NOT NULL", Value::Boolean(false)),
            ("FALSE IS NULL", Value::Boolean(false)),
            ("FALSE IS NOT NULL", Value::Boolean(true)),
            ("'a' IS NOT NULL", Value::Boolean(true)),
        ];
        for (sql, expected) in cases {
            assert_eq!(eval_sql(sql, &ctx), expected, "{sql}");
        }
    }

    #[test]
    fn arithmetic_edges() {
        let ctx = bare();
        let cases = [
            ("1 + 2", Value::Integer(3)),
            ("5 - 2 - 1", Value::Integer(2)),
            ("2 * 3 + 4", Value::Integer(10)),
            ("1 + 2 * 3", Value::Integer(7)),
            ("20 / 2 / 2", Value::Integer(5)),
            ("5 / 2", Value::Integer(2)),
            ("- 5 / 2", Value::Integer(-2)),
            ("5 / - 2", Value::Integer(-2)),
            ("- 5 / - 2", Value::Integer(2)),
            ("5 % 2", Value::Integer(1)),
            ("- 5 % 2", Value::Integer(-1)),
            ("5 % - 2", Value::Integer(1)),
            ("- 5 % - 2", Value::Integer(-1)),
            ("1 + NULL", Value::Null),
            ("NULL + 1", Value::Null),
            ("NULL + NULL", Value::Null),
            ("NULL * 0", Value::Null),
            ("NULL / 0", Value::Null),
            ("1 / NULL", Value::Null),
            ("NULL % 0", Value::Null),
            ("- NULL", Value::Null),
            ("(-9223372036854775807) - 1", Value::Integer(i64::MIN)),
        ];
        for (sql, expected) in cases {
            assert_eq!(eval_sql(sql, &ctx), expected, "{sql}");
        }

        let errors = [
            ("9223372036854775807 + 1", "integer overflow"),
            ("(-9223372036854775807) - 2", "integer overflow"),
            ("9223372036854775807 * 2", "integer overflow"),
            ("- ((-9223372036854775807) - 1)", "integer overflow"),
            ("((-9223372036854775807) - 1) / - 1", "integer overflow"),
            ("((-9223372036854775807) - 1) % - 1", "integer overflow"),
            ("((-9223372036854775807) - 1) * - 1", "integer overflow"),
            ("1 / 0", "division by zero"),
            ("1 % 0", "division by zero"),
            ("- 1 / 0", "division by zero"),
            ("FALSE AND (1 / 0)", "division by zero"),
            ("TRUE OR (1 / 0)", "division by zero"),
        ];
        for (sql, message) in errors {
            assert_eq!(eval_err(sql, &ctx), message, "{sql}");
        }
    }

    #[test]
    fn type_errors_name_the_operator() {
        let ctx = bare();
        let errors = [
            ("1 + 'a'", "type mismatch: INTEGER + TEXT"),
            ("'a' + 1", "type mismatch: TEXT + INTEGER"),
            ("NULL + 'a'", "type mismatch: NULL + TEXT"),
            ("'a' - NULL", "type mismatch: TEXT - NULL"),
            ("TRUE + 1", "type mismatch: BOOLEAN + INTEGER"),
            ("1 = 'a'", "type mismatch: INTEGER = TEXT"),
            ("TRUE < 1", "type mismatch: BOOLEAN < INTEGER"),
            ("1 <> TRUE", "type mismatch: INTEGER <> BOOLEAN"),
            ("'a' >= 1", "type mismatch: TEXT >= INTEGER"),
            ("TRUE AND 1", "type mismatch: BOOLEAN AND INTEGER"),
            ("1 OR TRUE", "type mismatch: INTEGER OR BOOLEAN"),
            ("NULL AND 1", "type mismatch: NULL AND INTEGER"),
            ("FALSE AND 1", "type mismatch: BOOLEAN AND INTEGER"),
            ("1 % TRUE", "type mismatch: INTEGER % BOOLEAN"),
            ("NOT 'a'", "type mismatch: NOT TEXT"),
            ("NOT 1", "type mismatch: NOT INTEGER"),
            ("- 'a'", "type mismatch: - TEXT"),
            ("- TRUE", "type mismatch: - BOOLEAN"),
        ];
        for (sql, message) in errors {
            assert_eq!(eval_err(sql, &ctx), message, "{sql}");
        }
    }

    #[test]
    fn column_resolution() {
        let columns = vec![
            column("id", ColumnType::Integer),
            column("name", ColumnType::Text),
        ];
        let values = vec![Value::Integer(7), Value::Text("Ada".to_string())];
        let ctx = RowContext {
            table: "users",
            columns: &columns,
            values: Some(&values),
        };
        assert_eq!(eval_sql("id", &ctx), Value::Integer(7));
        assert_eq!(eval_sql("ID", &ctx), Value::Integer(7));
        assert_eq!(eval_sql("Name", &ctx), Value::Text("Ada".to_string()));
        assert_eq!(eval_sql("users.id", &ctx), Value::Integer(7));
        assert_eq!(eval_sql("USERS.Name", &ctx), Value::Text("Ada".to_string()));
        assert_eq!(eval_sql("id + 1", &ctx), Value::Integer(8));
        assert_eq!(eval_err("nope", &ctx), "column not found: nope");
        assert_eq!(eval_err("users.nope", &ctx), "column not found: users.nope");
        assert_eq!(eval_err("other.id", &ctx), "column not found: other.id");

        let no_row = RowContext {
            table: "users",
            columns: &columns,
            values: None,
        };
        assert_eq!(eval_err("id", &no_row), "column not found: id");
        assert_eq!(eval_err("users.id", &no_row), "column not found: users.id");
        assert_eq!(eval_err("other.id", &no_row), "column not found: other.id");
    }
}
