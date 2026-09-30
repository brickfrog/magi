//! Semantic tokens, so an editor without a MAGI grammar still highlights programs: comments,
//! strings, numbers, calls to library functions, and keywords.
//!
//! Words are keywords only by position, as in the parser, so a column may still be called
//! `group` or `sort`. A word is a keyword when it is one of the known words below and stands
//! where the parser reads one: a statement word at the start of a line outside braces, a clause
//! word at the start of a line inside braces, a pipeline step after `|>`, a word such as `by`,
//! `on` or `as` later on a line that starts with a keyword, or an operator word (`and`, `is`,
//! `null`, ...). A word followed by `=`, `:`, `(` or `.`, or after `.`, is a name, never a
//! keyword.

use lsp_types::{SemanticToken, SemanticTokenType};

use super::convert::LineIndex;
use crate::semantic::functions;
use crate::syntax::parser::STATEMENT_KEYWORDS;
use crate::syntax::token::{Tok, lex};

/// Token types, in the order of the legend the server advertises.
pub const LEGEND: &[SemanticTokenType] = &[
    SemanticTokenType::KEYWORD,
    SemanticTokenType::COMMENT,
    SemanticTokenType::STRING,
    SemanticTokenType::NUMBER,
    SemanticTokenType::FUNCTION,
];
const KEYWORD: u32 = 0;
const COMMENT: u32 = 1;
const STRING: u32 = 2;
const NUMBER: u32 = 3;
const FUNCTION: u32 = 4;

/// Words that start an item inside a statement's braces.
const CLAUSES: &[&str] = &[
    "tier",
    "require",
    "expect",
    "warn",
    "rank",
    "block",
    "many",
    "one",
    "group",
    "subset",
    "compare",
    "evidence",
    "flag",
    "cardinality",
    "consume",
    "ambiguity",
    "duplicates",
    "identity",
    "schema",
    "table",
    "relationship",
    "dimension",
    "metric",
    "sheet",
];

/// Pipeline steps (after `|>`).
const STEPS: &[&str] = &[
    "select",
    "drop",
    "rename",
    "derive",
    "filter",
    "join",
    "left",
    "right",
    "full",
    "inner",
    "group",
    "aggregate",
    "sort",
    "distinct",
    "union",
    "limit",
    "normalize",
];

/// Words that continue a statement, clause or step on its line.
const CONTINUATIONS: &[&str] = &[
    "by",
    "join",
    "as",
    "on",
    "with",
    "to",
    "when",
    "max_items",
    "max_subsets",
    "asc",
    "desc",
    "none",
    "unique",
];

/// Operator words, keywords wherever an expression can be.
const OPERATORS: &[&str] = &["and", "or", "not", "is", "null", "in", "true", "false"];

/// The document's semantic tokens, encoded relative to each other as the protocol requires. A
/// token spanning lines (a block string, a string with a line break) is split per line.
pub fn tokens(text: &str) -> Vec<SemanticToken> {
    let lexed = lex(0, text);
    let toks = &lexed.tokens;
    let mut spans: Vec<(usize, usize, u32)> = lexed
        .comments
        .iter()
        .map(|c| (c.span.start as usize, c.span.end as usize, COMMENT))
        .collect();
    let mut depth = 0usize;
    // a keyword was seen on the current line
    let mut line_keyword = false;
    for (i, t) in toks.iter().enumerate() {
        let line_start = i == 0 || t.newline_before;
        if line_start {
            line_keyword = false;
        }
        let kind = match &t.tok {
            Tok::LBrace => {
                depth += 1;
                None
            }
            Tok::RBrace => {
                depth = depth.saturating_sub(1);
                None
            }
            Tok::Str(_) | Tok::BlockStr(_) => Some(STRING),
            Tok::Int(_) | Tok::Decimal(_) => Some(NUMBER),
            Tok::Ident {
                name,
                quoted: false,
            } => {
                let prev = i.checked_sub(1).map(|j| &toks[j].tok);
                let next = toks.get(i + 1).map(|t| &t.tok);
                let w = name.as_str();
                if prev == Some(&Tok::Dot) {
                    None
                } else if next == Some(&Tok::LParen) {
                    functions::ALL.contains(&w).then_some(FUNCTION)
                } else if matches!(next, Some(Tok::Assign | Tok::Colon | Tok::Dot)) {
                    None
                } else {
                    let keyword = (line_start && depth == 0 && STATEMENT_KEYWORDS.contains(&w))
                        || (line_start && depth > 0 && CLAUSES.contains(&w))
                        || (prev == Some(&Tok::Pipe) && STEPS.contains(&w))
                        || (line_keyword && CONTINUATIONS.contains(&w))
                        || OPERATORS.contains(&w)
                        || (w == "case" && next == Some(&Tok::LBrace))
                        || (w == "otherwise" && next == Some(&Tok::FatArrow))
                        || (w == "original" && prev == Some(&Tok::FatArrow));
                    line_keyword |= keyword;
                    keyword.then_some(KEYWORD)
                }
            }
            _ => None,
        };
        if let Some(kind) = kind {
            spans.push((t.span.start as usize, t.span.end as usize, kind));
        }
    }
    spans.sort_unstable();
    encode(text, &spans)
}

fn encode(text: &str, spans: &[(usize, usize, u32)]) -> Vec<SemanticToken> {
    let index = LineIndex::new(text);
    let mut out = Vec::with_capacity(spans.len());
    let (mut last_line, mut last_start) = (0u32, 0u32);
    for &(start, end, kind) in spans {
        let mut from = start;
        for piece in text[start..end].split_inclusive('\n') {
            let to = from + piece.trim_end_matches(['\n', '\r']).len();
            let (a, b) = (index.position(from), index.position(to));
            from += piece.len();
            if b.character == a.character {
                continue;
            }
            let delta_line = a.line - last_line;
            let delta_start = if delta_line == 0 {
                a.character - last_start
            } else {
                a.character
            };
            out.push(SemanticToken {
                delta_line,
                delta_start,
                length: b.character - a.character,
                token_type: kind,
                token_modifiers_bitset: 0,
            });
            (last_line, last_start) = (a.line, a.character);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (line, column, length, type) of every token, decoded.
    fn decoded(text: &str) -> Vec<(u32, u32, u32, u32)> {
        let (mut line, mut col) = (0, 0);
        tokens(text)
            .into_iter()
            .map(|t| {
                if t.delta_line > 0 {
                    col = 0;
                }
                line += t.delta_line;
                col += t.delta_start;
                (line, col, t.length, t.token_type)
            })
            .collect()
    }

    #[test]
    fn keywords_by_position_not_by_spelling() {
        let text = "dataset sort = a\n    |> sort by sort\n    |> derive { group = 1 }\n";
        assert_eq!(
            decoded(text),
            vec![
                (0, 0, 7, KEYWORD),  // dataset
                (1, 7, 4, KEYWORD),  // sort (step)
                (1, 12, 2, KEYWORD), // by
                (2, 7, 6, KEYWORD),  // derive
                (2, 24, 1, NUMBER),
            ]
        );
    }

    #[test]
    fn multi_line_strings_are_split_per_line() {
        let text = "# é\nsource s = sql(c, \"\"\"select\n  1\"\"\")\n";
        assert_eq!(
            decoded(text),
            vec![
                (0, 0, 3, COMMENT),
                (1, 0, 6, KEYWORD),
                (1, 18, 9, STRING),
                (2, 0, 6, STRING),
            ]
        );
    }
}
