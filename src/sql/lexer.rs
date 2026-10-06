//! Lexer for the SQL dialect.
//!
//! Whitespace (space, tab, and newline) and `--` comments are separators.
//! Keywords are ASCII case-insensitive. Identifiers keep the source spelling.
//! A column is one Unicode scalar value, starting at 1.

use std::io;

/// One token and the position of its first character.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Token {
    pub(crate) kind: TokenKind,
    pub(crate) line: u32,
    pub(crate) column: u32,
}

/// Classification of a [`Token`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TokenKind {
    Keyword(Keyword),
    Ident(String),
    Integer(i64),
    String(String),
    LParen,
    RParen,
    Comma,
    Semicolon,
    Star,
    Dot,
    Eq,
    /// `<>`.
    NotEq,
    /// `!=`. Same operator as [`TokenKind::NotEq`] once parsed.
    BangEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Plus,
    Minus,
    Slash,
    Percent,
    Eof,
}

/// A reserved word. Comparison ignores ASCII case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Keyword {
    And,
    As,
    Asc,
    By,
    Create,
    Cross,
    Delete,
    Desc,
    False,
    From,
    Inner,
    Insert,
    Integer,
    Into,
    Is,
    Join,
    Key,
    Left,
    Limit,
    Not,
    Null,
    Offset,
    On,
    Or,
    Order,
    Outer,
    Primary,
    Select,
    Set,
    Table,
    Text,
    True,
    Update,
    Values,
    Where,
}

impl Keyword {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Keyword::And => "AND",
            Keyword::As => "AS",
            Keyword::Asc => "ASC",
            Keyword::By => "BY",
            Keyword::Create => "CREATE",
            Keyword::Cross => "CROSS",
            Keyword::Delete => "DELETE",
            Keyword::Desc => "DESC",
            Keyword::False => "FALSE",
            Keyword::From => "FROM",
            Keyword::Inner => "INNER",
            Keyword::Insert => "INSERT",
            Keyword::Integer => "INTEGER",
            Keyword::Into => "INTO",
            Keyword::Is => "IS",
            Keyword::Join => "JOIN",
            Keyword::Key => "KEY",
            Keyword::Left => "LEFT",
            Keyword::Limit => "LIMIT",
            Keyword::Not => "NOT",
            Keyword::Null => "NULL",
            Keyword::Offset => "OFFSET",
            Keyword::On => "ON",
            Keyword::Or => "OR",
            Keyword::Order => "ORDER",
            Keyword::Outer => "OUTER",
            Keyword::Primary => "PRIMARY",
            Keyword::Select => "SELECT",
            Keyword::Set => "SET",
            Keyword::Table => "TABLE",
            Keyword::Text => "TEXT",
            Keyword::True => "TRUE",
            Keyword::Update => "UPDATE",
            Keyword::Values => "VALUES",
            Keyword::Where => "WHERE",
        }
    }

    fn from_word(word: &str) -> Option<Keyword> {
        Some(match word.to_ascii_lowercase().as_str() {
            "and" => Keyword::And,
            "as" => Keyword::As,
            "asc" => Keyword::Asc,
            "by" => Keyword::By,
            "create" => Keyword::Create,
            "cross" => Keyword::Cross,
            "delete" => Keyword::Delete,
            "desc" => Keyword::Desc,
            "false" => Keyword::False,
            "from" => Keyword::From,
            "inner" => Keyword::Inner,
            "insert" => Keyword::Insert,
            "integer" => Keyword::Integer,
            "into" => Keyword::Into,
            "is" => Keyword::Is,
            "join" => Keyword::Join,
            "key" => Keyword::Key,
            "left" => Keyword::Left,
            "limit" => Keyword::Limit,
            "not" => Keyword::Not,
            "null" => Keyword::Null,
            "offset" => Keyword::Offset,
            "on" => Keyword::On,
            "or" => Keyword::Or,
            "order" => Keyword::Order,
            "outer" => Keyword::Outer,
            "primary" => Keyword::Primary,
            "select" => Keyword::Select,
            "set" => Keyword::Set,
            "table" => Keyword::Table,
            "text" => Keyword::Text,
            "true" => Keyword::True,
            "update" => Keyword::Update,
            "values" => Keyword::Values,
            "where" => Keyword::Where,
            _ => return None,
        })
    }
}

