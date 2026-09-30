//! Executes a physical plan in an in-memory DuckDB database. Everything lives in this one
//! database for the duration of the run and is discarded afterwards.

use std::time::Instant;

use crate::ast::CheckSeverity;
use crate::backend::{duckdb as lower, sql};
use crate::diagnostic::Diagnostic;
use crate::plan::logical::LogicalPlan;
use crate::plan::physical::{PhysicalPlan, Step};
use crate::reconcile::lower::{Op, ReconcileProgram, TierMode, subset};
use crate::semantic::hir::{
    CheckKind, Hir, Lit, Materialization, TExpr, TExprKind, Validation, WinFunc,
};
use crate::semantic::types::{ColType, Type};
use crate::source::Reader;

pub struct RunOptions {
    /// Treat failed `require` checks like `expect`: keep going and write outputs.
    pub keep_going: bool,
}

pub struct Outcome {
    pub diagnostics: Vec<Diagnostic>,
    /// Some check or step failed; the exit status must be non-zero.
    pub failed: bool,
}

pub struct Runner<'a> {
    hir: &'a Hir,
    pub conn: duckdb::Connection,
    diags: Vec<Diagnostic>,
    failed: bool,
    /// Exports written to temp files, moved into place once every step has run.
    staged: Vec<(crate::export::Staged, usize)>,
    log: &'a mut dyn FnMut(&str),
}

type StepResult = Result<(), Stop>;
/// The run cannot continue.
struct Stop;

/// The session settings `magi run` applies before any step (`magi sql` prints them too).
pub fn session_settings(hir: &Hir) -> Vec<sql::Stmt> {
    let mut settings = vec![
        (
            "preserve_insertion_order",
            sql::Expr::Lit(sql::Lit::Bool(true)),
        ),
        // timestamp_tz arithmetic and text must not depend on the machine running MAGI
        ("TimeZone", sql::str_lit("UTC")),
    ];
    // intermediate data stays in memory unless a spill directory is configured
    let temp = hir
        .runtime
        .temp_directory
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    settings.push(("temp_directory", sql::str_lit(&temp)));
    if let Some(t) = hir.runtime.threads {
        settings.push(("threads", sql::int(t as i64)));
    }
    if let Some(m) = &hir.runtime.memory_limit {
        settings.push(("memory_limit", sql::str_lit(m)));
    }
    settings
        .into_iter()
        .map(|(name, value)| sql::Stmt::Set {
            name: name.into(),
            value,
        })
        .collect()
}

pub fn open(hir: &Hir) -> Result<duckdb::Connection, String> {
    let conn =
        duckdb::Connection::open_in_memory().map_err(|e| format!("cannot start DuckDB: {e}"))?;
    for stmt in session_settings(hir) {
        conn.execute_batch(&sql::render(&stmt)).map_err(|e| {
            let name = match &stmt {
                sql::Stmt::Set { name, .. } => name.as_str(),
                _ => "?",
            };
            format!("invalid runtime setting `{name}`: {e}")
        })?;
    }
    Ok(conn)
}

pub fn execute(
    hir: &Hir,
    plan: &PhysicalPlan,
    reader: &mut Reader,
    options: &RunOptions,
    log: &mut dyn FnMut(&str),
) -> Outcome {
    let conn = match open(hir) {
        Ok(c) => c,
        Err(e) => {
            return Outcome {
                diagnostics: vec![Diagnostic::error("M400", e)],
                failed: true,
            };
        }
    };
    let mut r = Runner {
        hir,
        conn,
        diags: Vec::new(),
        failed: false,
        staged: Vec::new(),
        log,
    };
    let total = plan.steps.len();
    let mut completed = true;
    for (i, step) in plan.steps.iter().enumerate() {
        let started = Instant::now();
        let res = match step {
            Step::Stage { source } => r.stage(*source, reader),
            Step::Materialize { dataset, kind } => r.materialize(*dataset, *kind),
            Step::Validate { validation } => r.validate(&hir.validations[*validation], options),
            Step::Reconcile { reconcile } => r.reconcile(*reconcile),
            Step::Export { export } => r.export(*export),
        };
        let ms = started.elapsed().as_millis();
        (r.log)(&format!("  [{}/{total}] done in {ms} ms", i + 1));
        if res.is_err() {
            r.failed = true;
            completed = false;
            break;
        }
    }
    // outputs change only when every step ran, and then all of them or none: a stopped run or a
    // failed commit leaves the previous files alone
    let (staged, index): (Vec<_>, Vec<_>) = std::mem::take(&mut r.staged).into_iter().unzip();
    if !completed {
        for s in staged {
            s.discard();
        }
    } else if let Err((k, msg)) = crate::export::commit(staged) {
        let e = &hir.exports[index[k]];
        r.diags.push(
            Diagnostic::error("M505", format!("cannot write `{}`: {msg}", e.display_path))
                .label(e.span, "this export"),
        );
        r.failed = true;
        completed = false;
    }
    if !completed && !hir.exports.is_empty() {
        (r.log)("No output files were changed");
    }
    Outcome {
        diagnostics: r.diags,
        failed: r.failed,
    }
}

