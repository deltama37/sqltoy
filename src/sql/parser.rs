//! Recursive-descent parser.
//!
//! Expression precedence, from low to high, is `OR`, `AND`, prefix `NOT`,
//! comparison and `IS [NOT] NULL`, `+` and `-`, `*` `/` `%`, then unary
//! minus. Binary operators are left-associative. A comparison or `IS` does
//! not chain with another comparison or `IS`.

use std::io;

use crate::catalog::ColumnType;

use super::ast::{
    Assignment, BinaryOp, ColumnDef, ColumnRef, CreateTable, Delete, Expr, Insert, Literal, Select,
    SelectItem, Statement, UnaryOp, Update,
};
use super::lexer::{syntax_error, tokenize, Keyword, Token, TokenKind};

/// Parses `sql` into statements.
///
/// Whitespace and `--` comments are ignored. The result is empty when the
/// input is empty or contains only whitespace, comments, and semicolons.
/// Semicolons may repeat, and the semicolon after the last statement is
/// optional.
///
/// A failure is [`std::io::ErrorKind::InvalidInput`]. The message is
/// `syntax error at {line}:{column}: {message}`, with 1-based line and column.
/// Columns count Unicode scalar values. The position is the offending token,
/// or one column past the last character at end of input. An integer literal
/// that does not fit in `i64` is an error, so `-9223372036854775808` cannot
/// be written: the digits overflow before unary minus applies.
pub fn parse(sql: &str) -> io::Result<Vec<Statement>> {
    let tokens = tokenize(sql)?;
    Parser {
        tokens,
        index: 0,
        depth: 0,
    }
    .parse_statements()
}

/// Deepest nesting of parentheses, `NOT`, and unary minus that is accepted.
///
/// The parser recurses once per level, so unbounded input could overflow the
/// stack.
const MAX_DEPTH: usize = 128;

struct Parser {
    tokens: Vec<Token>,
    index: usize,
    depth: usize,
}

impl Parser {
    fn parse_statements(&mut self) -> io::Result<Vec<Statement>> {
        let mut statements = Vec::new();
        self.skip_semicolons();
        while !matches!(self.kind(), TokenKind::Eof) {
            statements.push(self.parse_statement()?);
            if matches!(self.kind(), TokenKind::Semicolon) {
                self.skip_semicolons();
            } else if !matches!(self.kind(), TokenKind::Eof) {
                return Err(self.expected("';'"));
            }
        }
        Ok(statements)
    }

    fn parse_statement(&mut self) -> io::Result<Statement> {
        if self.at_keyword(Keyword::Create) {
            Ok(Statement::CreateTable(self.parse_create()?))
        } else if self.at_keyword(Keyword::Insert) {
            Ok(Statement::Insert(self.parse_insert()?))
        } else if self.at_keyword(Keyword::Select) {
            Ok(Statement::Select(self.parse_select()?))
        } else if self.at_keyword(Keyword::Update) {
            Ok(Statement::Update(self.parse_update()?))
        } else if self.at_keyword(Keyword::Delete) {
            Ok(Statement::Delete(self.parse_delete()?))
        } else {
            Err(self.expected("statement"))
        }
    }

    fn parse_create(&mut self) -> io::Result<CreateTable> {
        self.expect_keyword(Keyword::Create)?;
        self.expect_keyword(Keyword::Table)?;
        let name = self.expect_ident()?;
        self.expect(TokenKind::LParen, "'('")?;
        let columns = self.parse_list(|parser| parser.parse_column_def())?;
        self.expect(TokenKind::RParen, "')'")?;
        Ok(CreateTable { name, columns })
    }

    fn parse_column_def(&mut self) -> io::Result<ColumnDef> {
        let name = self.expect_ident()?;
        let column_type = self.parse_type()?;
        let primary_key = if self.eat_keyword(Keyword::Primary) {
            self.expect_keyword(Keyword::Key)?;
            true
        } else {
            false
        };
        Ok(ColumnDef {
            name,
            column_type,
            primary_key,
        })
    }

    fn parse_type(&mut self) -> io::Result<ColumnType> {
        if self.eat_keyword(Keyword::Integer) {
            Ok(ColumnType::Integer)
        } else if self.eat_keyword(Keyword::Text) {
            Ok(ColumnType::Text)
        } else {
            Err(self.expected("type"))
        }
    }

