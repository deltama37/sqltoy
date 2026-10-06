//! Abstract syntax tree for the SQL dialect this crate parses.
//!
//! Nodes do not store source positions. [`Display`] writes each node as SQL
//! that parses back to the same tree: keywords are uppercase, identifiers
//! keep their spelling, and every unary, binary, and `IS [NOT] NULL`
//! expression is parenthesized.

use std::fmt;

use crate::catalog::ColumnType;

/// One SQL statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    /// `CREATE TABLE`.
    CreateTable(CreateTable),
    /// `INSERT`.
    Insert(Insert),
    /// `SELECT`.
    Select(Select),
    /// `UPDATE`.
    Update(Update),
    /// `DELETE`.
    Delete(Delete),
}

/// `CREATE TABLE name (column type, ...)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateTable {
    /// Table name, with the spelling from the source.
    pub name: String,
    /// Column definitions, in order. At least one.
    pub columns: Vec<ColumnDef>,
}

/// One column in a [`CreateTable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    /// Column name, with the spelling from the source.
    pub name: String,
    /// `INTEGER` or `TEXT`.
    pub column_type: ColumnType,
}

/// `INSERT INTO table [(columns)] VALUES (...), ...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Insert {
    /// Table name.
    pub table: String,
    /// Column list when one was written, in order.
    pub columns: Option<Vec<String>>,
    /// Value rows. At least one row, and each row has at least one expression.
    pub rows: Vec<Vec<Expr>>,
}

/// `SELECT items FROM table [WHERE expr]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Select {
    /// Select list, in order. At least one item.
    pub items: Vec<SelectItem>,
    /// Table name in the `FROM` clause.
    pub from: String,
    /// `WHERE` expression when the clause was written.
    pub filter: Option<Expr>,
}

/// One item in a [`Select`] list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectItem {
    /// `*`.
    Wildcard,
    /// An expression and an optional output name.
    Expr {
        /// Selected expression.
        expr: Expr,
        /// Name introduced by `AS` or by a bare identifier.
        alias: Option<String>,
    },
}

/// `UPDATE table SET ... [WHERE expr]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    /// Table name.
    pub table: String,
    /// Assignments, in order. At least one.
    pub assignments: Vec<Assignment>,
    /// `WHERE` expression when the clause was written.
    pub filter: Option<Expr>,
}

/// One `column = expr` assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    /// Column name.
    pub column: String,
    /// Value expression.
    pub value: Expr,
}

/// `DELETE FROM table [WHERE expr]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delete {
    /// Table name.
    pub table: String,
    /// `WHERE` expression when the clause was written.
    pub filter: Option<Expr>,
}

/// A scalar expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// A literal.
    Literal(Literal),
    /// A column reference, optionally qualified with a table name.
    Column(ColumnRef),
    /// A prefix operator applied to one expression.
    Unary {
        /// Operator.
        op: UnaryOp,
        /// Operand.
        expr: Box<Expr>,
    },
    /// A binary operator applied to two expressions.
    Binary {
        /// Operator.
        op: BinaryOp,
        /// Left operand.
        left: Box<Expr>,
        /// Right operand.
        right: Box<Expr>,
    },
    /// `IS NULL` or `IS NOT NULL`.
    IsNull {
        /// Tested expression.
        expr: Box<Expr>,
        /// `true` for `IS NOT NULL`.
        negated: bool,
    },
}

/// A literal value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Literal {
    /// `NULL`.
    Null,
    /// `TRUE` or `FALSE`.
    Boolean(bool),
    /// Decimal integer that fits in `i64`.
    ///
    /// The parser only builds non-negative values. A minus sign is
    /// [`UnaryOp::Neg`], so `i64::MIN` is not a single literal.
    Integer(i64),
    /// Text, without the surrounding quotes.
    Text(String),
}

/// A reference to a column, optionally qualified by a table name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnRef {
    /// Qualifier before `.`, when one was written.
    pub table: Option<String>,
    /// Column name.
    pub column: String,
}

/// Prefix operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnaryOp {
    /// `NOT`.
    Not,
    /// Arithmetic negation.
    Neg,
}

/// Binary operator. `!=` and `<>` are both [`BinaryOp::NotEq`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinaryOp {
    /// `OR`.
    Or,
    /// `AND`.
    And,
    /// `=`.
    Eq,
    /// `<>` or `!=`.
    NotEq,
    /// `<`.
    Lt,
    /// `<=`.
    LtEq,
    /// `>`.
    Gt,
    /// `>=`.
    GtEq,
    /// `+`.
    Add,
    /// `-`.
    Sub,
    /// `*`.
    Mul,
    /// `/`.
    Div,
    /// `%`.
    Mod,
}

