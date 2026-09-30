//! Hand-written lexer. MAGI's grammar is small, so a separate lexer generator is not justified;
//! what matters is precise spans for diagnostics.
//!
//! Words are never reserved at the lexical level: `type`, `group`, `sort` are ordinary column
//! names. The parser recognises keywords by position.

use super::span::{FileId, Span};
use crate::diagnostic::Diagnostic;

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    /// Bare word or backtick-quoted identifier (`quoted` = true).
    Ident {
        name: String,
        quoted: bool,
    },
    Int(i64),
    /// Decimal literal kept as written, e.g. `0.02`.
    Decimal(String),
    Str(String),
    /// `"""..."""` block string (used for embedded SQL).
    BlockStr(String),
    Pipe,     // |>
    FatArrow, // =>
    Arrow,    // ->
    EqEq,
    NotEq,
    Le,
    Ge,
    Lt,
    Gt,
    Assign,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Dot,
    Colon,
    Semi,
    Question,
    /// Input the lexer could not read. It is already reported in [`Lexed::diagnostics`]; the
    /// parser drops the statement containing it without a second diagnostic.
    Error,
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
    /// A newline separates this token from the previous one.
    pub newline_before: bool,
}

/// A `#` comment, kept for `magi fmt`.
#[derive(Debug, Clone, PartialEq)]
pub struct Comment {
    pub text: String,
    pub span: Span,
    /// The comment follows code on the same line.
    pub trailing: bool,
}

pub struct Lexed {
    pub tokens: Vec<Token>,
    pub comments: Vec<Comment>,
    /// One M001 per piece of unreadable input; each also leaves a [`Tok::Error`] token so the
    /// rest of the file is still lexed and parsed.
    pub diagnostics: Vec<Diagnostic>,
}

