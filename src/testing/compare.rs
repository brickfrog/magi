//! Running a prepared test and comparing its relations with what the test expects.
//!
//! Rows are compared as multisets on the expected file's columns, as text: each relation value
//! is cast to `VARCHAR`, which is how DuckDB's CSV writer (every `.csv` export) renders it, and
//! null is an empty field. So a test's expected file can be an export with columns and rows
//! deleted.

use crate::backend::run::{self, RunOptions};
use crate::backend::sql;
use crate::diagnostic::{Diagnostic, Severity};
use crate::plan::physical;
use crate::semantic::hir::Hir;

use super::{Expect, ExpectedFile, Prepared};

/// Rows listed per difference in a failure report.
const SHOWN: usize = 10;

/// Codes of failed checks, which the verdict reports itself.
const CHECK_CODES: [&str; 3] = ["M402", "M403", "M404"];

/// Name of the table an expected file is loaded into.
const EXPECTED: &str = "__magi_expected";

pub struct Verdict {
    /// Why the test failed, one report each (its first line, then indented rows); empty when
    /// the test passed.
    pub failures: Vec<String>,
    /// What the run reported besides checks (a stop, rejected values, ...).
    pub diagnostics: Vec<Diagnostic>,
}

/// Run `p`'s program like `magi run --keep-going` (a failed `require` does not stop it) and
/// check the test's expectations against the relations it built. A failed `require` or
/// `expect` fails the test unless the test expects that validation's `.checks` or `.failures`.
pub fn run(p: &mut Prepared) -> Verdict {
    let plan = physical::build(&p.hir);
    let (outcome, conn) = run::execute_keep(
        &p.hir,
        &plan,
        &mut p.reader,
        &RunOptions { keep_going: true },
        &mut |_| {},
    );
    let diagnostics: Vec<Diagnostic> = outcome
        .diagnostics
        .into_iter()
        .filter(|d| !CHECK_CODES.contains(&d.code))
        .collect();
    let mut failures = Vec::new();
    let conn = match conn {
        Some(conn) if outcome.completed => conn,
        _ => {
            let stops: Vec<&Diagnostic> = diagnostics
                .iter()
                .filter(|d| d.severity == Severity::Error)
                .collect();
            if stops.is_empty() {
                failures.push("the run stopped".to_string());
            }
            for d in stops {
                failures.push(format!("the run stopped: {}", d.message));
            }
            return Verdict {
                failures,
                diagnostics,
            };
        }
    };
    for e in &p.expects {
        match compare(&conn, &p.hir, e) {
            Ok(Some(report)) => failures.push(report),
            Ok(None) => {}
            Err(err) => failures.push(format!("cannot compare {}: {err}", e.relation)),
        }
    }
    let exempt = |target: &str| {
        p.expects.iter().any(|e| {
            e.relation
                .strip_prefix(target)
                .is_some_and(|part| part == ".checks" || part == ".failures")
        })
    };
    for v in p.hir.validations.iter().filter(|v| !exempt(&v.target)) {
        let mut failed = match failed_checks(&conn, &v.target) {
            Ok(f) => f,
            Err(err) => {
                failures.push(format!("cannot read {}.checks: {err}", v.target));
                continue;
            }
        };
        // in declaration order; labels need not be unique, so each row is used once
        for c in &v.checks {
            let sev = c.severity.keyword();
            let Some(k) = failed
                .iter()
                .position(|(label, s, _)| *label == c.label && s == sev)
            else {
                continue;
            };
            let (label, _, rows) = failed.remove(k);
            let rows = rows.map_or(String::new(), |n| format!(" ({n} {})", plural(n, "row")));
            failures.push(format!("check `{label}` on {} failed{rows}", v.target));
        }
    }
    Verdict {
        failures,
        diagnostics,
    }
}

type Res<T> = Result<T, String>;

/// DuckDB errors without the data values they may quote.
fn db<T>(r: Result<T, duckdb::Error>) -> Res<T> {
    r.map_err(|e| run::redact(&e))
}

/// `(check, severity, failing rows)` of the failed `require` and `expect` checks.
fn failed_checks(
    conn: &duckdb::Connection,
    target: &str,
) -> Res<Vec<(String, String, Option<i64>)>> {
    let q = format!(
        "SELECT \"check\", severity, failing_rows FROM {} WHERE status = 'fail' AND severity IN ('require', 'expect')",
        sql::ident(&format!("{target}.checks"))
    );
    let mut stmt = db(conn.prepare(&q))?;
    let rows = db(stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))))?;
    db(rows.collect())
}