impl TokenKind {
    /// Describes a token in a syntax error.
    ///
    /// Keywords are uppercase. Identifiers are `identifier "name"`. Literals
    /// are `integer 5` or `string 'text'` with quotes doubled. Symbols are
    /// quoted, and end of input is the words `end of input`.
    pub(crate) fn describe(&self) -> String {
        match self {
            TokenKind::Keyword(keyword) => keyword.as_str().to_string(),
            TokenKind::Ident(name) => format!("identifier \"{name}\""),
            TokenKind::Integer(value) => format!("integer {value}"),
            TokenKind::String(value) => format!("string '{}'", value.replace('\'', "''")),
            TokenKind::LParen => "'('".to_string(),
            TokenKind::RParen => "')'".to_string(),
            TokenKind::Comma => "','".to_string(),
            TokenKind::Semicolon => "';'".to_string(),
            TokenKind::Star => "'*'".to_string(),
            TokenKind::Dot => "'.'".to_string(),
            TokenKind::Eq => "'='".to_string(),
            TokenKind::NotEq => "'<>'".to_string(),
            TokenKind::BangEq => "'!='".to_string(),
            TokenKind::Lt => "'<'".to_string(),
            TokenKind::LtEq => "'<='".to_string(),
            TokenKind::Gt => "'>'".to_string(),
            TokenKind::GtEq => "'>='".to_string(),
            TokenKind::Plus => "'+'".to_string(),
            TokenKind::Minus => "'-'".to_string(),
            TokenKind::Slash => "'/'".to_string(),
            TokenKind::Percent => "'%'".to_string(),
            TokenKind::Eof => "end of input".to_string(),
        }
    }
}

/// `syntax error at {line}:{column}: {message}`.
pub(crate) fn syntax_error(line: u32, column: u32, message: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("syntax error at {line}:{column}: {message}"),
    )
}

pub(crate) fn tokenize(sql: &str) -> io::Result<Vec<Token>> {
    let mut lexer = Lexer::new(sql);
    let mut tokens = Vec::new();
    loop {
        let token = lexer.next_token()?;
        let done = matches!(token.kind, TokenKind::Eof);
        tokens.push(token);
        if done {
            return Ok(tokens);
        }
    }
}

struct Lexer<'a> {
    source: &'a str,
    index: usize,
    line: u32,
    column: u32,
}

