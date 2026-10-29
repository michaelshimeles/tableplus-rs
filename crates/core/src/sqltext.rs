//! A small SQL lexer shared by syntax highlighting, statement splitting and autocomplete.

use crate::dialect::Dialect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Whitespace,
    Comment,
    String,
    QuotedIdent,
    Number,
    Keyword,
    Ident,
    Operator,
    Punct,
    Param,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub start: usize,
    pub end: usize,
}

impl Token {
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        &src[self.start..self.end]
    }

    pub fn is_trivia(&self) -> bool {
        matches!(self.kind, TokenKind::Whitespace | TokenKind::Comment)
    }
}

pub const KEYWORDS: &[&str] = &[
    "ADD",
    "ALL",
    "ALTER",
    "ANALYZE",
    "AND",
    "ANY",
    "ARRAY",
    "AS",
    "ASC",
    "AUTO_INCREMENT",
    "BEGIN",
    "BETWEEN",
    "BIGINT",
    "BOOLEAN",
    "BOTH",
    "BY",
    "CALL",
    "CASCADE",
    "CASE",
    "CAST",
    "CHAR",
    "CHECK",
    "COLLATE",
    "COLUMN",
    "COMMENT",
    "COMMIT",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "CURRENT_DATE",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "DATABASE",
    "DATE",
    "DECIMAL",
    "DECLARE",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DESCRIBE",
    "DISTINCT",
    "DO",
    "DROP",
    "ELSE",
    "END",
    "ENUM",
    "ESCAPE",
    "EXCEPT",
    "EXISTS",
    "EXPLAIN",
    "FALSE",
    "FETCH",
    "FILTER",
    "FIRST",
    "FLOAT",
    "FOLLOWING",
    "FOR",
    "FOREIGN",
    "FROM",
    "FULL",
    "FUNCTION",
    "GRANT",
    "GROUP",
    "HAVING",
    "IF",
    "ILIKE",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INT",
    "INTEGER",
    "INTERSECT",
    "INTERVAL",
    "INTO",
    "IS",
    "JOIN",
    "JSON",
    "JSONB",
    "KEY",
    "LAST",
    "LATERAL",
    "LEADING",
    "LEFT",
    "LIKE",
    "LIMIT",
    "MATERIALIZED",
    "NATURAL",
    "NOT",
    "NULL",
    "NULLS",
    "NUMERIC",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "OVER",
    "PARTITION",
    "PRECEDING",
    "PRIMARY",
    "PROCEDURE",
    "RANGE",
    "RECURSIVE",
    "REFERENCES",
    "RENAME",
    "REPLACE",
    "RETURNING",
    "RETURNS",
    "REVOKE",
    "RIGHT",
    "ROLLBACK",
    "ROW",
    "ROWS",
    "SCHEMA",
    "SELECT",
    "SEQUENCE",
    "SET",
    "SHOW",
    "SMALLINT",
    "TABLE",
    "TEMP",
    "TEMPORARY",
    "TEXT",
    "THEN",
    "TIME",
    "TIMESTAMP",
    "TO",
    "TRAILING",
    "TRANSACTION",
    "TRIGGER",
    "TRUE",
    "TRUNCATE",
    "TYPE",
    "UNBOUNDED",
    "UNION",
    "UNIQUE",
    "UNSIGNED",
    "UPDATE",
    "USE",
    "USING",
    "VACUUM",
    "VALUES",
    "VARCHAR",
    "VIEW",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
    "WITHOUT",
];

pub const FUNCTIONS: &[&str] = &[
    "abs",
    "avg",
    "ceil",
    "char_length",
    "coalesce",
    "concat",
    "count",
    "date_trunc",
    "dense_rank",
    "extract",
    "first_value",
    "floor",
    "greatest",
    "group_concat",
    "ifnull",
    "json_agg",
    "json_build_object",
    "lag",
    "lead",
    "least",
    "length",
    "lower",
    "max",
    "min",
    "now",
    "nullif",
    "rank",
    "replace",
    "round",
    "row_number",
    "string_agg",
    "substr",
    "substring",
    "sum",
    "trim",
    "upper",
];

pub fn is_keyword(word: &str) -> bool {
    let upper = word.to_ascii_uppercase();
    KEYWORDS.binary_search(&upper.as_str()).is_ok()
}

/// PostgreSQL, MySQL and SQLite read every non-ASCII character, a no-break space included, as part
/// of an identifier, and only ASCII as whitespace.
fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || !c.is_ascii()
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$' || !c.is_ascii()
}

