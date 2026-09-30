//! `test` declarations (`magi test`): the program run on fixture inputs, with chosen relations
//! compared to expected files.
//!
//! A test's program is the loaded program with the test's `given` sources substituted and every
//! `export`, `model` and `test` statement removed: it writes no file, and without exports every
//! relation is computed (`plan::optimize::needed`). It is analysed like any program
//! ([`resolve::analyze`]) with a reader that never contacts a database, and must not keep a
//! `sql(...)` source (M707), so a test reads only files and runs the same way everywhere.
//!
//! Paths resolve against the file the path literal is in: a `given x = "path"` keeps the
//! declaration of `x` (options such as a fixed-width `layout:` still resolve against the
//! program's file) with only the path literal replaced by the test's, which lies in the test's
//! file.

mod compare;

use std::collections::HashMap;
use std::path::Path;

use crate::ast::{self, Statement, TestDecl, TestItem};
use crate::diagnostic::{Diagnostic, Diagnostics, did_you_mean};
use crate::semantic::hir::{Hir, SourceKind};
use crate::semantic::load::Loaded;
use crate::semantic::resolve::{self, Options};
use crate::semantic::schema::SchemaProvider;
use crate::source::Reader;
use crate::syntax::span::Span;

pub use compare::run;

/// One test, analysed and ready to run.
pub struct Prepared {
    /// The test's program (same source map as the loaded program).
    pub loaded: Loaded,
    pub hir: Hir,
    /// Analysis of the test's program plus the test's own checks (M70x).
    pub diags: Diagnostics,
    /// The reader that analysed the program; the run stages sources with it.
    pub reader: Reader,
    pub expects: Vec<Expect>,
}

/// `expect REL == "file"` or `expect REL is empty`.
pub struct Expect {
    /// The relation's name in the program.
    pub relation: String,
    /// `None` for `is empty`.
    pub file: Option<ExpectedFile>,
}

pub struct ExpectedFile {
    /// The path as the test writes it.
    pub display: String,
    /// The relation's columns the file has, in the file's order.
    pub columns: Vec<String>,
    /// The file's rows, one value per column: `None` for an empty field, the text of a quoted
    /// one (`""` is the empty string).
    pub rows: Vec<Record>,
}

/// The fields of one line of a CSV file: `None` for an empty field.
type Record = Vec<Option<String>>;