pub fn lex(file: FileId, src: &str) -> Lexed {
    let bytes = src.as_bytes();
    let mut i = 0usize;
    let mut tokens: Vec<Token> = Vec::new();
    let mut comments = Vec::new();
    let mut diagnostics = Vec::new();
    let mut newline = true;
    let mut line_has_code = false;
    let span = |s: usize, e: usize| Span::new(file, s, e);

    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'\n' => {
                newline = true;
                line_has_code = false;
                i += 1;
                continue;
            }
            b' ' | b'\t' | b'\r' => {
                i += 1;
                continue;
            }
            b'#' => {
                let start = i;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                comments.push(Comment {
                    text: src[start..i].trim_end().to_string(),
                    span: span(start, i),
                    trailing: line_has_code,
                });
                continue;
            }
            _ => {}
        }
        let start = i;
        // `get`: the next character may be non-ASCII (an unexpected `€`), so `i + 2` may not be a
        // character boundary
        let two = src.get(i..i + 2).unwrap_or("");
        let tok = match two {
            "|>" => Some(Tok::Pipe),
            "=>" => Some(Tok::FatArrow),
            "->" => Some(Tok::Arrow),
            "==" => Some(Tok::EqEq),
            "!=" => Some(Tok::NotEq),
            "<=" => Some(Tok::Le),
            ">=" => Some(Tok::Ge),
            _ => None,
        };
        let tok = if let Some(t) = tok {
            i += 2;
            t
        } else if c.is_ascii_alphabetic() || c == b'_' {
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            Tok::Ident {
                name: src[start..i].to_string(),
                quoted: false,
            }
        } else if c.is_ascii_digit() {
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let mut is_decimal = false;
            if i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1].is_ascii_digit() {
                is_decimal = true;
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            let text = &src[start..i];
            if is_decimal {
                Tok::Decimal(text.to_string())
            } else {
                match text.parse::<i64>() {
                    Ok(v) => Tok::Int(v),
                    Err(_) => {
                        diagnostics.push(
                            Diagnostic::error("M001", "integer literal is too large")
                                .label(span(start, i), "does not fit in a 64-bit integer")
                                .help("write it as a decimal, e.g. `12345678901234567890.0`"),
                        );
                        Tok::Error
                    }
                }
            }
        } else if src[i..].starts_with("\"\"\"") {
            i += 3;
            let body_start = i;
            match src[i..].find("\"\"\"") {
                Some(off) => {
                    i += off + 3;
                    Tok::BlockStr(src[body_start..i - 3].to_string())
                }
                None => {
                    diagnostics.push(
                        Diagnostic::error("M001", "unterminated block string")
                            .label(span(start, start + 3), "block string starts here")
                            .help("close it with `\"\"\"`"),
                    );
                    // Everything after the opening quotes would be string content.
                    i = bytes.len();
                    Tok::Error
                }
            }
        } else if c == b'"' {
            i += 1;
            let mut out = String::new();
            let mut terminated = false;
            while i < bytes.len() && bytes[i] != b'\n' {
                match bytes[i] {
                    b'"' => {
                        i += 1;
                        terminated = true;
                        break;
                    }
                    b'\\' => {
                        let esc = bytes.get(i + 1).copied();
                        let ch = match esc {
                            Some(b'n') => '\n',
                            Some(b't') => '\t',
                            Some(b'\\') => '\\',
                            Some(b'"') => '"',
                            // anything else stays as written, so regex patterns like `\d+` read naturally
                            _ => {
                                out.push('\\');
                                i += 1;
                                continue;
                            }
                        };
                        out.push(ch);
                        i += 2;
                    }
                    _ => {
                        let ch = src[i..].chars().next().unwrap();
                        out.push(ch);
                        i += ch.len_utf8();
                    }
                }
            }
            if terminated {
                Tok::Str(out)
            } else {
                diagnostics.push(
                    Diagnostic::error("M001", "unterminated string literal")
                        .label(span(start, i), "string starts here"),
                );
                Tok::Error
            }
        } else if c == b'`' {
            i += 1;
            let body_start = i;
            while i < bytes.len() && bytes[i] != b'`' && bytes[i] != b'\n' {
                i += 1;
            }
            if i >= bytes.len() || bytes[i] != b'`' {
                diagnostics.push(
                    Diagnostic::error("M001", "unterminated quoted identifier")
                        .label(span(start, i), "starts here"),
                );
                Tok::Error
            } else {
                let name = src[body_start..i].to_string();
                i += 1;
                if name.is_empty() {
                    diagnostics.push(
                        Diagnostic::error("M001", "empty quoted identifier")
                            .label(span(start, i), "here"),
                    );
                    Tok::Error
                } else {
                    Tok::Ident { name, quoted: true }
                }
            }
        } else {
            i += 1;
            match c {
                b'=' => Tok::Assign,
                b'<' => Tok::Lt,
                b'>' => Tok::Gt,
                b'+' => Tok::Plus,
                b'-' => Tok::Minus,
                b'*' => Tok::Star,
                b'/' => Tok::Slash,
                b'%' => Tok::Percent,
                b'(' => Tok::LParen,
                b')' => Tok::RParen,
                b'{' => Tok::LBrace,
                b'}' => Tok::RBrace,
                b'[' => Tok::LBracket,
                b']' => Tok::RBracket,
                b',' => Tok::Comma,
                b'.' => Tok::Dot,
                b':' => Tok::Colon,
                b';' => Tok::Semi,
                b'?' => Tok::Question,
                _ => {
                    let ch = src[start..].chars().next().unwrap();
                    i = start + ch.len_utf8();
                    // A run of bad characters (`&&`) is one error, reported at its first one.
                    if let Some(prev) = tokens.last_mut()
                        && prev.tok == Tok::Error
                        && prev.span.end as usize == start
                    {
                        prev.span.end = i as u32;
                        continue;
                    }
                    diagnostics.push(
                        Diagnostic::error("M001", format!("unexpected character `{ch}`"))
                            .label(span(start, i), "not valid here"),
                    );
                    Tok::Error
                }
            }
        };
        tokens.push(Token {
            tok,
            span: span(start, i),
            newline_before: newline,
        });
        newline = false;
        line_has_code = true;
    }
    tokens.push(Token {
        tok: Tok::Eof,
        span: span(src.len(), src.len()),
        newline_before: true,
    });
    Lexed {
        tokens,
        comments,
        diagnostics,
    }
}

impl Tok {
    /// Human description for "expected X, found Y" messages.
    pub fn describe(&self) -> String {
        match self {
            Tok::Ident { name, .. } => format!("`{name}`"),
            Tok::Int(v) => format!("number `{v}`"),
            Tok::Decimal(v) => format!("number `{v}`"),
            Tok::Str(_) => "a string".into(),
            Tok::BlockStr(_) => "a block string".into(),
            Tok::Eof => "end of file".into(),
            Tok::Error => "invalid input".into(),
            other => format!("`{}`", other.symbol()),
        }
    }
    pub fn symbol(&self) -> &'static str {
        match self {
            Tok::Pipe => "|>",
            Tok::FatArrow => "=>",
            Tok::Arrow => "->",
            Tok::EqEq => "==",
            Tok::NotEq => "!=",
            Tok::Le => "<=",
            Tok::Ge => ">=",
            Tok::Lt => "<",
            Tok::Gt => ">",
            Tok::Assign => "=",
            Tok::Plus => "+",
            Tok::Minus => "-",
            Tok::Star => "*",
            Tok::Slash => "/",
            Tok::Percent => "%",
            Tok::LParen => "(",
            Tok::RParen => ")",
            Tok::LBrace => "{",
            Tok::RBrace => "}",
            Tok::LBracket => "[",
            Tok::RBracket => "]",
            Tok::Comma => ",",
            Tok::Dot => ".",
            Tok::Colon => ":",
            Tok::Semi => ";",
            Tok::Question => "?",
            _ => "",
        }
    }
}
