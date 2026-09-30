//! The program's names in the editor: go to definition, hover and completion.
//!
//! A word names a declaration when it is spelt like one (names are case-insensitive). Words that
//! are plainly something else are skipped: an assigned column (`x = ...`), an option key
//! (`path: ...`), a function call (`f(...)`) and a column after a qualifier (`a.amount`), unless
//! the qualifier is a declaration with that output (`rec.matches`).

use std::fmt::Write as _;

use lsp_types::{CompletionItem, CompletionItemKind};

use super::analysis::Analysis;
use crate::ast::{Ident, Statement};
use crate::semantic::functions;
use crate::semantic::hir::{Hir, RelKind, Relation};
use crate::semantic::load::Loaded;
use crate::syntax::parser::STATEMENT_KEYWORDS;
use crate::syntax::span::Span;
use crate::syntax::token::{Tok, Token, lex};

/// A top-level declaration that other statements refer to by name.
#[derive(Clone, Copy)]
pub struct Decl<'a> {
    pub kind: &'static str,
    pub name: &'a Ident,
    /// `csv`, `sql`, `odbc`, ... for sources and connections.
    pub detail: Option<&'a str>,
}

pub fn declarations(loaded: &Loaded) -> Vec<Decl<'_>> {
    loaded
        .statements()
        .filter_map(|s| {
            let (kind, name, detail) = match s {
                Statement::Source(d) => ("source", &d.name, Some(d.kind.name.as_str())),
                Statement::Dataset(d) => ("dataset", &d.name, None),
                Statement::Reconcile(d) => ("reconcile", &d.name, None),
                Statement::Mapping(d) => ("mapping", &d.name, None),
                Statement::Connection(d) => ("connection", &d.name, Some(d.kind.name.as_str())),
                _ => return None,
            };
            Some(Decl { kind, name, detail })
        })
        .collect()
}

fn same_name(a: &str, b: &str) -> bool {
    a.chars()
        .flat_map(char::to_lowercase)
        .eq(b.chars().flat_map(char::to_lowercase))
}

fn relation<'h>(hir: &'h Hir, name: &str) -> Option<&'h Relation> {
    hir.relation(name)
        .or_else(|| hir.relations.iter().find(|r| same_name(&r.name, name)))
}

/// Relations named `<name>.<part>`: a reconcile's outputs, a source's rejects, a validation's
/// failures and checks.
fn outputs<'h>(hir: &'h Hir, name: &str) -> impl Iterator<Item = &'h Relation> {
    hir.relations.iter().filter(move |r| {
        r.name
            .split_once('.')
            .is_some_and(|(owner, _)| same_name(owner, name))
    })
}

/// What a word in the program names.
pub enum Target<'a> {
    /// A declaration (`sales`, or `rec` in `rec.matches`).
    Decl(Decl<'a>),
    /// A declaration's output (`matches` in `rec.matches`).
    Output(Decl<'a>, &'a Relation),
}

impl<'a> Target<'a> {
    pub fn decl(&self) -> Decl<'a> {
        match self {
            Target::Decl(d) | Target::Output(d, _) => *d,
        }
    }
}

fn word(t: &Token) -> Option<&str> {
    match &t.tok {
        Tok::Ident { name, .. } => Some(name),
        _ => None,
    }
}

/// The name at byte `offset` of `text` (a word containing it, or ending there) and its span.
pub fn target_at<'a>(a: &'a Analysis, text: &str, offset: usize) -> Option<(Target<'a>, Span)> {
    let tokens = lex(0, text).tokens;
    let at = |t: &Token| t.span.start as usize <= offset && offset <= t.span.end as usize;
    let i = tokens
        .iter()
        .position(|t| word(t).is_some() && at(t) && offset < t.span.end as usize)
        .or_else(|| tokens.iter().position(|t| word(t).is_some() && at(t)))?;
    let name = word(&tokens[i])?;
    let prev = |n: usize| i.checked_sub(n).map(|j| &tokens[j]);
    let decls = declarations(&a.loaded);
    let decl = |n: &str| decls.iter().copied().find(|d| same_name(&d.name.name, n));
    if prev(1).is_some_and(|t| t.tok == Tok::Dot) {
        let owner = decl(word(prev(2)?)?)?;
        let rel = relation(&a.hir, &format!("{}.{name}", owner.name.name))?;
        return Some((Target::Output(owner, rel), tokens[i].span));
    }
    let declares = prev(1)
        .and_then(word)
        .is_some_and(|w| STATEMENT_KEYWORDS.contains(&w));
    let next = tokens.get(i + 1).map(|t| &t.tok);
    if !declares && matches!(next, Some(Tok::Assign | Tok::Colon | Tok::LParen)) {
        return None;
    }
    Some((Target::Decl(decl(name)?), tokens[i].span))
}