/// The tests the entry file declares (not those of imported files), in order.
pub fn entry_tests(loaded: &Loaded) -> Vec<&TestDecl> {
    loaded
        .files
        .last()
        .map(|f| {
            f.statements
                .iter()
                .filter_map(|s| match s {
                    Statement::Test(t) => Some(t),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// M701 for every test whose name an earlier test of the program (any file) already has.
pub fn duplicate_names(loaded: &Loaded) -> Vec<Diagnostic> {
    let mut first: HashMap<&str, Span> = HashMap::new();
    let mut out = Vec::new();
    for s in loaded.statements() {
        let Statement::Test(t) = s else { continue };
        match first.get(t.name.name.as_str()) {
            Some(prev) => out.push(
                Diagnostic::error(
                    "M701",
                    format!("test `{}` is defined more than once", t.name.name),
                )
                .label(t.name.span, "this test has the same name")
                .label(*prev, "first defined here")
                .help("give each test its own name"),
            ),
            None => {
                first.insert(&t.name.name, t.name.span);
            }
        }
    }
    out
}

/// The diagnostics of `test` that `base` (the program without test changes) does not have: the
/// same code and message at the same place is the same diagnostic.
pub fn new_diagnostics(base: &[Diagnostic], test: &[Diagnostic]) -> Vec<Diagnostic> {
    let at = |d: &Diagnostic| d.labels.first().map(|l| l.span);
    test.iter()
        .filter(|d| {
            !base
                .iter()
                .any(|b| b.code == d.code && b.message == d.message && at(b) == at(d))
        })
        .cloned()
        .collect()
}

/// What a `given` puts in place of a source's declaration.
enum Given<'t> {
    Path(&'t ast::StrLit),
    Source(&'t ast::SourceDecl),
}

/// Build and analyse `test`'s program. `today` applies unless the test pins its own. Fails only
/// when DuckDB cannot start.
pub fn prepare(loaded: &Loaded, test: &TestDecl, today: &str) -> Result<Prepared, String> {
    let mut diags = Diagnostics::default();
    let sources: Vec<&ast::SourceDecl> = loaded
        .statements()
        .filter_map(|s| match s {
            Statement::Source(d) => Some(d),
            _ => None,
        })
        .collect();
    let mut today_at: Option<Span> = None;
    let mut today = today.to_string();
    // given source name (lower case) -> (the given's name, what replaces the declaration)
    let mut givens: HashMap<String, (&ast::Ident, Given)> = HashMap::new();
    for item in &test.items {
        let (name, given) = match item {
            TestItem::Today { date, span } => {
                if let Some(prev) = today_at {
                    diags.push(
                        Diagnostic::error("M703", "`today:` is given twice")
                            .label(*span, "second `today:`")
                            .label(prev, "first here"),
                    );
                    continue;
                }
                today_at = Some(*span);
                match resolve::parse_date(&date.value) {
                    Ok(d) => today = d,
                    Err(e) => diags.push(
                        Diagnostic::error("M703", format!("invalid `today:` date: {e}"))
                            .label(date.span, "the date `today()` returns in this test"),
                    ),
                }
                continue;
            }
            TestItem::GivenPath { name, path, .. } => (name, Given::Path(path)),
            TestItem::GivenSource(d) => (&d.name, Given::Source(d)),
            TestItem::Expect { .. } => continue,
        };
        let key = name.name.to_ascii_lowercase();
        if let Some((prev, _)) = givens.get(&key) {
            diags.push(
                Diagnostic::error("M702", format!("source `{}` is given twice", name.name))
                    .label(name.span, "given again")
                    .label(prev.span, "first given here"),
            );
            continue;
        }
        let Some(decl) = sources
            .iter()
            .find(|d| d.name.name.eq_ignore_ascii_case(&name.name))
        else {
            let mut d = Diagnostic::error(
                "M702",
                format!("`{}` is not a source of the program", name.name),
            )
            .label(name.span, "no source has this name");
            let names = sources.iter().map(|d| d.name.name.as_str());
            if let Some(s) = did_you_mean(&name.name, names) {
                d = d.help(format!("did you mean `{s}`?"));
            }
            diags.push(d);
            continue;
        };
        if let Given::Path(path) = given
            && decl.kind.name == "sql"
        {
            diags.push(
                Diagnostic::error(
                    "M702",
                    format!("source `{}` reads a database, not a file", name.name),
                )
                .label(path.span, "a path replaces a file source's path")
                .help(format!(
                    "give a whole declaration: `given {} = csv(\"{}\")`",
                    name.name, path.value
                )),
            );
            continue;
        }
        givens.insert(key, (name, given));
    }

    let program = loaded.without_outputs(|d| match givens.get(&d.name.name.to_ascii_lowercase()) {
        Some((_, g)) => substitute(d, g),
        None => d.clone(),
    });
    let mut reader = Reader::new(false)?;
    let (hir, analysed) = resolve::analyze(
        &program,
        &mut reader as &mut dyn SchemaProvider,
        &Options { today },
    );
    diags.extend(analysed.list);

    for s in &hir.sources {
        if matches!(s.kind, SourceKind::Sql { .. }) {
            diags.push(
                Diagnostic::error(
                    "M707",
                    format!(
                        "test `{}` reads SQL source `{}`; give it a file (`given {} = csv(...)`)",
                        test.name.name, s.name, s.name
                    ),
                )
                .label(test.name.span, "this test")
                .help("tests read only files, so they run the same way on every machine"),
            );
        }
    }

    let mut expects = Vec::new();
    for item in &test.items {
        let TestItem::Expect { rel, file, .. } = item else {
            continue;
        };
        let name = rel.display();
        let Some(relation) = hir.relation(&name) else {
            if let Some(d) = unknown_relation(&hir, &program, rel) {
                diags.push(d);
            }
            continue;
        };
        let Some(lit) = file else {
            expects.push(Expect {
                relation: relation.name.clone(),
                file: None,
            });
            continue;
        };
        let path = program.sources.base_dir(lit.span.file).join(&lit.value);
        let (header, rows) = match read_expected(&path) {
            Ok(parsed) => parsed,
            Err(e) => {
                diags.push(
                    Diagnostic::error("M705", format!("cannot read `{}`: {e}", lit.value))
                        .label(lit.span, "expected rows")
                        .help("write it like a MAGI CSV export: a header line, then comma-separated values in `\"` quotes where needed"),
                );
                continue;
            }
        };
        if relation.open {
            // columns unknown: only for a SQL source without schema, which a test cannot keep
            continue;
        }
        let mut columns: Vec<String> = Vec::new();
        let mut ok = true;
        for h in &header {
            let Some(col) = relation
                .columns
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(h))
            else {
                let mut d = Diagnostic::error(
                    "M706",
                    format!(
                        "`{}` has a column `{h}` that `{}` does not have",
                        lit.value, relation.name
                    ),
                )
                .label(lit.span, "its header names the columns compared");
                let names = relation.columns.iter().map(|c| c.name.as_str());
                d = match did_you_mean(h, names) {
                    Some(s) => d.help(format!("did you mean `{s}`?")),
                    None => d.help(format!(
                        "`{}` has {}",
                        relation.name,
                        relation
                            .columns
                            .iter()
                            .map(|c| format!("`{}`", c.name))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                };
                diags.push(d);
                ok = false;
                continue;
            };
            if columns.contains(&col.name) {
                diags.push(
                    Diagnostic::error(
                        "M706",
                        format!("`{}` names column `{}` twice", lit.value, col.name),
                    )
                    .label(lit.span, "its header names the columns compared")
                    .help("names are case-insensitive; keep one of them"),
                );
                ok = false;
                continue;
            }
            columns.push(col.name.clone());
        }
        if ok {
            expects.push(Expect {
                relation: relation.name.clone(),
                file: Some(ExpectedFile {
                    display: lit.value.clone(),
                    columns,
                    rows,
                }),
            });
        }
    }

    Ok(Prepared {
        loaded: program,
        hir,
        diags,
        reader,
        expects,
    })
}

/// `decl` with a test's `given` in its place; the declaration keeps its name.
fn substitute(decl: &ast::SourceDecl, given: &Given) -> ast::SourceDecl {
    match given {
        Given::Path(path) => ast::SourceDecl {
            args: vec![ast::Expr {
                kind: ast::ExprKind::Literal(ast::Literal::Str(path.value.clone())),
                span: path.span,
            }],
            ..decl.clone()
        },
        Given::Source(d) => ast::SourceDecl {
            name: ast::Ident {
                name: decl.name.name.clone(),
                ..d.name.clone()
            },
            ..(*d).clone()
        },
    }
}

/// M704 for an `expect` of a relation the program does not have, unless the relation failed to
/// resolve (its error is already reported).
fn unknown_relation(hir: &Hir, program: &Loaded, rel: &ast::RelRef) -> Option<Diagnostic> {
    let base = &rel.name.name;
    let declared = program.statements().any(|s| match s {
        Statement::Source(d) => &d.name.name == base,
        Statement::Dataset(d) => &d.name.name == base,
        Statement::Reconcile(d) => &d.name.name == base,
        _ => false,
    });
    // a reconciliation has only its parts (`r.matches`, ...) as relations
    let parts = format!("{base}.");
    let resolved = hir
        .relations
        .iter()
        .any(|r| &r.name == base || r.name.starts_with(&parts));
    if declared && !resolved {
        return None;
    }
    let name = rel.display();
    let mut d = Diagnostic::error(
        "M704",
        format!("the program has no relation `{name}` to compare"),
    )
    .label(rel.span, "not a relation of the program");
    let names = hir.relations.iter().map(|r| r.name.as_str());
    if let Some(s) = did_you_mean(&name, names) {
        d = d.help(format!("did you mean `{s}`?"));
    }
    Some(d)
}

/// Read an expected-rows file as MAGI's CSV export writes it: a header line, then records of
/// comma-separated fields, `"`-quoted where needed (`""` is a quote inside quotes); lines end
/// with LF or CRLF. An empty field is null; a quoted empty field is the empty string.
fn read_expected(path: &Path) -> Result<(Vec<String>, Vec<Record>), String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut records = parse_csv(text)?.into_iter();
    let Some((_, header)) = records.next() else {
        return Err("the file is empty; its first line must name the columns".into());
    };
    let header = header
        .into_iter()
        .enumerate()
        .map(|(i, h)| match h {
            Some(h) if !h.trim().is_empty() => Ok(h),
            _ => Err(format!("header field {} is empty", i + 1)),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut rows = Vec::new();
    for (line, fields) in records {
        if fields.len() != header.len() {
            return Err(format!(
                "line {line} has {} field(s), the header has {}",
                fields.len(),
                header.len()
            ));
        }
        rows.push(fields);
    }
    Ok((header, rows))
}

/// Records of `text` with the line each starts on. A line break after the last record does not
/// start another one.
fn parse_csv(text: &str) -> Result<Vec<(usize, Record)>, String> {
    let mut records = Vec::new();
    let mut chars = text.chars().peekable();
    let mut line = 1;
    while chars.peek().is_some() {
        let start = line;
        let mut fields = Vec::new();
        loop {
            let field = if chars.peek() == Some(&'"') {
                chars.next();
                let mut value = String::new();
                loop {
                    match chars.next() {
                        Some('"') if chars.peek() == Some(&'"') => {
                            chars.next();
                            value.push('"');
                        }
                        Some('"') => break,
                        Some(c) => {
                            if c == '\n' {
                                line += 1;
                            }
                            value.push(c);
                        }
                        None => return Err(format!("line {start}: a quoted field is not closed")),
                    }
                }
                if !matches!(chars.peek(), None | Some(',' | '\n' | '\r')) {
                    return Err(format!(
                        "line {line}: text after a closing quote (a quote inside quotes is written `\"\"`)"
                    ));
                }
                Some(value)
            } else {
                let mut value = String::new();
                while let Some(&c) = chars.peek() {
                    if matches!(c, ',' | '\n' | '\r') {
                        break;
                    }
                    value.push(c);
                    chars.next();
                }
                (!value.is_empty()).then_some(value)
            };
            fields.push(field);
            match chars.next() {
                Some(',') => continue,
                Some('\r') if chars.peek() == Some(&'\n') => {
                    chars.next();
                }
                Some('\n') | None => {}
                Some(_) => return Err(format!("line {line}: a line ends with a lone CR")),
            }
            line += 1;
            break;
        }
        records.push((start, fields));
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::parse_csv;

    #[test]
    fn csv_fields_distinguish_null_from_empty_text() {
        let rows = parse_csv("a,b\r\n,\"\"\n\"x,\"\"y\"\"\nz\",1\n").unwrap();
        assert_eq!(
            rows,
            vec![
                (1, vec![Some("a".into()), Some("b".into())]),
                (2, vec![None, Some(String::new())]),
                (3, vec![Some("x,\"y\"\nz".into()), Some("1".into())]),
            ]
        );
    }

    #[test]
    fn csv_empty_line_is_one_null_field_but_final_break_is_not_a_record() {
        let rows = parse_csv("a\n\nb\n").unwrap();
        assert_eq!(
            rows,
            vec![
                (1, vec![Some("a".into())]),
                (2, vec![None]),
                (3, vec![Some("b".into())]),
            ]
        );
    }
}