    fn parse_insert(&mut self) -> io::Result<Insert> {
        self.expect_keyword(Keyword::Insert)?;
        self.expect_keyword(Keyword::Into)?;
        let table = self.expect_ident()?;
        let columns = if self.eat(TokenKind::LParen) {
            let columns = self.parse_list(|parser| parser.expect_ident())?;
            self.expect(TokenKind::RParen, "')'")?;
            Some(columns)
        } else {
            None
        };
        self.expect_keyword(Keyword::Values)?;
        let rows = self.parse_list(|parser| parser.parse_row())?;
        Ok(Insert {
            table,
            columns,
            rows,
        })
    }

    fn parse_row(&mut self) -> io::Result<Vec<Expr>> {
        self.expect(TokenKind::LParen, "'('")?;
        let exprs = self.parse_list(|parser| parser.parse_expr())?;
        self.expect(TokenKind::RParen, "')'")?;
        Ok(exprs)
    }

    fn parse_select(&mut self) -> io::Result<Select> {
        self.expect_keyword(Keyword::Select)?;
        let items = self.parse_list(|parser| parser.parse_select_item())?;
        self.expect_keyword(Keyword::From)?;
        let from = self.expect_ident()?;
        let filter = self.parse_optional_where()?;
        Ok(Select {
            items,
            from,
            filter,
        })
    }

    fn parse_select_item(&mut self) -> io::Result<SelectItem> {
        if self.eat(TokenKind::Star) {
            return Ok(SelectItem::Wildcard);
        }
        let expr = self.parse_expr()?;
        let alias = if self.at_keyword(Keyword::As) || matches!(self.kind(), TokenKind::Ident(_)) {
            self.eat_keyword(Keyword::As);
            Some(self.expect_ident()?)
        } else {
            None
        };
        Ok(SelectItem::Expr { expr, alias })
    }

    fn parse_update(&mut self) -> io::Result<Update> {
        self.expect_keyword(Keyword::Update)?;
        let table = self.expect_ident()?;
        self.expect_keyword(Keyword::Set)?;
        let assignments = self.parse_list(|parser| parser.parse_assignment())?;
        let filter = self.parse_optional_where()?;
        Ok(Update {
            table,
            assignments,
            filter,
        })
    }

    fn parse_assignment(&mut self) -> io::Result<Assignment> {
        let column = self.expect_ident()?;
        self.expect(TokenKind::Eq, "'='")?;
        let value = self.parse_expr()?;
        Ok(Assignment { column, value })
    }

    fn parse_delete(&mut self) -> io::Result<Delete> {
        self.expect_keyword(Keyword::Delete)?;
        self.expect_keyword(Keyword::From)?;
        let table = self.expect_ident()?;
        let filter = self.parse_optional_where()?;
        Ok(Delete { table, filter })
    }

    fn parse_optional_where(&mut self) -> io::Result<Option<Expr>> {
        if self.eat_keyword(Keyword::Where) {
            Ok(Some(self.parse_expr()?))
        } else {
            Ok(None)
        }
    }

    fn parse_expr(&mut self) -> io::Result<Expr> {
        self.nested(|parser| parser.parse_or())
    }

    fn nested<T>(&mut self, inner: impl FnOnce(&mut Self) -> io::Result<T>) -> io::Result<T> {
        if self.depth >= MAX_DEPTH {
            return Err(self.error_here("expression nested too deeply"));
        }
        self.depth += 1;
        let result = inner(self);
        self.depth -= 1;
        result
    }

    fn parse_or(&mut self) -> io::Result<Expr> {
        let mut expr = self.parse_and()?;
        while self.eat_keyword(Keyword::Or) {
            let right = self.parse_and()?;
            expr = binary(BinaryOp::Or, expr, right);
        }
        Ok(expr)
    }

    fn parse_and(&mut self) -> io::Result<Expr> {
        let mut expr = self.parse_not()?;
        while self.eat_keyword(Keyword::And) {
            let right = self.parse_not()?;
            expr = binary(BinaryOp::And, expr, right);
        }
        Ok(expr)
    }