impl fmt::Display for Statement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Statement::CreateTable(statement) => write!(f, "{statement}"),
            Statement::Insert(statement) => write!(f, "{statement}"),
            Statement::Select(statement) => write!(f, "{statement}"),
            Statement::Update(statement) => write!(f, "{statement}"),
            Statement::Delete(statement) => write!(f, "{statement}"),
        }
    }
}

impl fmt::Display for CreateTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CREATE TABLE {} (", self.name)?;
        write_comma_sep(f, &self.columns)?;
        write!(f, ")")
    }
}

impl fmt::Display for ColumnDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.name, self.column_type)
    }
}

impl fmt::Display for Insert {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "INSERT INTO {}", self.table)?;
        if let Some(columns) = &self.columns {
            write!(f, " (")?;
            write_comma_sep(f, columns)?;
            write!(f, ")")?;
        }
        write!(f, " VALUES ")?;
        for (index, row) in self.rows.iter().enumerate() {
            if index > 0 {
                write!(f, ", ")?;
            }
            write_parenthesized(f, row)?;
        }
        Ok(())
    }
}

impl fmt::Display for Select {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SELECT ")?;
        write_comma_sep(f, &self.items)?;
        write!(f, " FROM {}", self.from)?;
        write_where(f, self.filter.as_ref())
    }
}

impl fmt::Display for SelectItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SelectItem::Wildcard => f.write_str("*"),
            SelectItem::Expr { expr, alias } => {
                write!(f, "{expr}")?;
                if let Some(alias) = alias {
                    write!(f, " AS {alias}")?;
                }
                Ok(())
            }
        }
    }
}

impl fmt::Display for Update {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UPDATE {} SET ", self.table)?;
        write_comma_sep(f, &self.assignments)?;
        write_where(f, self.filter.as_ref())
    }
}

impl fmt::Display for Assignment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} = {}", self.column, self.value)
    }
}

impl fmt::Display for Delete {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DELETE FROM {}", self.table)?;
        write_where(f, self.filter.as_ref())
    }
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Literal(literal) => write!(f, "{literal}"),
            Expr::Column(column) => write!(f, "{column}"),
            // The space after `-` keeps a literal operand a separate token,
            // so `(- 5)` parses as negation of `5` rather than as one integer.
            Expr::Unary { op, expr } => match op {
                UnaryOp::Not => write!(f, "(NOT {expr})"),
                UnaryOp::Neg => write!(f, "(- {expr})"),
            },
            Expr::Binary { op, left, right } => write!(f, "({left} {op} {right})"),
            Expr::IsNull { expr, negated } => {
                if *negated {
                    write!(f, "({expr} IS NOT NULL)")
                } else {
                    write!(f, "({expr} IS NULL)")
                }
            }
        }
    }
}

impl fmt::Display for Literal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Literal::Null => f.write_str("NULL"),
            Literal::Boolean(true) => f.write_str("TRUE"),
            Literal::Boolean(false) => f.write_str("FALSE"),
            Literal::Integer(value) => write!(f, "{value}"),
            Literal::Text(value) => write!(f, "'{}'", value.replace('\'', "''")),
        }
    }
}

impl fmt::Display for ColumnRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.table {
            Some(table) => write!(f, "{table}.{}", self.column),
            None => write!(f, "{}", self.column),
        }
    }
}

impl fmt::Display for UnaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            UnaryOp::Not => "NOT",
            UnaryOp::Neg => "-",
        })
    }
}

impl fmt::Display for BinaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            BinaryOp::Or => "OR",
            BinaryOp::And => "AND",
            BinaryOp::Eq => "=",
            BinaryOp::NotEq => "<>",
            BinaryOp::Lt => "<",
            BinaryOp::LtEq => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::GtEq => ">=",
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Mod => "%",
        })
    }
}

/// Joins statements with `"; "` and does not add a trailing semicolon.
///
/// An empty slice is an empty string. Parsing the result yields the same
/// statements.
pub fn format_statements(statements: &[Statement]) -> String {
    let mut out = String::new();
    for (index, statement) in statements.iter().enumerate() {
        if index > 0 {
            out.push_str("; ");
        }
        out.push_str(&statement.to_string());
    }
    out
}

fn write_where(f: &mut fmt::Formatter<'_>, filter: Option<&Expr>) -> fmt::Result {
    if let Some(filter) = filter {
        write!(f, " WHERE {filter}")?;
    }
    Ok(())
}

fn write_comma_sep<T: fmt::Display>(f: &mut fmt::Formatter<'_>, items: &[T]) -> fmt::Result {
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            f.write_str(", ")?;
        }
        write!(f, "{item}")?;
    }
    Ok(())
}

fn write_parenthesized<T: fmt::Display>(f: &mut fmt::Formatter<'_>, items: &[T]) -> fmt::Result {
    write!(f, "(")?;
    write_comma_sep(f, items)?;
    write!(f, ")")
}