fn is_space(c: char) -> bool {
    c.is_ascii_whitespace() || c == '\x0b'
}

/// Tokenizes `src`. Never fails: unterminated strings/comments run to the end of input.
pub fn tokenize(src: &str, dialect: Dialect) -> Vec<Token> {
    let bytes = src.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    let len = src.len();
    let next_char = |i: usize| src[i..].chars().next();
    while i < len {
        let start = i;
        let c = next_char(i).unwrap();
        let kind = if is_space(c) {
            while let Some(c) = next_char(i).filter(|c| is_space(*c)) {
                i += c.len_utf8();
            }
            TokenKind::Whitespace
        } else if line_comment(src, i, dialect) {
            i = src[i..].find('\n').map_or(len, |p| i + p);
            TokenKind::Comment
        } else if src[i..].starts_with("/*") {
            i = block_comment_end(src, i, dialect);
            TokenKind::Comment
        } else if c == '\'' || (dialect == Dialect::MySql && c == '"') {
            i = scan_quoted(src, i, c, dialect == Dialect::MySql);
            TokenKind::String
        } else if (c == 'E' || c == 'e') && dialect == Dialect::Postgres && bytes.get(i + 1) == Some(&b'\'') {
            i = scan_quoted(src, i + 1, '\'', true);
            TokenKind::String
        } else if (c == 'X' || c == 'x') && bytes.get(i + 1) == Some(&b'\'') {
            i = scan_quoted(src, i + 1, '\'', false);
            TokenKind::String
        } else if c == '"' || (c == '`' && dialect != Dialect::Postgres) {
            i = scan_quoted(src, i, c, false);
            TokenKind::QuotedIdent
        } else if c == '$' && dialect == Dialect::Postgres {
            if let Some(end) = scan_dollar_quoted(src, i) {
                i = end;
                TokenKind::String
            } else {
                i += 1;
                while i < len && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                TokenKind::Param
            }
        } else if c.is_ascii_digit() || (c == '.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            i = scan_number(src, i);
            TokenKind::Number
        } else if is_ident_start(c) {
            while let Some(c) = next_char(i).filter(|c| is_ident_char(*c)) {
                i += c.len_utf8();
            }
            if is_keyword(&src[start..i]) { TokenKind::Keyword } else { TokenKind::Ident }
        } else if c == '?' {
            i += 1;
            TokenKind::Param
        } else if c == ':' && bytes.get(i + 1).is_some_and(|b| b.is_ascii_alphabetic()) && !src[..i].ends_with(':') {
            i += 1;
            while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            TokenKind::Param
        } else if matches!(c, '(' | ')' | ',' | ';' | '.' | '[' | ']' | '{' | '}') {
            i += 1;
            TokenKind::Punct
        } else if "+-*/<>=!|&%^~:@#".contains(c) {
            i += 1;
            while i < len && "<>=|&:".contains(bytes[i] as char) && "<>=!|:".contains(c) {
                i += 1;
            }
            TokenKind::Operator
        } else {
            i += c.len_utf8();
            TokenKind::Unknown
        };
        tokens.push(Token { kind, start, end: i });
    }
    tokens
}

/// MySQL starts a `--` comment only before whitespace or a control character; `1 --1` is
/// arithmetic there.
fn line_comment(src: &str, i: usize, dialect: Dialect) -> bool {
    match dialect {
        Dialect::MySql => {
            src[i..].starts_with('#')
                || (src[i..].starts_with("--")
                    && src[i + 2..].chars().next().is_none_or(|c| c.is_ascii_whitespace() || c.is_ascii_control()))
        }
        _ => src[i..].starts_with("--"),
    }
}

/// The end of the block comment that opens at `open`. PostgreSQL nests them.
fn block_comment_end(src: &str, open: usize, dialect: Dialect) -> usize {
    let bytes = src.as_bytes();
    let mut depth = 0usize;
    let mut i = open;
    while i + 1 < bytes.len() {
        match (bytes[i], bytes[i + 1]) {
            (b'/', b'*') if depth == 0 || dialect == Dialect::Postgres => {
                depth += 1;
                i += 2;
            }
            (b'*', b'/') => {
                depth -= 1;
                i += 2;
                if depth == 0 {
                    return i;
                }
            }
            _ => i += 1,
        }
    }
    src.len()
}