impl<'a> Lexer<'a> {
    fn new(source: &'a str) -> Lexer<'a> {
        Lexer {
            source,
            index: 0,
            line: 1,
            column: 1,
        }
    }

    fn peek_char(&self) -> Option<char> {
        self.source[self.index..].chars().next()
    }

    fn peek_second(&self) -> Option<char> {
        let mut chars = self.source[self.index..].chars();
        chars.next()?;
        chars.next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek_char()?;
        self.index += c.len_utf8();
        if c == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(c)
    }

    fn skip_trivia(&mut self) {
        loop {
            match self.peek_char() {
                Some(c) if is_whitespace(c) => {
                    self.bump();
                }
                Some('-') if self.peek_second() == Some('-') => {
                    self.bump();
                    self.bump();
                    while let Some(c) = self.peek_char() {
                        if c == '\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                _ => break,
            }
        }
    }

    fn next_token(&mut self) -> io::Result<Token> {
        self.skip_trivia();
        let line = self.line;
        let column = self.column;
        let Some(c) = self.peek_char() else {
            return Ok(Token {
                kind: TokenKind::Eof,
                line,
                column,
            });
        };
        let kind = match c {
            '(' => {
                self.bump();
                TokenKind::LParen
            }
            ')' => {
                self.bump();
                TokenKind::RParen
            }
            ',' => {
                self.bump();
                TokenKind::Comma
            }
            ';' => {
                self.bump();
                TokenKind::Semicolon
            }
            '*' => {
                self.bump();
                TokenKind::Star
            }
            '.' => {
                self.bump();
                TokenKind::Dot
            }
            '=' => {
                self.bump();
                TokenKind::Eq
            }
            '+' => {
                self.bump();
                TokenKind::Plus
            }
            '/' => {
                self.bump();
                TokenKind::Slash
            }
            '%' => {
                self.bump();
                TokenKind::Percent
            }
            '-' => {
                self.bump();
                TokenKind::Minus
            }
            '<' => {
                self.bump();
                match self.peek_char() {
                    Some('>') => {
                        self.bump();
                        TokenKind::NotEq
                    }
                    Some('=') => {
                        self.bump();
                        TokenKind::LtEq
                    }
                    _ => TokenKind::Lt,
                }
            }
            '>' => {
                self.bump();
                if self.peek_char() == Some('=') {
                    self.bump();
                    TokenKind::GtEq
                } else {
                    TokenKind::Gt
                }
            }
            '!' => {
                self.bump();
                if self.peek_char() == Some('=') {
                    self.bump();
                    TokenKind::BangEq
                } else {
                    return Err(syntax_error(line, column, "unexpected character '!'"));
                }
            }
            '\'' => return self.string(line, column),
            c if c.is_ascii_digit() => return self.number(line, column),
            c if is_ident_start(c) => return self.ident(line, column),
            c => {
                self.bump();
                return Err(syntax_error(
                    line,
                    column,
                    format!("unexpected character '{c}'"),
                ));
            }
        };
        Ok(Token { kind, line, column })
    }

    fn string(&mut self, line: u32, column: u32) -> io::Result<Token> {
        self.bump();
        let mut value = String::new();
        loop {
            match self.bump() {
                None => {
                    return Err(syntax_error(line, column, "unterminated string literal"));
                }
                Some('\'') => {
                    if self.peek_char() == Some('\'') {
                        self.bump();
                        value.push('\'');
                    } else {
                        break;
                    }
                }
                Some(c) => value.push(c),
            }
        }
        Ok(Token {
            kind: TokenKind::String(value),
            line,
            column,
        })
    }

    fn number(&mut self, line: u32, column: u32) -> io::Result<Token> {
        let start = self.index;
        while matches!(self.peek_char(), Some(c) if c.is_ascii_digit()) {
            self.bump();
        }
        let digits = &self.source[start..self.index];
        let value = digits
            .parse::<i64>()
            .map_err(|_| syntax_error(line, column, "integer literal out of range"))?;
        Ok(Token {
            kind: TokenKind::Integer(value),
            line,
            column,
        })
    }

    fn ident(&mut self, line: u32, column: u32) -> io::Result<Token> {
        let start = self.index;
        self.bump();
        while matches!(self.peek_char(), Some(c) if is_ident_continue(c)) {
            self.bump();
        }
        if self.index - start > 64 {
            return Err(syntax_error(line, column, "identifier too long"));
        }
        let text = &self.source[start..self.index];
        let kind = match Keyword::from_word(text) {
            Some(keyword) => TokenKind::Keyword(keyword),
            None => TokenKind::Ident(text.to_string()),
        };
        Ok(Token { kind, line, column })
    }
}

fn is_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::{tokenize, Keyword, Token, TokenKind};
    use std::io::ErrorKind;

    fn assert_token(token: &Token, kind: TokenKind, line: u32, column: u32) {
        assert_eq!(token.line, line, "{token:?}");
        assert_eq!(token.column, column, "{token:?}");
        assert_eq!(token.kind, kind);
    }

    fn assert_lex_err(sql: &str, line: u32, column: u32, message: &str) {
        let err = tokenize(sql).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            err.to_string(),
            format!("syntax error at {line}:{column}: {message}")
        );
    }

    #[test]
    fn positions_symbols_keywords_and_eof() {
        let tokens = tokenize("(),;*.=<>!=<=>=+-/%").unwrap();
        let expected = [
            (TokenKind::LParen, 1),
            (TokenKind::RParen, 2),
            (TokenKind::Comma, 3),
            (TokenKind::Semicolon, 4),
            (TokenKind::Star, 5),
            (TokenKind::Dot, 6),
            (TokenKind::Eq, 7),
            (TokenKind::NotEq, 8),
            (TokenKind::BangEq, 10),
            (TokenKind::LtEq, 12),
            (TokenKind::GtEq, 14),
            (TokenKind::Plus, 16),
            (TokenKind::Minus, 17),
            (TokenKind::Slash, 18),
            (TokenKind::Percent, 19),
            (TokenKind::Eof, 20),
        ];
        assert_eq!(tokens.len(), expected.len());
        for (token, (kind, column)) in tokens.iter().zip(expected) {
            assert_token(token, kind, 1, column);
        }

        let tokens = tokenize("SeLeCt").unwrap();
        assert_token(&tokens[0], TokenKind::Keyword(Keyword::Select), 1, 1);
        assert_token(&tokens[1], TokenKind::Eof, 1, 7);

        let tokens = tokenize("SELECT").unwrap();
        assert_token(&tokens[1], TokenKind::Eof, 1, 7);
        let tokens = tokenize("SELECT\n").unwrap();
        assert_token(&tokens[1], TokenKind::Eof, 2, 1);

        let tokens = tokenize("\tSELECT\t*").unwrap();
        assert_token(&tokens[0], TokenKind::Keyword(Keyword::Select), 1, 2);
        assert_token(&tokens[1], TokenKind::Star, 1, 9);
    }

    #[test]
    fn comments_whitespace_and_multiline_positions() {
        let sql = "SELECT -- comment\n  id\nFROM t";
        let tokens = tokenize(sql).unwrap();
        assert_token(&tokens[0], TokenKind::Keyword(Keyword::Select), 1, 1);
        assert_token(&tokens[1], TokenKind::Ident("id".to_string()), 2, 3);
        assert_token(&tokens[2], TokenKind::Keyword(Keyword::From), 3, 1);
        assert_token(&tokens[3], TokenKind::Ident("t".to_string()), 3, 6);
        assert_token(&tokens[4], TokenKind::Eof, 3, 7);

        let tokens = tokenize("--ab").unwrap();
        assert_token(&tokens[0], TokenKind::Eof, 1, 5);

        let tokens = tokenize("-- 'unterminated\nSELECT").unwrap();
        assert_token(&tokens[0], TokenKind::Keyword(Keyword::Select), 2, 1);

        let tokens = tokenize(" \n\t -- x\n").unwrap();
        assert_token(&tokens[0], TokenKind::Eof, 3, 1);
        assert!(tokenize("").unwrap()[0].kind == TokenKind::Eof);
    }

    #[test]
    fn strings_keep_escapes_newlines_and_char_columns() {
        let tokens = tokenize("'it''s'").unwrap();
        assert_token(&tokens[0], TokenKind::String("it's".to_string()), 1, 1);
        assert_token(&tokens[1], TokenKind::Eof, 1, 8);

        let tokens = tokenize("'ab\nc' SELECT").unwrap();
        assert_token(&tokens[0], TokenKind::String("ab\nc".to_string()), 1, 1);
        assert_token(&tokens[1], TokenKind::Keyword(Keyword::Select), 2, 4);
        assert_token(&tokens[2], TokenKind::Eof, 2, 10);

        let tokens = tokenize("'-- not a comment'").unwrap();
        assert_token(
            &tokens[0],
            TokenKind::String("-- not a comment".to_string()),
            1,
            1,
        );

        let tokens = tokenize("'あ'x").unwrap();
        assert_token(&tokens[0], TokenKind::String("あ".to_string()), 1, 1);
        assert_token(&tokens[1], TokenKind::Ident("x".to_string()), 1, 4);

        let tokens = tokenize("''").unwrap();
        assert_token(&tokens[0], TokenKind::String(String::new()), 1, 1);
    }

    #[test]
    fn keywords_are_case_insensitive_and_maximal() {
        let words = "AND AS ASC BY CREATE CROSS DELETE DESC FALSE FROM INNER INSERT INTEGER INTO IS JOIN KEY LEFT LIMIT NOT NULL OFFSET ON OR ORDER OUTER PRIMARY SELECT SET TABLE TEXT TRUE UPDATE VALUES WHERE";
        for sql in [words, &words.to_ascii_lowercase()] {
            let tokens = tokenize(sql).unwrap();
            assert_eq!(tokens.len(), 36);
            for token in &tokens[..tokens.len() - 1] {
                assert!(matches!(token.kind, TokenKind::Keyword(_)), "{token:?}");
            }
        }
        let tokens = tokenize("SELECT_ fromage _id A1").unwrap();
        assert_token(&tokens[0], TokenKind::Ident("SELECT_".to_string()), 1, 1);
        assert_token(&tokens[1], TokenKind::Ident("fromage".to_string()), 1, 9);
        assert_token(&tokens[2], TokenKind::Ident("_id".to_string()), 1, 17);
        assert_token(&tokens[3], TokenKind::Ident("A1".to_string()), 1, 21);
    }

    #[test]
    fn integers_and_identifier_limits() {
        let tokens = tokenize("0 007 9223372036854775807").unwrap();
        assert_token(&tokens[0], TokenKind::Integer(0), 1, 1);
        assert_token(&tokens[1], TokenKind::Integer(7), 1, 3);
        assert_token(&tokens[2], TokenKind::Integer(i64::MAX), 1, 7);

        let name = "a".repeat(64);
        let tokens = tokenize(&name).unwrap();
        assert_token(&tokens[0], TokenKind::Ident(name), 1, 1);

        assert_lex_err("9223372036854775808", 1, 1, "integer literal out of range");
        assert_lex_err("-9223372036854775808", 1, 2, "integer literal out of range");
        assert_lex_err(&"a".repeat(65), 1, 1, "identifier too long");
    }

    #[test]
    fn lexer_errors_use_syntax_error_format() {
        assert_lex_err("@", 1, 1, "unexpected character '@'");
        assert_lex_err("あ", 1, 1, "unexpected character 'あ'");
        assert_lex_err("aあ", 1, 2, "unexpected character 'あ'");
        assert_lex_err("!", 1, 1, "unexpected character '!'");
        assert_lex_err("'", 1, 1, "unterminated string literal");
        assert_lex_err("SELECT '", 1, 8, "unterminated string literal");
        assert_lex_err("SELECT '\noops", 1, 8, "unterminated string literal");
    }
}