    fn parse_not(&mut self) -> io::Result<Expr> {
        if self.eat_keyword(Keyword::Not) {
            let expr = self.nested(|parser| parser.parse_not())?;
            Ok(Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(expr),
            })
        } else {
            self.parse_cmp()
        }
    }

    fn parse_cmp(&mut self) -> io::Result<Expr> {
        let left = self.parse_add()?;
        if let Some(op) = self.comparison_op() {
            self.index += 1;
            let right = self.parse_add()?;
            self.reject_chained_comparison()?;
            return Ok(binary(op, left, right));
        }
        if self.eat_keyword(Keyword::Is) {
            let negated = self.eat_keyword(Keyword::Not);
            self.expect_keyword(Keyword::Null)?;
            self.reject_chained_comparison()?;
            return Ok(Expr::IsNull {
                expr: Box::new(left),
                negated,
            });
        }
        Ok(left)
    }

    fn parse_add(&mut self) -> io::Result<Expr> {
        let mut expr = self.parse_mul()?;
        loop {
            let op = match self.kind() {
                TokenKind::Plus => BinaryOp::Add,
                TokenKind::Minus => BinaryOp::Sub,
                _ => break,
            };
            self.index += 1;
            let right = self.parse_mul()?;
            expr = binary(op, expr, right);
        }
        Ok(expr)
    }

    fn parse_mul(&mut self) -> io::Result<Expr> {
        let mut expr = self.parse_unary()?;
        loop {
            let op = match self.kind() {
                TokenKind::Star => BinaryOp::Mul,
                TokenKind::Slash => BinaryOp::Div,
                TokenKind::Percent => BinaryOp::Mod,
                _ => break,
            };
            self.index += 1;
            let right = self.parse_unary()?;
            expr = binary(op, expr, right);
        }
        Ok(expr)
    }

    fn parse_unary(&mut self) -> io::Result<Expr> {
        if matches!(self.kind(), TokenKind::Minus) {
            self.index += 1;
            let expr = self.nested(|parser| parser.parse_unary())?;
            Ok(Expr::Unary {
                op: UnaryOp::Neg,
                expr: Box::new(expr),
            })
        } else {
            self.parse_primary()
        }
    }

    fn parse_primary(&mut self) -> io::Result<Expr> {
        let kind = self.kind().clone();
        match kind {
            TokenKind::Integer(value) => {
                self.index += 1;
                Ok(Expr::Literal(Literal::Integer(value)))
            }
            TokenKind::String(value) => {
                self.index += 1;
                Ok(Expr::Literal(Literal::Text(value)))
            }
            TokenKind::Keyword(Keyword::True) => {
                self.index += 1;
                Ok(Expr::Literal(Literal::Boolean(true)))
            }
            TokenKind::Keyword(Keyword::False) => {
                self.index += 1;
                Ok(Expr::Literal(Literal::Boolean(false)))
            }
            TokenKind::Keyword(Keyword::Null) => {
                self.index += 1;
                Ok(Expr::Literal(Literal::Null))
            }
            TokenKind::Ident(name) => {
                self.index += 1;
                if self.eat(TokenKind::Dot) {
                    let column = self.expect_ident()?;
                    Ok(Expr::Column(ColumnRef {
                        table: Some(name),
                        column,
                    }))
                } else {
                    Ok(Expr::Column(ColumnRef {
                        table: None,
                        column: name,
                    }))
                }
            }
            TokenKind::LParen => {
                self.index += 1;
                let expr = self.parse_expr()?;
                self.expect(TokenKind::RParen, "')'")?;
                Ok(expr)
            }
            _ => Err(self.expected("expression")),
        }
    }

    fn reject_chained_comparison(&self) -> io::Result<()> {
        if self.comparison_op().is_some() || self.at_keyword(Keyword::Is) {
            Err(self.error_here("chained comparison"))
        } else {
            Ok(())
        }
    }

    fn comparison_op(&self) -> Option<BinaryOp> {
        match self.kind() {
            TokenKind::Eq => Some(BinaryOp::Eq),
            TokenKind::NotEq | TokenKind::BangEq => Some(BinaryOp::NotEq),
            TokenKind::Lt => Some(BinaryOp::Lt),
            TokenKind::LtEq => Some(BinaryOp::LtEq),
            TokenKind::Gt => Some(BinaryOp::Gt),
            TokenKind::GtEq => Some(BinaryOp::GtEq),
            _ => None,
        }
    }

    fn parse_list<T>(
        &mut self,
        mut item: impl FnMut(&mut Self) -> io::Result<T>,
    ) -> io::Result<Vec<T>> {
        let mut items = Vec::new();
        loop {
            items.push(item(self)?);
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        Ok(items)
    }

    fn skip_semicolons(&mut self) {
        while self.eat(TokenKind::Semicolon) {}
    }

    fn kind(&self) -> &TokenKind {
        &self.tokens[self.index].kind
    }

    fn current(&self) -> &Token {
        &self.tokens[self.index]
    }

    fn at_keyword(&self, keyword: Keyword) -> bool {
        self.kind() == &TokenKind::Keyword(keyword)
    }

    fn eat(&mut self, kind: TokenKind) -> bool {
        if self.kind() == &kind {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn eat_keyword(&mut self, keyword: Keyword) -> bool {
        self.eat(TokenKind::Keyword(keyword))
    }

    fn expect(&mut self, kind: TokenKind, expected: &str) -> io::Result<()> {
        if self.eat(kind) {
            Ok(())
        } else {
            Err(self.expected(expected))
        }
    }

    fn expect_keyword(&mut self, keyword: Keyword) -> io::Result<()> {
        if self.eat_keyword(keyword) {
            Ok(())
        } else {
            Err(self.expected(keyword.as_str()))
        }
    }

    fn expect_ident(&mut self) -> io::Result<String> {
        let name = match self.kind() {
            TokenKind::Ident(name) => name.clone(),
            _ => return Err(self.expected("identifier")),
        };
        self.index += 1;
        Ok(name)
    }

    fn expected(&self, expected: &str) -> io::Error {
        let token = self.current();
        syntax_error(
            token.line,
            token.column,
            format!("expected {expected}, found {}", token.kind.describe()),
        )
    }

    fn error_here(&self, message: &str) -> io::Error {
        let token = self.current();
        syntax_error(token.line, token.column, message)
    }
}

fn binary(op: BinaryOp, left: Expr, right: Expr) -> Expr {
    Expr::Binary {
        op,
        left: Box::new(left),
        right: Box::new(right),
    }
}

#[cfg(test)]
mod tests {
    use super::parse;
    use crate::catalog::ColumnType;
    use crate::sql::{
        format_statements, Assignment, BinaryOp, ColumnDef, ColumnRef, CreateTable, Delete, Expr,
        Insert, Literal, Select, SelectItem, Statement, UnaryOp, Update,
    };
    use std::io::ErrorKind;

    fn parse_one(sql: &str) -> Statement {
        match parse(sql) {
            Ok(mut statements) => {
                assert_eq!(statements.len(), 1, "{sql}");
                statements.pop().unwrap()
            }
            Err(err) => panic!("parse {sql}: {err}"),
        }
    }

    #[test]
    fn deep_nesting_is_an_error_not_a_stack_overflow() {
        let ok = format!("SELECT {}1{} FROM t", "(".repeat(100), ")".repeat(100));
        parse_one(&ok);
        for sql in [
            format!("SELECT {}1{} FROM t", "(".repeat(5000), ")".repeat(5000)),
            format!("SELECT {}1 FROM t", "- ".repeat(5000)),
            format!("SELECT * FROM t WHERE {}a", "NOT ".repeat(5000)),
        ] {
            let err = parse(&sql).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidInput);
            assert!(
                err.to_string().ends_with("expression nested too deeply"),
                "{err}"
            );
        }
    }

    #[test]
    fn crlf_line_endings_are_whitespace() {
        assert_eq!(
            parse_one("SELECT a\r\nFROM t\r\n"),
            parse_one("SELECT a FROM t")
        );
        assert_parse_err("SELECT a\r\nWHERE", 2, 1, "expected FROM, found WHERE");
    }

    fn assert_parse_err(sql: &str, line: u32, column: u32, message: &str) {
        let err = parse(sql).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            err.to_string(),
            format!("syntax error at {line}:{column}: {message}")
        );
    }

    fn col(name: &str) -> Expr {
        Expr::Column(ColumnRef {
            table: None,
            column: name.to_string(),
        })
    }

    fn table_col(table: &str, column: &str) -> Expr {
        Expr::Column(ColumnRef {
            table: Some(table.to_string()),
            column: column.to_string(),
        })
    }

    fn lit_int(value: i64) -> Expr {
        Expr::Literal(Literal::Integer(value))
    }

    fn lit_text(value: &str) -> Expr {
        Expr::Literal(Literal::Text(value.to_string()))
    }

    fn neg(expr: Expr) -> Expr {
        Expr::Unary {
            op: UnaryOp::Neg,
            expr: Box::new(expr),
        }
    }

    fn not(expr: Expr) -> Expr {
        Expr::Unary {
            op: UnaryOp::Not,
            expr: Box::new(expr),
        }
    }

    fn bin(op: BinaryOp, left: Expr, right: Expr) -> Expr {
        Expr::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    fn is_null(expr: Expr, negated: bool) -> Expr {
        Expr::IsNull {
            expr: Box::new(expr),
            negated,
        }
    }

    fn expr_of(sql: &str) -> Expr {
        let Statement::Select(mut select) = parse_one(sql) else {
            panic!("expected select: {sql}");
        };
        assert!(select.filter.is_none(), "{sql}");
        match select.items.pop() {
            Some(SelectItem::Expr { expr, alias: None }) if select.items.is_empty() => expr,
            item => panic!("expected one expression in {sql}, got {item:?}"),
        }
    }

    #[test]
    fn empty_input_and_semicolons_only() {
        for sql in ["", "   ", "\n\t", ";", ";;", "; ;", "-- only\n", "-- c\n;"] {
            assert_eq!(parse(sql).unwrap(), Vec::<Statement>::new(), "{sql:?}");
        }
    }

    #[test]
    fn create_table_insert_select_update_delete() {
        assert_eq!(
            parse_one("CREATE TABLE users (id INTEGER, name TEXT)"),
            Statement::CreateTable(CreateTable {
                name: "users".to_string(),
                columns: vec![
                    ColumnDef {
                        name: "id".to_string(),
                        column_type: ColumnType::Integer,
                        primary_key: false,
                    },
                    ColumnDef {
                        name: "name".to_string(),
                        column_type: ColumnType::Text,
                        primary_key: false,
                    },
                ],
            })
        );
        assert_eq!(
            parse_one("CrEaTe TaBlE Users (Id integer)"),
            Statement::CreateTable(CreateTable {
                name: "Users".to_string(),
                columns: vec![ColumnDef {
                    name: "Id".to_string(),
                    column_type: ColumnType::Integer,
                    primary_key: false,
                }],
            })
        );

        assert_eq!(
            parse_one("INSERT INTO users VALUES (1, 'Alice')"),
            Statement::Insert(Insert {
                table: "users".to_string(),
                columns: None,
                rows: vec![vec![lit_int(1), lit_text("Alice")]],
            })
        );
        assert_eq!(
            parse_one("INSERT INTO users (id, name) VALUES (1, 'a'), (2, 'b')"),
            Statement::Insert(Insert {
                table: "users".to_string(),
                columns: Some(vec!["id".to_string(), "name".to_string()]),
                rows: vec![
                    vec![lit_int(1), lit_text("a")],
                    vec![lit_int(2), lit_text("b")],
                ],
            })
        );
        assert_eq!(
            parse_one("INSERT INTO t VALUES (NULL, TRUE, FALSE, 'it''s')"),
            Statement::Insert(Insert {
                table: "t".to_string(),
                columns: None,
                rows: vec![vec![
                    Expr::Literal(Literal::Null),
                    Expr::Literal(Literal::Boolean(true)),
                    Expr::Literal(Literal::Boolean(false)),
                    lit_text("it's"),
                ]],
            })
        );

        assert_eq!(
            parse_one("SELECT * FROM users"),
            Statement::Select(Select {
                items: vec![SelectItem::Wildcard],
                from: "users".to_string(),
                filter: None,
            })
        );
        assert_eq!(
            parse_one("SELECT id AS user_id, name n, users.age FROM Users"),
            Statement::Select(Select {
                items: vec![
                    SelectItem::Expr {
                        expr: col("id"),
                        alias: Some("user_id".to_string()),
                    },
                    SelectItem::Expr {
                        expr: col("name"),
                        alias: Some("n".to_string()),
                    },
                    SelectItem::Expr {
                        expr: table_col("users", "age"),
                        alias: None,
                    },
                ],
                from: "Users".to_string(),
                filter: None,
            })
        );
        assert_eq!(
            parse_one("select * from t where id = 1"),
            Statement::Select(Select {
                items: vec![SelectItem::Wildcard],
                from: "t".to_string(),
                filter: Some(bin(BinaryOp::Eq, col("id"), lit_int(1))),
            })
        );

        assert_eq!(
            parse_one("UPDATE users SET name = 'A', age = age + 1 WHERE id = 1"),
            Statement::Update(Update {
                table: "users".to_string(),
                assignments: vec![
                    Assignment {
                        column: "name".to_string(),
                        value: lit_text("A"),
                    },
                    Assignment {
                        column: "age".to_string(),
                        value: bin(BinaryOp::Add, col("age"), lit_int(1)),
                    },
                ],
                filter: Some(bin(BinaryOp::Eq, col("id"), lit_int(1))),
            })
        );
        assert_eq!(
            parse_one("UPDATE users SET name = 'x'"),
            Statement::Update(Update {
                table: "users".to_string(),
                assignments: vec![Assignment {
                    column: "name".to_string(),
                    value: lit_text("x"),
                }],
                filter: None,
            })
        );

        assert_eq!(
            parse_one("DELETE FROM users WHERE id <> 1"),
            Statement::Delete(Delete {
                table: "users".to_string(),
                filter: Some(bin(BinaryOp::NotEq, col("id"), lit_int(1))),
            })
        );
        assert_eq!(
            parse_one("DELETE FROM users"),
            Statement::Delete(Delete {
                table: "users".to_string(),
                filter: None,
            })
        );
        assert_eq!(
            expr_of("SELECT a != b FROM t"),
            expr_of("SELECT a <> b FROM t")
        );
    }

    #[test]
    fn comments_and_repeated_semicolons_separate_statements() {
        let sql = "\
-- header
CREATE TABLE users (
  id INTEGER, -- key
  name TEXT
);
;;
SELECT -- star
  * FROM users;;
DELETE FROM users";
        let statements = parse(sql).unwrap();
        assert_eq!(statements.len(), 3);
        assert!(matches!(statements[0], Statement::CreateTable(_)));
        assert!(matches!(statements[1], Statement::Select(_)));
        assert!(matches!(statements[2], Statement::Delete(_)));
        assert_eq!(
            parse("SELECT\t*\tFROM\tt").unwrap(),
            parse("SELECT * FROM t").unwrap()
        );
    }

    #[test]
    fn precedence_and_associativity() {
        assert_eq!(
            expr_of("SELECT 1 + 2 * 3 FROM t"),
            bin(
                BinaryOp::Add,
                lit_int(1),
                bin(BinaryOp::Mul, lit_int(2), lit_int(3))
            )
        );
        assert_eq!(
            expr_of("SELECT (1 + 2) * 3 FROM t"),
            bin(
                BinaryOp::Mul,
                bin(BinaryOp::Add, lit_int(1), lit_int(2)),
                lit_int(3)
            )
        );
        assert_eq!(
            expr_of("SELECT 1 + 2 * 3 - 4 / 5 % 2 FROM t"),
            bin(
                BinaryOp::Sub,
                bin(
                    BinaryOp::Add,
                    lit_int(1),
                    bin(BinaryOp::Mul, lit_int(2), lit_int(3))
                ),
                bin(
                    BinaryOp::Mod,
                    bin(BinaryOp::Div, lit_int(4), lit_int(5)),
                    lit_int(2)
                ),
            )
        );
        assert_eq!(
            expr_of("SELECT - a * b FROM t"),
            bin(BinaryOp::Mul, neg(col("a")), col("b"))
        );
        assert_eq!(expr_of("SELECT - - a FROM t"), neg(neg(col("a"))));
        assert_eq!(
            expr_of("SELECT -(1 + 2) FROM t"),
            neg(bin(BinaryOp::Add, lit_int(1), lit_int(2)))
        );
        assert_eq!(
            expr_of("SELECT a - b - c FROM t"),
            bin(
                BinaryOp::Sub,
                bin(BinaryOp::Sub, col("a"), col("b")),
                col("c")
            )
        );
        assert_eq!(
            expr_of("SELECT a * b * c FROM t"),
            bin(
                BinaryOp::Mul,
                bin(BinaryOp::Mul, col("a"), col("b")),
                col("c")
            )
        );
        assert_eq!(
            expr_of("SELECT NOT a = 1 AND b = 2 OR c FROM t"),
            bin(
                BinaryOp::Or,
                bin(
                    BinaryOp::And,
                    not(bin(BinaryOp::Eq, col("a"), lit_int(1))),
                    bin(BinaryOp::Eq, col("b"), lit_int(2)),
                ),
                col("c"),
            )
        );
        assert_eq!(
            expr_of("SELECT a OR b AND c FROM t"),
            bin(
                BinaryOp::Or,
                col("a"),
                bin(BinaryOp::And, col("b"), col("c")),
            )
        );
        assert_eq!(
            expr_of("SELECT a AND b OR c OR d FROM t"),
            bin(
                BinaryOp::Or,
                bin(
                    BinaryOp::Or,
                    bin(BinaryOp::And, col("a"), col("b")),
                    col("c"),
                ),
                col("d"),
            )
        );
        assert_eq!(
            expr_of("SELECT a IS NOT NULL AND b IS NULL FROM t"),
            bin(
                BinaryOp::And,
                is_null(col("a"), true),
                is_null(col("b"), false),
            )
        );
        assert_eq!(
            expr_of("SELECT (NOT a) = 1 FROM t"),
            bin(BinaryOp::Eq, not(col("a")), lit_int(1))
        );
        assert_eq!(
            expr_of("SELECT NOT a = 1 FROM t"),
            not(bin(BinaryOp::Eq, col("a"), lit_int(1)))
        );
        assert_eq!(
            expr_of("SELECT a = (NOT b) FROM t"),
            bin(BinaryOp::Eq, col("a"), not(col("b")))
        );
        assert_eq!(
            expr_of("SELECT - a IS NULL FROM t"),
            is_null(neg(col("a")), false)
        );
        assert_eq!(
            expr_of("SELECT 9223372036854775807 FROM t"),
            lit_int(i64::MAX)
        );
        assert_eq!(expr_of("SELECT 007 FROM t"), lit_int(7));
    }

    #[test]
    fn syntax_errors_report_position_and_message() {
        assert_parse_err("SELECT * WHERE id = 1", 1, 10, "expected FROM, found WHERE");
        assert_parse_err("SELECT *\nWHERE id = 1", 2, 1, "expected FROM, found WHERE");
        assert_parse_err(
            "SELECT * FROM t WHERE a < b < c",
            1,
            29,
            "chained comparison",
        );
        assert_parse_err(
            "SELECT * FROM t WHERE a = b IS NULL",
            1,
            29,
            "chained comparison",
        );
        assert_parse_err("SELECT 'abc", 1, 8, "unterminated string literal");
        assert_parse_err("SELECT @ FROM t", 1, 8, "unexpected character '@'");
        assert_parse_err("ä", 1, 1, "unexpected character 'ä'");
        assert_parse_err(
            "SELECT 9223372036854775808 FROM t",
            1,
            8,
            "integer literal out of range",
        );
        assert_parse_err(
            "SELECT -9223372036854775808 FROM t",
            1,
            9,
            "integer literal out of range",
        );
        assert_parse_err("-9223372036854775808", 1, 2, "integer literal out of range");
        assert_parse_err(
            "CREATE TABLE select (id INTEGER)",
            1,
            14,
            "expected identifier, found SELECT",
        );
        assert_parse_err(
            "CREATE TABLE users (select INTEGER)",
            1,
            21,
            "expected identifier, found SELECT",
        );
        assert_parse_err(
            "SELECT * FROM t garbage",
            1,
            17,
            "expected ';', found identifier \"garbage\"",
        );
        assert_parse_err(
            "SELECT * FROM",
            1,
            14,
            "expected identifier, found end of input",
        );
        assert_parse_err("SELECT", 1, 7, "expected expression, found end of input");
        assert_parse_err(
            "SELECT * FROM t;garbage",
            1,
            17,
            "expected statement, found identifier \"garbage\"",
        );
        assert_parse_err(
            &format!("SELECT {} FROM t", "a".repeat(65)),
            1,
            8,
            "identifier too long",
        );
        assert_parse_err("SELECT != FROM t", 1, 8, "expected expression, found '!='");
        assert_parse_err("SELECT <> FROM t", 1, 8, "expected expression, found '<>'");
        assert_parse_err(
            "CREATE TABLE t (id BLOB)",
            1,
            20,
            "expected type, found identifier \"BLOB\"",
        );
        assert_parse_err("SELECT 1.2 FROM t", 1, 9, "expected FROM, found '.'");
        assert_parse_err("SELECT t.* FROM t", 1, 10, "expected identifier, found '*'");
        assert_parse_err("SELECT * AS a FROM t", 1, 10, "expected FROM, found AS");
        assert_parse_err(
            "SELECT a = NOT b FROM t",
            1,
            12,
            "expected expression, found NOT",
        );
    }

    #[test]
    fn display_parenthesizes_and_round_trips() {
        assert_eq!(
            parse_one("SELECT 1 + 2 * 3 FROM t").to_string(),
            "SELECT (1 + (2 * 3)) FROM t"
        );
        assert_eq!(
            parse_one("SELECT -5 FROM t").to_string(),
            "SELECT (- 5) FROM t"
        );
        assert_eq!(
            parse_one("SELECT NOT a FROM t").to_string(),
            "SELECT (NOT a) FROM t"
        );
        assert_eq!(
            parse_one("SELECT a + 1 > 2 FROM t").to_string(),
            "SELECT ((a + 1) > 2) FROM t"
        );
        assert_eq!(
            parse_one("SELECT a IS NOT NULL FROM t").to_string(),
            "SELECT (a IS NOT NULL) FROM t"
        );
        assert_eq!(
            parse_one("create table users (id integer, name text)").to_string(),
            "CREATE TABLE users (id INTEGER, name TEXT)"
        );
        assert_eq!(
            parse_one("SELECT id user_id FROM t").to_string(),
            "SELECT id AS user_id FROM t"
        );
        let not_eq = parse_one("SELECT a != b FROM t");
        assert_eq!(not_eq.to_string(), "SELECT (a <> b) FROM t");
        assert!(!not_eq.to_string().contains("!="));

        let corpus = [
            "CREATE TABLE users (id INTEGER, name TEXT)",
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)",
            "CREATE TABLE T (_id INTEGER)",
            "CrEaTe TaBlE Users (Id integer)",
            "INSERT INTO users VALUES (1, 'Alice')",
            "INSERT INTO users (id, name) VALUES (1, 'a'), (2, 'b')",
            "INSERT INTO t VALUES (NULL, TRUE, FALSE)",
            "INSERT INTO t VALUES ('it''s', '')",
            "INSERT INTO t VALUES ('a\nb')",
            "INSERT INTO t VALUES (';')",
            "SELECT * FROM users",
            "SELECT id, name AS n FROM users",
            "SELECT id user_id, name FROM Users",
            "SELECT users.id, t.name FROM users",
            "SELECT * FROM users WHERE id = 1",
            "select * from users where id = 1",
            "SELECT NOT a = 1 AND b = 2 OR c FROM t",
            "SELECT 1 + 2 * 3 - 4 / 5 % 2 FROM t",
            "SELECT - a * b FROM t",
            "SELECT -5 FROM t",
            "SELECT a - b - c FROM t",
            "SELECT a IS NOT NULL AND b IS NULL FROM t",
            "SELECT (NOT a) = FALSE FROM t WHERE a >= 1 OR b < 2 AND c <= 3",
            "SELECT a != b, a <> c FROM t",
            "SELECT 007 FROM t",
            "UPDATE users SET name = 'A', age = age + 1 WHERE id <> 1",
            "UPDATE users SET name = 'x'",
            "DELETE FROM users WHERE id != 1",
            "DELETE FROM users",
            "SELECT * FROM t; INSERT INTO t VALUES (1)",
            "INSERT INTO t VALUES ('a\nb'); SELECT * FROM t",
            "-- c\nSELECT * FROM t -- tail",
            "SELECT NOT NULL FROM t",
            "SELECT (1 + 2) * - 3 FROM t",
        ];
        assert!(corpus.len() >= 20);
        for sql in corpus {
            assert_round_trip(sql);
        }
    }

    fn assert_round_trip(sql: &str) {
        let statements = parse(sql).unwrap_or_else(|err| panic!("{sql}: {err}"));
        assert!(!statements.is_empty(), "{sql}");
        for statement in &statements {
            let displayed = statement.to_string();
            assert!(!displayed.ends_with(';'), "{displayed}");
            let again = parse(&displayed).unwrap_or_else(|err| panic!("{displayed}: {err}"));
            assert_eq!(again, vec![statement.clone()], "{displayed}");
        }
        let joined = format_statements(&statements);
        let again = parse(&joined).unwrap_or_else(|err| panic!("{joined}: {err}"));
        assert_eq!(again, statements, "{joined}");
    }
}