fn scan_quoted(src: &str, open: usize, quote: char, backslash_escapes: bool) -> usize {
    let bytes = src.as_bytes();
    let q = quote as u8;
    let mut i = open + 1;
    while i < bytes.len() {
        let b = bytes[i];
        if backslash_escapes && b == b'\\' {
            i += 2;
            continue;
        }
        if b == q {
            if bytes.get(i + 1) == Some(&q) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

fn scan_dollar_quoted(src: &str, start: usize) -> Option<usize> {
    let rest = &src[start + 1..];
    let tag_len = rest.find('$')?;
    let tag = &rest[..tag_len];
    if !tag.chars().all(|c| c.is_alphanumeric() || c == '_') || tag.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let delim = &src[start..start + tag_len + 2];
    let body_start = start + tag_len + 2;
    Some(src[body_start..].find(delim).map_or(src.len(), |p| body_start + p + delim.len()))
}

fn scan_number(src: &str, start: usize) -> usize {
    let b = src.as_bytes();
    let mut i = start;
    if b[i] == b'0' && matches!(b.get(i + 1), Some(b'x' | b'X')) {
        i += 2;
        while i < b.len() && b[i].is_ascii_hexdigit() {
            i += 1;
        }
        return i;
    }
    while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
        i += 1;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            i = j;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
        }
    }
    i
}

/// A statement inside a larger script; `start..end` excludes the delimiter and surrounding
/// whitespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementSpan {
    pub start: usize,
    pub end: usize,
}

impl StatementSpan {
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        &src[self.start..self.end]
    }
}

/// Splits a script into statements on `;` (or a custom `DELIMITER` for MySQL dumps).
/// Statements containing only comments are dropped.
pub fn split_statements(src: &str, dialect: Dialect) -> Vec<StatementSpan> {
    let tokens = tokenize(src, dialect);
    let mut out = Vec::new();
    let mut delimiter = ";".to_string();
    let mut stmt_start: Option<usize> = None;
    let mut stmt_end = 0;
    let mut i = 0;
    let push = |out: &mut Vec<StatementSpan>, start: Option<usize>, end: usize| {
        if let Some(s) = start {
            out.push(StatementSpan { start: s, end });
        }
    };
    while i < tokens.len() {
        let t = tokens[i];
        if t.is_trivia() || matches!(t.kind, TokenKind::String | TokenKind::QuotedIdent) {
            if !t.is_trivia() {
                stmt_start.get_or_insert(t.start);
                stmt_end = t.end;
            }
            i += 1;
            continue;
        }
        if stmt_start.is_none() && t.kind == TokenKind::Ident && t.text(src).eq_ignore_ascii_case("delimiter") {
            let line_end = src[t.end..].find('\n').map_or(src.len(), |p| t.end + p);
            let new = src[t.end..line_end].trim();
            if !new.is_empty() {
                delimiter = new.to_string();
            }
            while i < tokens.len() && tokens[i].start < line_end {
                i += 1;
            }
            continue;
        }
        if src[t.start..].starts_with(delimiter.as_str()) {
            push(&mut out, stmt_start.take(), stmt_end);
            let delim_end = t.start + delimiter.len();
            while i < tokens.len() && tokens[i].start < delim_end {
                i += 1;
            }
            continue;
        }
        stmt_start.get_or_insert(t.start);
        stmt_end = t.end;
        i += 1;
    }
    push(&mut out, stmt_start, stmt_end);
    out
}

/// The statement under the cursor (byte offset): the statement containing it, or else the
/// closest statement before it, or the first one.
pub fn statement_at(src: &str, cursor: usize, dialect: Dialect) -> Option<StatementSpan> {
    let stmts = split_statements(src, dialect);
    let mut best = None;
    for s in &stmts {
        if s.start <= cursor {
            best = Some(s.clone());
        }
        if cursor <= s.end {
            break;
        }
    }
    best.or_else(|| stmts.into_iter().next())
}

/// Removes a trailing `;` and ASCII whitespace; a trailing no-break space belongs to the last name.
pub fn trim_statement(sql: &str) -> &str {
    sql.trim_start().trim_end_matches(is_space).trim_end_matches(';').trim_end_matches(is_space)
}