fn plain(msg: impl Into<String>) -> Diagnostic {
    Diagnostic::error("M400", msg)
}

/// The relation checks read, and the `(column, failures column)` pairs copied into
/// `<target>.failures`. For a source it is a view adding each row's `source_row`.
pub fn validation_base(hir: &Hir, v: &Validation) -> (String, Vec<(String, String)>) {
    let rel = hir.relation(&v.target).expect("validated relation");
    let is_source = hir.sources.iter().any(|s| s.name == v.target);
    let mut cols = Vec::new();
    if is_source {
        cols.push(("__magi_source_row".to_string(), "source_row".to_string()));
    }
    cols.extend(rel.columns.iter().map(|c| {
        let out = if matches!(c.name.as_str(), "check" | "severity" | "source_row") {
            format!("row_{}", c.name)
        } else {
            c.name.clone()
        };
        (c.name.clone(), out)
    }));
    let base = if is_source {
        format!("__magi_vsrc_{}", v.target)
    } else {
        v.target.clone()
    };
    (base, cols)
}

/// `CREATE VIEW` of the validation base for a source target (see [`validation_base`]): the
/// source's typed rows with their source row, never DuckDB's `rowid` (a source column may be
/// named `rowid`).
pub fn validation_base_sql(hir: &Hir, v: &Validation) -> Option<String> {
    let src = hir.sources.iter().find(|s| s.name == v.target)?;
    let (base, _) = validation_base(hir, v);
    let row = sql::ident(crate::source::ROW);
    Some(format!(
        "CREATE OR REPLACE TEMP VIEW {} AS SELECT * EXCLUDE ({row}), {row} AS __magi_source_row FROM {};",
        sql::ident(&base),
        sql::ident(&crate::source::typed_name(&src.name))
    ))
}

/// The query a check runs: the failing rows as `<target>.failures` rows for a row check, or one
/// row `(ok, measured)` for an aggregate check — `measured` is the compared aggregate
/// (`missing(x)` in `missing(x) < 0.05`) as text.
pub fn check_query(
    base: &str,
    out_cols: &[(String, String)],
    c: &crate::semantic::hir::Check,
) -> sql::Query {
    let u = ColType::nullable(Type::Unknown);
    let bool_ty = ColType::required(Type::Bool);
    let not = |e: TExpr| {
        TExpr::new(
            TExprKind::Unary {
                op: crate::ast::UnaryOp::Not,
                expr: Box::new(e),
            },
            bool_ty,
        )
    };
    let failing = match &c.kind {
        CheckKind::Row(pred) => {
            let ok = TExpr::new(
                TExprKind::Call {
                    func: "coalesce",
                    args: vec![pred.clone(), TExpr::lit(Lit::Bool(true))],
                },
                bool_ty,
            );
            LogicalPlan::scan(base).filter(not(ok))
        }
        CheckKind::NotNull(e) => LogicalPlan::scan(base).filter(TExpr::new(
            TExprKind::IsNull {
                expr: Box::new(e.clone()),
                negated: false,
            },
            bool_ty,
        )),
        CheckKind::Unique(cols) => {
            let n = TExpr::new(
                TExprKind::Window {
                    func: WinFunc::Count,
                    arg: None,
                    partition: cols.iter().map(|x| TExpr::col(0, x.clone(), u)).collect(),
                    order: Vec::new(),
                    filter: None,
                },
                u,
            );
            let gt = TExpr::new(
                TExprKind::Binary {
                    op: crate::ast::BinaryOp::Gt,
                    left: Box::new(TExpr::col(0, "__magi_n", u)),
                    right: Box::new(TExpr::lit(Lit::Int(1))),
                },
                u,
            );
            LogicalPlan::scan(base)
                .window(vec![(n, "__magi_n".into())])
                .filter(gt)
        }
        CheckKind::Aggregate(pred) => {
            use crate::ast::BinaryOp as B;
            let value = match &pred.kind {
                TExprKind::Binary {
                    op: B::Eq | B::NotEq | B::Lt | B::Le | B::Gt | B::Ge,
                    left,
                    right,
                } => Some(if matches!(left.kind, TExprKind::Literal(_)) {
                    right
                } else {
                    left
                }),
                _ => None,
            };
            let measured = value.map_or(TExpr::lit(Lit::Null), |v| {
                TExpr::new(
                    TExprKind::TryCast {
                        expr: v.clone(),
                        ty: Type::String,
                    },
                    ColType::nullable(Type::String),
                )
            });
            let items = vec![
                (pred.clone(), "ok".to_string()),
                (measured, "measured".into()),
            ];
            return lower::query(&LogicalPlan::scan(base).aggregate(Vec::new(), items));
        }
    };
    let mut items: Vec<(TExpr, String)> = vec![
        (TExpr::lit(Lit::Str(c.label.clone())), "check".into()),
        (
            TExpr::lit(Lit::Str(c.severity.keyword().into())),
            "severity".into(),
        ),
    ];
    items.extend(
        out_cols
            .iter()
            .map(|(src, out)| (TExpr::col(0, src.clone(), u), out.clone())),
    );
    lower::query(&failing.project(items))
}

