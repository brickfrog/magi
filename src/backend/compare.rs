//! Relations compared as multisets of rows (`magi test`, `magi diff`). Rows are compared as
//! text: each value is cast to `VARCHAR`, which is how DuckDB's CSV writer (every `.csv`
//! export) renders it, and null stays null. So a test's expected file can be an export, and a
//! column whose type changed (`decimal(10,2)` to `decimal(12,2)`) still compares by value.

use crate::backend::{run, sql};

pub type Res<T> = Result<T, String>;

/// DuckDB errors without the data values they may quote.
pub fn db<T>(r: Result<T, duckdb::Error>) -> Res<T> {
    r.map_err(|e| run::redact(&e))
}

/// `SELECT` of `columns` of `table` (an SQL table reference, quoted), rendered as text.
pub fn rendered(table: &str, columns: &[String]) -> String {
    let items = columns
        .iter()
        .map(|c| {
            let c = sql::ident(c);
            format!("CAST({c} AS VARCHAR) AS {c}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("SELECT {items} FROM {table}")
}

/// The rows of query `left` that query `right` does not have: a row `right` has `k` times
/// removes `k` of its copies.
pub fn except(left: &str, right: &str) -> String {
    format!("{left} EXCEPT ALL {right}")
}

/// The number of rows of `query`.
pub fn count(conn: &duckdb::Connection, query: &str) -> Res<i64> {
    db(conn.query_row(&format!("SELECT count(*) FROM ({query})"), [], |r| r.get(0)))
}

/// Append up to `limit` of the `total` rows of `query` (text columns named `columns`) to
/// `report`, sorted by their values: one indented line each, `prefix` then `col=value, ...`
/// with each value written as in a CSV file (null is empty; `""` is the empty string; a value
/// with a comma, quote, line break or surrounding spaces is quoted), then how many more there
/// are.
pub fn list(
    conn: &duckdb::Connection,
    report: &mut String,
    prefix: &str,
    query: &str,
    columns: &[String],
    total: i64,
    limit: usize,
) -> Res<()> {
    let order = (1..=columns.len())
        .map(|i| format!("{i} NULLS FIRST"))
        .collect::<Vec<_>>()
        .join(", ");
    let q = format!("SELECT * FROM ({query}) ORDER BY {order} LIMIT {limit}");
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
    let more = total - limit as i64;
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

pub fn plural(n: i64, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}