/// True for statements that produce rows (used to decide on row limits).
pub fn is_query(sql: &str) -> bool {
    let first = sql.trim_start().split(|c: char| !c.is_alphanumeric()).next().unwrap_or("").to_ascii_uppercase();
    matches!(
        first.as_str(),
        "SELECT" | "WITH" | "SHOW" | "VALUES" | "TABLE" | "EXPLAIN" | "DESCRIBE" | "DESC" | "PRAGMA"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(src: &str, d: Dialect) -> Vec<&str> {
        split_statements(src, d).iter().map(|s| s.text(src)).collect()
    }

    #[test]
    fn keywords_sorted() {
        let mut sorted = KEYWORDS.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, KEYWORDS, "KEYWORDS must stay sorted for binary search");
    }

    #[test]
    fn tokens() {
        let src = "SELECT a.\"b\", 'it''s', 1.5e3 FROM t -- hi\nWHERE x = $1";
        let kinds: Vec<_> =
            tokenize(src, Dialect::Postgres).into_iter().filter(|t| !t.is_trivia()).map(|t| t.kind).collect();
        use TokenKind::*;
        assert_eq!(
            kinds,
            vec![
                Keyword,
                Ident,
                Punct,
                QuotedIdent,
                Punct,
                String,
                Punct,
                Number,
                Keyword,
                Ident,
                Keyword,
                Ident,
                Operator,
                Param
            ]
        );
        let all = tokenize(src, Dialect::Postgres);
        assert_eq!(all.last().unwrap().end, src.len());
    }

    #[test]
    fn split_basic() {
        assert_eq!(
            texts("select 1; select 2 ;\n\n select ';' ", Dialect::Postgres),
            vec!["select 1", "select 2", "select ';'"]
        );
        assert_eq!(texts("-- only a comment\n;select 1", Dialect::Postgres), vec!["select 1"]);
        assert_eq!(
            texts("create function f() returns int as $$ select 1; $$ language sql; select 2", Dialect::Postgres),
            vec!["create function f() returns int as $$ select 1; $$ language sql", "select 2"]
        );
        assert_eq!(texts("select 'a\\'b;'; select 2", Dialect::MySql), vec!["select 'a\\'b;'", "select 2"]);
        assert_eq!(texts("select \"x;y\" from t", Dialect::Sqlite), vec!["select \"x;y\" from t"]);
        assert_eq!(texts("/* a; b */ select 1", Dialect::Sqlite), vec!["select 1"]);
    }

    #[test]
    fn split_follows_each_servers_comments() {
        assert_eq!(texts("select 1 --1 ; delete from t", Dialect::MySql), vec!["select 1 --1", "delete from t"]);
        assert_eq!(texts("select 1 -- c ; delete from t", Dialect::MySql), vec!["select 1"]);
        assert_eq!(texts("select 1 --1 ; delete from t", Dialect::Postgres), vec!["select 1"]);
        let nested = "/* /* */ select 1 */ delete from t; select 2";
        assert_eq!(texts(nested, Dialect::Postgres), vec!["delete from t", "select 2"]);
        assert_eq!(texts("/* /* */ select 1", Dialect::MySql), vec!["select 1"]);
        let nbsp = "select 1 --\u{a0} from t; delete from t";
        assert_eq!(texts(nbsp, Dialect::MySql), vec!["select 1 --\u{a0} from t", "delete from t"]);
        let dollar = "select 1 as a€$x$; delete from t; select '$x$'";
        assert_eq!(texts(dollar, Dialect::Postgres), vec!["select 1 as a€$x$", "delete from t", "select '$x$'"]);
    }

    #[test]
    fn split_mysql_delimiter() {
        let src = "DELIMITER //\nCREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END //\nDELIMITER ;\nSELECT 3;";
        assert_eq!(texts(src, Dialect::MySql), vec!["CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END", "SELECT 3"]);
    }

    #[test]
    fn cursor_statement() {
        let src = "select 1;\nselect 2;\n\nselect 3";
        let at = |c| statement_at(src, c, Dialect::Postgres).unwrap().text(src).to_string();
        assert_eq!(at(0), "select 1");
        assert_eq!(at(5), "select 1");
        assert_eq!(at(9), "select 1");
        assert_eq!(at(12), "select 2");
        assert_eq!(at(src.len()), "select 3");
        assert_eq!(statement_at("", 0, Dialect::Postgres), None);
    }

    #[test]
    fn query_detection() {
        assert!(is_query("  select 1"));
        assert!(is_query("WITH x AS (select 1) select * from x"));
        assert!(!is_query("update t set a = 1"));
        assert_eq!(trim_statement(" select 1 ; "), "select 1");
        assert_eq!(
            trim_statement("\u{a0}delete from t\u{a0} ;\n"),
            "delete from t\u{a0}",
            "a trailing no-break space is part of the name"
        );
    }
}