/// DuckDB error text without the data values it may quote (diagnostics never print
/// source data). Errors about values (conversion, invalid input, out of range, constraint, CSV)
/// keep only the text before the first quote or number of their first line, followed by a fixed
/// note; everything after it may be (or echo) a value. Other errors (binder, catalog, parser,
/// ...) name program objects and are kept, except CSV `Original Line:` echoes.
pub fn redact(err: &dyn std::fmt::Display) -> String {
    const DATA_ERRORS: [&str; 5] = [
        "Conversion Error",
        "Invalid Input Error",
        "Out of Range Error",
        "Constraint Error",
        "CSV Error",
    ];
    let text = err.to_string();
    let first = text.lines().next().unwrap_or_default();
    if DATA_ERRORS.iter().any(|p| first.contains(p)) {
        let kept = &first[..data_start(first).unwrap_or(first.len())];
        let kept = kept.trim_end_matches(|c: char| c.is_whitespace() || "([{<:,;=-".contains(c));
        return format!("{kept} (details withheld: they quote data)");
    }
    text.lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if trimmed.starts_with("Original Line") || trimmed.starts_with("Line:") {
                format!(
                    "{}Original Line: <redacted>",
                    &line[..line.len() - trimmed.len()]
                )
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Byte offset of the first quote, or of the first digit that starts a number (not one inside a
/// word like `INT64`).
fn data_start(text: &str) -> Option<usize> {
    let mut prev: Option<char> = None;
    for (i, c) in text.char_indices() {
        let number = c.is_ascii_digit() && !prev.is_some_and(|p| p.is_alphanumeric() || p == '_');
        if matches!(c, '\'' | '"' | '`') || number {
            return Some(i);
        }
        prev = Some(c);
    }
    None
}

impl Runner<'_> {
    fn exec(
        &mut self,
        sql_text: &str,
        span: Option<crate::syntax::span::Span>,
        what: &str,
    ) -> StepResult {
        self.conn.execute_batch(sql_text).map_err(|e| {
            let mut d = plain(format!("{what} failed: {}", redact(&e)));
            if let Some(s) = span {
                d = d.label(s, "while executing this");
            }
            self.diags.push(d);
            Stop
        })
    }

    fn scalar_i64(&mut self, q: &str) -> Result<i64, Stop> {
        self.conn
            .query_row(q, [], |r| r.get::<_, Option<i64>>(0))
            .map(|v| v.unwrap_or(0))
            .map_err(|e| {
                self.diags
                    .push(plain(format!("internal query failed: {}", redact(&e))));
                Stop
            })
    }

    fn count(&mut self, table: &str) -> Result<i64, Stop> {
        self.scalar_i64(&format!("SELECT count(*) FROM {}", sql::ident(table)))
    }

    /// The relation DuckDB produced must have exactly the columns analysis promised; anything
    /// else is a MAGI bug, reported instead of silently exporting different columns.
    fn verify(&mut self, relation: &str) -> StepResult {
        let Some(rel) = self.hir.relation(relation) else {
            return Ok(());
        };
        if rel.open {
            return Ok(());
        }
        let actual = crate::export::describe(&self.conn, relation).map_err(|e| {
            self.diags.push(plain(format!(
                "cannot describe `{relation}`: {}",
                redact(&e)
            )));
            Stop
        })?;
        let expected: Vec<&str> = rel.columns.iter().map(|c| c.name.as_str()).collect();
        let got: Vec<&str> = actual.iter().map(|(n, _)| n.as_str()).collect();
        if expected != got {
            self.diags.push(plain(format!(
                "internal error: `{relation}` has columns [{}] but analysis expected [{}]",
                got.join(", "),
                expected.join(", ")
            )));
            return Err(Stop);
        }
        if std::env::var_os("MAGI_DEBUG_TYPES").is_some() {
            for (c, (_, t)) in rel.columns.iter().zip(&actual) {
                let actual_ty = Type::from_duckdb(t);
                if c.ty.ty != actual_ty && !matches!(c.ty.ty, Type::Unknown | Type::Null) {
                    eprintln!(
                        "type drift: {relation}.{} analysed {} but DuckDB has {t}",
                        c.name, c.ty.ty
                    );
                }
            }
        }
        Ok(())
    }

    fn stage(&mut self, i: usize, reader: &mut Reader) -> StepResult {
        let src = &self.hir.sources[i];
        (self.log)(&format!(
            "Load source {} from {}",
            src.name,
            src.kind.describe()
        ));
        let rel = self.hir.relation(&src.name).expect("source relation");
        let report = match crate::source::stage(&self.conn, src, rel, reader, &self.hir.connections)
        {
            Ok(r) => r,
            Err(e) => {
                self.diags.push(
                    Diagnostic::error(
                        "M200",
                        format!("cannot load source `{}`: {}", src.name, redact(&e)),
                    )
                    .label(src.span, src.kind.describe()),
                );
                return Err(Stop);
            }
        };
        (self.log)(&format!("  {} rows", report.rows));
        let rows = |r: &[i64]| {
            r.iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        for (col, expected, n, first) in &report.rejects {
            self.diags.push(
                Diagnostic::warning("M208", format!("source `{}`: {n} value(s) in column `{col}` are not valid {expected} and became null", src.name))
                    .label(src.span, format!("first at source row(s) {}", rows(first)))
                    .help(format!("inspect them with `export {}.rejects to \"rejects.csv\"`, or read the column as text and parse it explicitly", src.name)),
            );
        }
        for (col, scale, n) in &report.rounded {
            self.diags.push(
                Diagnostic::warning("M209", format!("source `{}`: {n} value(s) in column `{col}` have more than {scale} decimal places and were rounded", src.name))
                    .label(src.span, "declared scale is too small")
                    .help("increase the scale in the declared `decimal(p, s)`"),
            );
        }
        let mut stop = false;
        for (col, n, first) in &report.null_violations {
            self.diags.push(
                Diagnostic::error("M210", format!("source `{}`: column `{col}` is declared non-null but {n} row(s) have no value", src.name))
                    .label(src.span, format!("first at source row(s) {}", rows(first)))
                    .help("declare it nullable (`type?`) if missing values are expected"),
            );
            stop = true;
        }
        if let Some((dups, nulls)) = report.identity {
            self.diags.push(
                Diagnostic::error(
                    "M211",
                    format!(
                        "source `{}`: identity is not usable ({dups} duplicate, {nulls} null)",
                        src.name
                    ),
                )
                .label(src.span, "identity must be unique and present")
                .help("pick columns that identify each row, or remove `identity`"),
            );
            stop = true;
        }
        if stop {
            return Err(Stop);
        }
        self.verify(&src.name)
    }

    fn materialize(&mut self, i: usize, kind: Materialization) -> StepResult {
        let d = &self.hir.datasets[i];
        (self.log)(&format!("Build dataset {}", d.name));
        let q = lower::query(&d.plan);
        let stmt = match kind {
            Materialization::Table => sql::Stmt::CreateTable {
                name: d.name.clone(),
                query: q,
                temp: true,
            },
            Materialization::View => sql::Stmt::CreateView {
                name: d.name.clone(),
                query: q,
            },
        };
        let span = d.span;
        self.exec(
            &sql::render(&stmt),
            Some(span),
            &format!("dataset `{}`", d.name),
        )?;
        self.verify(&d.name.clone())?;
        if kind == Materialization::Table {
            let n = self.count(&d.name)?;
            (self.log)(&format!("  {n} rows"));
        }
        Ok(())
    }

    fn validate(&mut self, v: &Validation, options: &RunOptions) -> StepResult {
        (self.log)(&format!("Validate {}", v.target));
        let total = self.count(&v.target)?;
        let (base, out_cols) = validation_base(self.hir, v);
        if let Some(create) = validation_base_sql(self.hir, v) {
            self.exec(&create, Some(v.span), "validation")?;
        }
        let mut failure_parts = Vec::new();
        let mut check_rows = Vec::new();
        let mut require_failed = false;
        for (k, c) in v.checks.iter().enumerate() {
            let sev = c.severity.keyword();
            let mut measured: Option<String> = None;
            let mut null_aggregate = false;
            let q = sql::render_query(&check_query(&base, &out_cols, c));
            let (status, failing) = if matches!(c.kind, CheckKind::Aggregate(_)) {
                let (ok, m): (Option<bool>, Option<String>) = self
                    .conn
                    .query_row(&q, [], |r| Ok((r.get(0)?, r.get(1)?)))
                    .map_err(|e| {
                        self.diags.push(
                            plain(format!("check `{}` failed to run: {}", c.label, redact(&e)))
                                .label(c.span, "this check"),
                        );
                        Stop
                    })?;
                measured = m;
                // `sum(x) > 0` over no rows (or only nulls) compares null: that is not a pass
                null_aggregate = ok.is_none();
                (if ok == Some(true) { "pass" } else { "fail" }, None)
            } else {
                let table = format!("__magi_vfail_{}_{k}", v.target);
                self.exec(
                    &format!("CREATE TEMP TABLE {} AS {q}", sql::ident(&table)),
                    Some(c.span),
                    &format!("check `{}`", c.label),
                )?;
                let n = self.count(&table)?;
                failure_parts.push(table);
                (if n > 0 { "fail" } else { "pass" }, Some(n))
            };
            if status == "fail" {
                let rows = match failing {
                    Some(n) => format!("{n} of {total} rows fail"),
                    None if null_aggregate => {
                        "fails because the aggregate is null (no rows, or only nulls)".into()
                    }
                    None => "fails for the relation".into(),
                };
                let msg = format!("{sev} `{}` on `{}`: {rows}", c.label, v.target);
                let help = if failing.is_some() {
                    Some(format!(
                        "export the failing rows: `export {}.failures to \"failures.csv\"`",
                        v.target
                    ))
                } else if null_aggregate {
                    None
                } else {
                    Some(format!(
                        "the measured value is in the `measured` column of `{}.checks`",
                        v.target
                    ))
                };
                let mut d = match c.severity {
                    CheckSeverity::Require => {
                        require_failed = true;
                        Diagnostic::error("M402", msg)
                    }
                    CheckSeverity::Expect => {
                        self.failed = true;
                        Diagnostic::error("M403", msg)
                    }
                    CheckSeverity::Warn => Diagnostic::warning("M404", msg),
                };
                d = d.label(c.span, "failed");
                if let Some(h) = help {
                    d = d.help(h);
                }
                self.diags.push(d);
            }
            (self.log)(&format!(
                "  {} {sev} {}",
                if status == "pass" { "ok  " } else { "FAIL" },
                c.label
            ));
            check_rows.push((c.label.clone(), sev, status, failing, total, measured));
        }
        // `<target>.failures`
        let failures = format!("{}.failures", v.target);
        if failure_parts.is_empty() {
            let mut items: Vec<(TExpr, String)> = vec![
                (TExpr::lit(Lit::Str(String::new())), "check".into()),
                (TExpr::lit(Lit::Str(String::new())), "severity".into()),
            ];
            items.extend(out_cols.iter().map(|(src, out)| {
                (
                    TExpr::col(0, src.clone(), ColType::nullable(Type::Unknown)),
                    out.clone(),
                )
            }));
            let never = TExpr::lit(Lit::Bool(false));
            let q = lower::query(&LogicalPlan::scan(&base).filter(never).project(items));
            self.exec(
                &sql::render(&sql::Stmt::CreateTable {
                    name: failures.clone(),
                    query: q,
                    temp: true,
                }),
                Some(v.span),
                "validation",
            )?;
        } else {
            let parts = failure_parts.iter().map(LogicalPlan::scan).collect();
            let q = lower::query(&LogicalPlan::Union { inputs: parts });
            self.exec(
                &sql::render(&sql::Stmt::CreateTable {
                    name: failures.clone(),
                    query: q,
                    temp: true,
                }),
                Some(v.span),
                "validation",
            )?;
            for t in &failure_parts {
                self.exec(
                    &sql::render(&sql::Stmt::Drop {
                        name: t.clone(),
                        view: false,
                    }),
                    None,
                    "cleanup",
                )?;
            }
        }
        if base != v.target {
            self.exec(
                &sql::render(&sql::Stmt::Drop {
                    name: base.clone(),
                    view: true,
                }),
                None,
                "cleanup",
            )?;
        }
        // `<target>.checks`: fixed column types whichever checks exist
        const COLUMNS: [(&str, &str); 6] = [
            ("check", "VARCHAR"),
            ("severity", "VARCHAR"),
            ("status", "VARCHAR"),
            ("failing_rows", "BIGINT"),
            ("total_rows", "BIGINT"),
            ("measured", "VARCHAR"),
        ];
        let null = || sql::Expr::Lit(sql::Lit::Null);
        let rows: Vec<sql::Query> = check_rows
            .iter()
            .map(|(label, sev, status, failing, total, measured)| {
                let values = [
                    sql::str_lit(label),
                    sql::str_lit(sev),
                    sql::str_lit(status),
                    failing.map_or_else(null, sql::int),
                    sql::int(*total),
                    measured.as_deref().map_or_else(null, sql::str_lit),
                ];
                sql::Query::select(sql::Select {
                    items: values
                        .into_iter()
                        .zip(COLUMNS)
                        .map(|(value, (name, ty))| {
                            let cast = sql::Expr::Cast {
                                expr: Box::new(value),
                                ty: ty.into(),
                                try_: false,
                            };
                            (cast, Some(name.into()))
                        })
                        .collect(),
                    ..sql::Select::default()
                })
            })
            .collect();
        let checks = format!("{}.checks", v.target);
        let stmt = if rows.is_empty() {
            sql::Stmt::CreateEmptyTable {
                name: checks,
                columns: COLUMNS
                    .iter()
                    .map(|(n, t)| (n.to_string(), t.to_string()))
                    .collect(),
            }
        } else {
            sql::Stmt::CreateTable {
                name: checks,
                query: sql::Query {
                    ctes: Vec::new(),
                    body: sql::Body::UnionAllByName(rows),
                },
                temp: true,
            }
        };
        self.exec(&sql::render(&stmt), Some(v.span), "validation")?;
        self.verify(&format!("{}.failures", v.target))?;
        self.verify(&format!("{}.checks", v.target))?;
        if require_failed {
            if options.keep_going {
                self.failed = true;
            } else {
                self.diags.push(
                    Diagnostic::error("M405", "stopping: a `require` check failed; no outputs were written")
                        .help("fix the data or the check; `magi run --keep-going` writes outputs anyway (and still exits with an error)"),
                );
                return Err(Stop);
            }
        }
        Ok(())
    }

    fn run_ops(&mut self, ops: &[Op], rc_span: crate::syntax::span::Span) -> StepResult {
        for op in ops {
            let text = render_op(op);
            self.exec(&text, Some(rc_span), "reconciliation")?;
        }
        Ok(())
    }

    /// Members per unit: (Σ over units of the subsets of up to `max_items` members, units,
    /// members of the largest unit).
    fn subset_members(&mut self, sub: &subset::Program) -> Result<(u64, u64, u64), Stop> {
        let q = sql::render_query(&lower::query(&sub.budget));
        let counts: Result<Vec<(i64, i64)>, duckdb::Error> =
            self.conn.prepare(&q).and_then(|mut stmt| {
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect()
            });
        let counts = counts.map_err(|e| {
            self.diags
                .push(plain(format!("internal query failed: {}", redact(&e))));
            Stop
        })?;
        let (mut total, mut units, mut largest) = (0u64, 0u64, 0u64);
        for (n, k) in counts {
            let (n, k) = (n.max(0) as u64, k.max(0) as u64);
            total = total.saturating_add(subset::subsets(n, sub.max_items).saturating_mul(k));
            units += k;
            largest = largest.max(n);
        }
        Ok((total, units, largest))
    }

    /// Enumerates a subset tier's subsets level by level. Without a sum target the run first
    /// counts every subset it could build and stops when they exceed `max_subsets`; with one it
    /// counts the subsets each level examines and stops as soon as the total exceeds it.
    fn subset_enumerate(
        &mut self,
        sub: &subset::Program,
        index: usize,
        tier: &str,
        span: crate::syntax::span::Span,
    ) -> StepResult {
        let (upper, units, largest) = self.subset_members(sub)?;
        let help = "give each unit fewer members with blocking keys or member-level `require`s (conditions without aggregates over the subset side), or lower `max_items`; `max_subsets` raises the limit";
        let Some(pruning) = &sub.pruning else {
            (self.log)(&format!(
                "  tier {index} {tier}: {upper} subsets of up to {} rows ({units} unit(s), largest {largest} members; budget {})",
                sub.max_items, sub.max_subsets
            ));
            if upper > sub.max_subsets {
                let count = if upper == u64::MAX {
                    format!("more than {}", u64::MAX)
                } else {
                    upper.to_string()
                };
                self.diags.push(
                    Diagnostic::error(
                        "M315",
                        format!("tier `{tier}` would enumerate {count} subsets of up to {} rows ({units} unit(s), the largest with {largest} members), more than `max_subsets` {}", sub.max_items, sub.max_subsets),
                    )
                    .label(sub.span, "the run stopped before enumerating them")
                    .help(help),
                );
                return Err(Stop);
            }
            return self.run_ops(&sub.levels, span);
        };
        let (mut examined, mut kept) = (0u64, 0u64);
        for (k, (fanout, level)) in pruning.fanout.iter().zip(&sub.levels).enumerate() {
            let k = k + 1;
            let q = format!(
                "SELECT CAST(coalesce(fanout, 0) AS BIGINT) FROM ({}) AS __magi_f",
                sql::render_query(&lower::query(fanout))
            );
            let f = self.scalar_i64(&q)?;
            examined = examined.saturating_add(f.max(0) as u64);
            if examined > sub.max_subsets {
                self.diags.push(
                    Diagnostic::error(
                        "M315",
                        format!("tier `{tier}` examined {examined} subsets of up to {k} rows ({units} unit(s), the largest with {largest} members), more than `max_subsets` {}", sub.max_subsets),
                    )
                    .label(sub.span, format!("the run stopped while enumerating subsets of {k} rows"))
                    .help(help),
                );
                return Err(Stop);
            }
            self.run_ops(std::slice::from_ref(level), span)?;
            if let Op::Create { table, .. } = level {
                let n = self.count(table)?;
                kept = kept.saturating_add(n.max(0) as u64);
            }
        }
        (self.log)(&format!(
            "  tier {index} {tier}: {examined} subsets examined, {kept} kept, of up to {} rows ({units} unit(s), largest {largest} members; budget {})",
            sub.max_items, sub.max_subsets
        ));
        Ok(())
    }

    fn reconcile(&mut self, i: usize) -> StepResult {
        let rc = &self.hir.reconciles[i];
        (self.log)(&format!(
            "Reconcile {} = {} with {}",
            rc.name, rc.a.relation, rc.b.relation
        ));
        let program = crate::reconcile::lower::lower(rc);
        let span = rc.span;
        self.run_ops(&program.setup, span)?;
        for chk in &program.identity_checks {
            let q = sql::render_query(&lower::query(&chk.plan));
            let (rows, ids, nulls): (i64, i64, Option<i64>) = self
                .conn
                .query_row(&q, [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map_err(|e| {
                    self.diags.push(plain(format!(
                        "identity check failed to run: {}",
                        redact(&e)
                    )));
                    Stop
                })?;
            let nulls = nulls.unwrap_or(0);
            if rows != ids || nulls > 0 {
                self.diags.push(
                    Diagnostic::error("M310", format!("identity `{}` of side `{}` does not identify rows ({} rows, {ids} distinct identities, {nulls} null)", chk.columns.join(", "), chk.side, rows))
                        .label(span, "reconciliation needs a unique, non-null identity per row")
                        .help("choose identity columns that are unique, or remove the identity to use synthetic row numbers"),
                );
                return Err(Stop);
            }
        }
        for tier in &program.tiers {
            self.run_ops(&tier.prepare, span)?;
            let mut rounds = 0;
            match &tier.mode {
                TierMode::Single(ops) => self.run_ops(ops, span)?,
                TierMode::Subset(sub) => {
                    self.subset_enumerate(sub, tier.index, &tier.name, span)?;
                    self.run_ops(&sub.ops, span)?;
                }
                TierMode::Rounds {
                    round,
                    progress,
                    commit,
                    tie,
                    tied,
                    tie_commit,
                } => loop {
                    rounds += 1;
                    self.run_ops(round, span)?;
                    if self.count(progress)? > 0 {
                        self.run_ops(commit, span)?;
                        continue;
                    }
                    self.run_ops(tie, span)?;
                    if self.count(tied)? > 0 {
                        self.run_ops(tie_commit, span)?;
                        continue;
                    }
                    break;
                },
            }
            let links = format!("__magi_rc_{}_links", rc.name);
            let matched = self.scalar_i64(&format!(
                "SELECT count(*) FROM {} WHERE kind = 'match' AND tier_index = {}",
                sql::ident(&links),
                tier.index
            ))?;
            let amb = self.scalar_i64(&format!("SELECT count(DISTINCT amb_group) FROM {} WHERE kind = 'ambiguous' AND tier_index = {}", sql::ident(&links), tier.index))?;
            let rounds = if rounds > 0 {
                format!(", {rounds} rounds")
            } else {
                String::new()
            };
            (self.log)(&format!(
                "  tier {} {}: {matched} matched links, {amb} ambiguities{rounds}",
                tier.index, tier.name
            ));
        }
        self.run_ops(&program.outputs, span)?;
        for part in crate::reconcile::model::PARTS {
            self.verify(&format!("{}.{part}", rc.name))?;
        }
        {
            let inv = &program.invariant;
            let q = sql::render_query(&lower::query(inv));
            let ok: bool = self.conn.query_row(&q, [], |r| r.get(0)).unwrap_or(false);
            if !ok {
                self.diags.push(plain(format!("internal error: reconciliation `{}` does not account for every row exactly once", rc.name)).label(span, "please report this"));
                return Err(Stop);
            }
        }
        for t in &program.scratch {
            let _ = self.conn.execute_batch(&sql::render(&sql::Stmt::Drop {
                name: t.clone(),
                view: false,
            }));
        }
        let summary = format!("{}.summary", rc.name);
        if let Ok(mut stmt) = self.conn.prepare(&format!(
            "SELECT step, pairs, a_rows, b_rows FROM {} ORDER BY position",
            sql::ident(&summary)
        )) && let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        }) {
            (self.log)(&format!(
                "  {:<20} {:>8} {:>8} {:>8}",
                "step", "links", "a rows", "b rows"
            ));
            for (step, pairs, a, b) in rows.flatten() {
                (self.log)(&format!("  {step:<20} {pairs:>8} {a:>8} {b:>8}"));
            }
        }
        Ok(())
    }

    fn export(&mut self, i: usize) -> StepResult {
        let e = &self.hir.exports[i];
        match crate::export::write(&self.conn, self.hir, e) {
            Ok(written) => {
                self.staged.push((written.staged, i));
                for (rel, n) in written.parts {
                    (self.log)(&format!("Export {rel} -> {} ({n} rows)", e.display_path));
                }
                for (sheet, col, n) in written.as_text {
                    self.diags.push(
                        Diagnostic::warning(
                            "M506",
                            format!("`{}`: {n} value(s) in column `{col}` of sheet `{sheet}` cannot be stored exactly as an Excel number and were written as text", e.display_path),
                        )
                        .label(e.span, "this export")
                        .help("Excel numbers hold 15 significant digits; export to .csv, .parquet or .duckdb to keep them as numbers"),
                    );
                }
                Ok(())
            }
            Err(msg) => {
                self.diags.push(
                    Diagnostic::error(
                        "M505",
                        format!("cannot write `{}`: {}", e.display_path, redact(&msg)),
                    )
                    .label(e.span, "this export"),
                );
                Err(Stop)
            }
        }
    }
}

pub fn render_op(op: &Op) -> String {
    match op {
        Op::Create { table, plan } => sql::render(&sql::Stmt::CreateTable {
            name: table.clone(),
            query: lower::query(plan),
            temp: true,
        }),
        Op::CreateEmpty { table, columns } => sql::render(&sql::Stmt::CreateEmptyTable {
            name: table.clone(),
            columns: columns.clone(),
        }),
        Op::Insert { table, plan } => sql::render(&sql::Stmt::Insert {
            table: table.clone(),
            query: lower::query(plan),
        }),
    }
}

/// Generated SQL of a reconciliation, with the round loop spelled out in comments.
pub fn reconcile_sql(p: &ReconcileProgram) -> String {
    let mut out = String::new();
    let emit = |ops: &[Op], out: &mut String| {
        for op in ops {
            out.push_str(&render_op(op));
            out.push_str(";\n\n");
        }
    };
    out.push_str(&format!("-- reconcile {}: setup\n", p.name));
    emit(&p.setup, &mut out);
    for chk in &p.identity_checks {
        out.push_str(&format!(
            "-- identity check for side {} (must return rows = ids and null_ids = 0)\n",
            chk.side
        ));
        out.push_str(&sql::render_query(&lower::query(&chk.plan)));
        out.push_str(";\n\n");
    }
    for t in &p.tiers {
        out.push_str(&format!(
            "-- tier {} {}\n-- {}\n",
            t.index, t.name, t.description
        ));
        emit(&t.prepare, &mut out);
        match &t.mode {
            TierMode::Single(ops) => emit(ops, &mut out),
            TierMode::Subset(sub) => match &sub.pruning {
                None => {
                    out.push_str(&format!(
                        "-- members per unit (n, units): the run stops when the sum over units of C(n, 1) + ... + C(n, {}) exceeds {}\n",
                        sub.max_items, sub.max_subsets
                    ));
                    out.push_str(&sql::render_query(&lower::query(&sub.budget)));
                    out.push_str(";\n\n");
                    emit(&sub.levels, &mut out);
                    emit(&sub.ops, &mut out);
                }
                Some(pruning) => {
                    out.push_str(&format!(
                        "-- subsets are extended only while `{}` can still hold; before each level the run adds up the subsets it examines and stops past {}\n",
                        pruning.require, sub.max_subsets
                    ));
                    for (k, (fanout, level)) in pruning.fanout.iter().zip(&sub.levels).enumerate() {
                        out.push_str(&format!("-- subsets of {} rows: examined\n", k + 1));
                        out.push_str(&sql::render_query(&lower::query(fanout)));
                        out.push_str(";\n\n");
                        emit(std::slice::from_ref(level), &mut out);
                    }
                    emit(&sub.ops, &mut out);
                }
            },
            TierMode::Rounds {
                round,
                progress,
                commit,
                tie,
                tied,
                tie_commit,
            } => {
                out.push_str("-- repeat:\n");
                emit(round, &mut out);
                out.push_str(&format!(
                    "-- if {} has rows: commit and repeat\n",
                    sql::ident(progress)
                ));
                emit(commit, &mut out);
                out.push_str("-- otherwise look for tied best candidates:\n");
                emit(tie, &mut out);
                out.push_str(&format!("-- if {} has rows: record ambiguities and repeat; otherwise the tier is done\n", sql::ident(tied)));
                emit(tie_commit, &mut out);
            }
        }
    }
    out.push_str("-- outputs\n");
    emit(&p.outputs, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::redact;

    fn duckdb_error(sql: &str) -> String {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let err = conn
            .execute_batch(sql)
            .expect_err("the statement must fail");
        redact(&err)
    }

    #[test]
    fn value_errors_withhold_values_containing_quotes() {
        for (sql, secret) in [
            (r#"SELECT strptime('ab"cd-SECRET', '%Y')"#, "SECRET"),
            (r#"SELECT json_extract('ab"cd-SECRET', '$.a')"#, "SECRET"),
            (
                "SELECT json_extract('O''Brien-TOPSECRET', '$.a')",
                "TOPSECRET",
            ),
            ("SELECT CAST('O''Brien-TOPSECRET' AS INTEGER)", "TOPSECRET"),
            (
                "SELECT 9223372036854775807::BIGINT + 1",
                "9223372036854775807",
            ),
        ] {
            let text = duckdb_error(sql);
            assert!(!text.contains(secret), "{sql}: {text}");
            assert!(!text.contains("Brien"), "{sql}: {text}");
            assert!(
                text.ends_with("(details withheld: they quote data)"),
                "{sql}: {text}"
            );
        }
        // the error class and DuckDB's own words before the value stay
        let text = duckdb_error(r#"SELECT strptime('ab"cd-SECRET', '%Y')"#);
        assert!(
            text.starts_with("Invalid Input Error: Could not parse string"),
            "{text}"
        );
        let text = duckdb_error("SELECT 9223372036854775807::BIGINT + 1");
        assert!(text.contains("INT64"), "{text}");
    }

    #[test]
    fn program_errors_keep_the_program_text() {
        let text = duckdb_error("SELECT lpad('ab', 5::BIGINT, 'x')");
        assert!(text.contains("lpad(VARCHAR, INTEGER, VARCHAR)"), "{text}");
        assert!(text.contains("lpad('ab', 5::BIGINT, 'x')"), "{text}");
        let text = duckdb_error(r#"SELECT "no_such_column""#);
        assert!(text.contains("\"no_such_column\""), "{text}");
    }
}