fn columns(out: &mut String, rel: &Relation) {
    if rel.open {
        out.push_str("\n\ncolumns unknown until the source is contacted (`magi check --sources`)");
        return;
    }
    let width = rel.columns.iter().map(|c| c.name.len()).max().unwrap_or(0);
    out.push_str("\n\n```text\n");
    for c in &rel.columns {
        let _ = writeln!(out, "{:<width$}  {}", c.name, c.ty);
    }
    out.push_str("```");
}

/// Hover text (Markdown): the declaration's kind and where it is, then the relation's columns
/// with their types, or the outputs a reconciliation produces.
pub fn hover(a: &Analysis, target: &Target<'_>) -> String {
    let d = target.decl();
    let file = a.loaded.sources.file(d.name.span.file);
    let (line, _) = file.line_col(d.name.span.start);
    let shown = file
        .path
        .file_name()
        .map_or_else(|| file.name.clone(), |n| n.to_string_lossy().into_owned());
    let place = format!("declared in `{shown}` line {line}");
    let mut out = String::new();
    match target {
        Target::Output(_, rel) => {
            let what = match rel.kind {
                RelKind::SourceRejects => "rejected rows",
                RelKind::ValidationPart => "validation output",
                _ => "output",
            };
            let _ = write!(
                out,
                "{what} `{}` of {} `{}` ({place})",
                rel.name, d.kind, d.name.name
            );
            columns(&mut out, rel);
        }
        Target::Decl(d) => {
            let _ = write!(out, "{} `{}`", d.kind, d.name.name);
            if let Some(detail) = d.detail {
                let _ = write!(out, " ({detail})");
            }
            let _ = write!(out, ", {place}");
            if matches!(d.kind, "source" | "dataset") {
                match relation(&a.hir, &d.name.name) {
                    Some(rel) => columns(&mut out, rel),
                    None => {
                        out.push_str("\n\ncolumns unknown until the program's errors are fixed")
                    }
                }
            }
            let parts: Vec<String> = outputs(&a.hir, &d.name.name)
                .map(|r| format!("`{}`", r.name))
                .collect();
            if !parts.is_empty() {
                let _ = write!(out, "\n\noutputs: {}", parts.join(", "));
            }
        }
    }
    out
}

/// The byte offset where the word (letters, digits, `_`) ending at byte `end` of `text` starts.
fn word_start(text: &str, end: usize) -> usize {
    text[..end]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| c.is_alphanumeric() || c == '_')
        .last()
        .map_or(end, |(i, _)| i)
}

fn item(label: &str, kind: CompletionItemKind, detail: &str) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        kind: Some(kind),
        detail: Some(detail.to_string()),
        ..Default::default()
    }
}

/// Completion at byte `offset` of `text`: after `name.`, the outputs of declaration `name`; at
/// the start of an unindented line, the statement keywords; elsewhere the program's relations,
/// mappings and connections and the function library.
pub fn completions(a: Option<&Analysis>, text: &str, offset: usize) -> Vec<CompletionItem> {
    let offset = offset.min(text.len());
    let start = word_start(text, offset);
    let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
    if start == line_start {
        return STATEMENT_KEYWORDS
            .iter()
            .map(|k| item(k, CompletionItemKind::KEYWORD, "statement"))
            .collect();
    }
    let Some(a) = a else {
        return functions::ALL
            .iter()
            .map(|f| item(f, CompletionItemKind::FUNCTION, "function"))
            .collect();
    };
    if let Some(before) = text[..start].strip_suffix('.') {
        let owner = &before[word_start(before, before.len())..];
        return outputs(&a.hir, owner)
            .filter_map(|r| {
                let (_, part) = r.name.split_once('.')?;
                Some(item(part, CompletionItemKind::FIELD, "output"))
            })
            .collect();
    }
    let mut items: Vec<CompletionItem> = a
        .hir
        .relations
        .iter()
        .map(|r| {
            let detail = match r.kind {
                RelKind::Source => "source",
                RelKind::SourceRejects => "rejected rows",
                RelKind::Dataset => "dataset",
                RelKind::ReconcilePart => "reconciliation output",
                RelKind::ValidationPart => "validation output",
            };
            item(&r.name, CompletionItemKind::STRUCT, detail)
        })
        .collect();
    for d in declarations(&a.loaded) {
        if matches!(d.kind, "mapping" | "connection") {
            items.push(item(&d.name.name, CompletionItemKind::VARIABLE, d.kind));
        }
    }
    items.extend(
        functions::ALL
            .iter()
            .map(|f| item(f, CompletionItemKind::FUNCTION, "function")),
    );
    items
}