/// The report of an expectation that does not hold, or `None`.
fn compare(conn: &duckdb::Connection, hir: &Hir, e: &Expect) -> Res<Option<String>> {
    let rel = sql::ident(&e.relation);
    let Some(file) = &e.file else {
        let n: i64 = db(conn.query_row(&format!("SELECT count(*) FROM {rel}"), [], |r| r.get(0)))?;
        if n == 0 {
            return Ok(None);
        }
        let columns: Vec<String> = hir
            .relation(&e.relation)
            .map(|r| r.columns.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default();
        let mut report = format!("{} is not empty: {n} {}", e.relation, plural(n, "row"));
        list(
            conn,
            &mut report,
            "row: ",
            &rendered(&rel, &columns),
            &columns,
            n,
        )?;
        return Ok(Some(report));
    };
    load_expected(conn, file)?;
    let result = differences(conn, &rel, file, &e.relation);
    db(conn.execute_batch(&format!("DROP TABLE {}", sql::ident(EXPECTED))))?;
    result
}

/// `SELECT` of `columns` of `rel`, rendered as text.
fn rendered(rel: &str, columns: &[String]) -> String {
    let items = columns
        .iter()
        .map(|c| {
            let c = sql::ident(c);
            format!("CAST({c} AS VARCHAR) AS {c}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("SELECT {items} FROM {rel}")
}

fn load_expected(conn: &duckdb::Connection, file: &ExpectedFile) -> Res<()> {
    let columns = file
        .columns
        .iter()
        .map(|c| format!("{} VARCHAR", sql::ident(c)))
        .collect::<Vec<_>>()
        .join(", ");
    db(conn.execute_batch(&format!(
        "CREATE OR REPLACE TABLE {} ({columns})",
        sql::ident(EXPECTED)
    )))?;
    let mut appender = db(conn.appender(EXPECTED))?;
    for row in &file.rows {
        db(appender.append_row(duckdb::appender_params_from_iter(row)))?;
    }
    db(appender.flush())
}

fn differences(
    conn: &duckdb::Connection,
    rel: &str,
    file: &ExpectedFile,
    relation: &str,
) -> Res<Option<String>> {
    let actual = rendered(rel, &file.columns);
    let expected = format!("SELECT * FROM {}", sql::ident(EXPECTED));
    let missing = format!("{expected} EXCEPT ALL {actual}");
    let unexpected = format!("{actual} EXCEPT ALL {expected}");
    let count = |q: &str| -> Res<i64> {
        db(conn.query_row(&format!("SELECT count(*) FROM ({q})"), [], |r| r.get(0)))
    };
    let (m, u) = (count(&missing)?, count(&unexpected)?);
    if m == 0 && u == 0 {
        return Ok(None);
    }
    let mut counts = Vec::new();
    if m > 0 {
        counts.push(format!("{m} missing {}", plural(m, "row")));
    }
    if u > 0 {
        counts.push(format!("{u} unexpected {}", plural(u, "row")));
    }
    let mut report = format!(
        "{relation} differs from {} (on {}): {}",
        file.display,
        file.columns.join(", "),
        counts.join(", ")
    );
    if m > 0 {
        list(
            conn,
            &mut report,
            "missing:    ",
            &missing,
            &file.columns,
            m,
        )?;
    }
    if u > 0 {
        list(
            conn,
            &mut report,
            "unexpected: ",
            &unexpected,
            &file.columns,
            u,
        )?;
    }
    Ok(Some(report))
}

/// Append up to [`SHOWN`] of the `total` rows of `query` to `report`, sorted by their values:
/// one indented line each, `prefix` then `col=value, ...` with each value written as in a CSV
/// file (null is empty; `""` is the empty string; a value with a comma, quote, line break or
/// surrounding spaces is quoted).
fn list(
    conn: &duckdb::Connection,
    report: &mut String,
    prefix: &str,
    query: &str,
    columns: &[String],
    total: i64,
) -> Res<()> {
    let order = (1..=columns.len())
        .map(|i| format!("{i} NULLS FIRST"))
        .collect::<Vec<_>>()
        .join(", ");
    let q = format!("SELECT * FROM ({query}) ORDER BY {order} LIMIT {SHOWN}");
    let mut stmt = db(conn.prepare(&q))?;
    let rows = db(stmt.query_map([], |r| {
        (0..columns.len())
            .map(|i| r.get::<_, Option<String>>(i))
            .collect::<Result<Vec<_>, _>>()
    }))?;
    for row in rows {
        let fields = columns
            .iter()
            .zip(db(row)?)
            .map(|(c, v)| format!("{c}={}", v.as_deref().map_or(String::new(), csv_field)))
            .collect::<Vec<_>>()
            .join(", ");
        report.push_str(&format!("\n  {prefix}{fields}"));
    }
    let more = total - SHOWN as i64;
    if more > 0 {
        report.push_str(&format!("\n  ... and {more} more"));
    }
    Ok(())
}

fn csv_field(v: &str) -> String {
    let quote = v.is_empty() || v.trim() != v || v.contains([',', '"', '\n', '\r']);
    if quote {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v.to_string()
    }
}

fn plural(n: i64, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}
