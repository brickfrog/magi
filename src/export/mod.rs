//! Output sinks. Every export reads its relation in a total, reproducible order: the relation's
//! declared sort keys, then every column. Files are written to unique temporary names and moved
//! into place together by [`commit`], so a failed run never changes or half-writes an output.

pub mod excel;
pub mod tmdl;

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::backend::sql::{self, Expr, OrderBy, Query, Select, Stmt, TableRef, func, str_lit};
use crate::semantic::hir::{Export, ExportFormat, Hir};
use crate::semantic::types::Type;
use excel::{Cell, Sheet};

/// `SELECT * FROM rel ORDER BY <sort keys>, <every column>`.
pub fn ordered_query(hir: &Hir, relation: &str, columns: &[String]) -> Query {
    let mut order: Vec<OrderBy> = Vec::new();
    if let Some(rel) = hir.relation(relation) {
        for (k, desc) in &rel.sort {
            if columns.contains(k) {
                order.push(OrderBy {
                    expr: sql::col(k),
                    desc: *desc,
                });
            }
        }
    }
    for c in columns {
        if !order.iter().any(|o| o.expr == sql::col(c)) {
            order.push(OrderBy {
                expr: sql::col(c),
                desc: false,
            });
        }
    }
    Query::select(Select {
        items: vec![(Expr::Star { table: None }, None)],
        from: Some((TableRef::Named(relation.into()), None)),
        order_by: order,
        ..Select::default()
    })
}

pub fn describe(
    conn: &duckdb::Connection,
    relation: &str,
) -> Result<Vec<(String, String)>, String> {
    let mut stmt = conn
        .prepare(&format!("DESCRIBE {}", sql::ident(relation)))
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// A fresh, hidden name next to `path` (same directory, so renames stay atomic) that no file
/// has: staging never overwrites or deletes a file it did not create.
fn unique_sibling(path: &Path, tag: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = path.file_name().unwrap_or_default();
    loop {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let mut candidate = OsString::from(".");
        candidate.push(name);
        candidate.push(format!(".magi-{tag}-{}-{n}", std::process::id()));
        let p = path.with_file_name(candidate);
        let mut wal = p.clone().into_os_string();
        wal.push(".wal");
        if std::fs::symlink_metadata(&p).is_err() && std::fs::symlink_metadata(&wal).is_err() {
            return p;
        }
    }
}

/// A key that is equal for two paths naming the same file: the longest existing prefix is
/// canonicalised (resolving symlinks, `.` and `..`), the rest is normalised lexically.
pub fn file_identity(path: &Path) -> PathBuf {
    let absolute;
    let path = if path.is_relative()
        && let Ok(cwd) = std::env::current_dir()
    {
        absolute = cwd.join(path);
        &absolute
    } else {
        path
    };
    let parts: Vec<Component> = path.components().collect();
    for k in (1..=parts.len()).rev() {
        let prefix: PathBuf = parts[..k].iter().collect();
        if let Ok(real) = std::fs::canonicalize(&prefix) {
            return lexical(real, &parts[k..]);
        }
    }
    lexical(PathBuf::new(), &parts)
}

fn lexical(mut base: PathBuf, rest: &[Component]) -> PathBuf {
    for c in rest {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(base.components().next_back(), Some(Component::Normal(_))) {
                    base.pop();
                } else if !base.has_root() {
                    base.push("..");
                }
            }
            other => base.push(other),
        }
    }
    base
}

fn count(conn: &duckdb::Connection, relation: &str) -> Result<u64, String> {
    conn.query_row(
        &format!("SELECT count(*) FROM {}", sql::ident(relation)),
        [],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n as u64)
    .map_err(|e| e.to_string())
}

/// A written export waiting in its temp file; [`commit`] moves it into place.
pub struct Staged {
    tmp: PathBuf,
    path: PathBuf,
}

impl Staged {
    pub fn discard(self) {
        remove_temp(&self.tmp);
    }
}

fn remove_temp(tmp: &Path) {
    let _ = std::fs::remove_file(tmp);
    // DuckDB may leave a write-ahead log next to a database file
    let mut wal = tmp.to_path_buf().into_os_string();
    wal.push(".wal");
    let _ = std::fs::remove_file(wal);
}

/// Move every staged export into place, or none: existing targets are first moved aside to
/// unique backups, then the temp files are renamed onto the targets. On any failure the new
/// files are removed and the backups restored; on success the backups are deleted. The error
/// names the index of the export that could not be written.
pub fn commit(staged: Vec<Staged>) -> Result<(), (usize, String)> {
    let mut backups: Vec<Option<PathBuf>> = Vec::with_capacity(staged.len());
    let mut failure = None;
    for (i, s) in staged.iter().enumerate() {
        match std::fs::symlink_metadata(&s.path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => backups.push(None),
            Err(e) => failure = Some((i, e.to_string())),
            Ok(m) if m.is_dir() => failure = Some((i, "a directory is in the way".into())),
            Ok(_) => {
                let backup = unique_sibling(&s.path, "old");
                match std::fs::rename(&s.path, &backup) {
                    Ok(()) => backups.push(Some(backup)),
                    Err(e) => {
                        failure = Some((i, format!("cannot move the existing file aside: {e}")))
                    }
                }
            }
        }
        if failure.is_some() {
            break;
        }
    }
    let mut placed = 0;
    if failure.is_none() {
        for (i, s) in staged.iter().enumerate() {
            if let Err(e) = std::fs::rename(&s.tmp, &s.path) {
                failure = Some((i, e.to_string()));
                break;
            }
            placed += 1;
        }
    }
    let Some((i, mut msg)) = failure else {
        for b in backups.into_iter().flatten() {
            let _ = std::fs::remove_file(b);
        }
        for s in staged {
            remove_temp(&s.tmp);
        }
        return Ok(());
    };
    for s in &staged[..placed] {
        let _ = std::fs::remove_file(&s.path);
    }
    for (s, b) in staged.iter().zip(&backups) {
        if let Some(b) = b
            && std::fs::rename(b, &s.path).is_err()
        {
            msg.push_str(&format!(
                "; the previous {} could not be restored and is kept as {}",
                s.path.display(),
                b.display()
            ));
        }
    }
    for s in staged {
        s.discard();
    }
    Err((i, msg))
}

/// What [`write`] produced: the staged file, `(relation, rows)` per written part, and
/// `(sheet, column, count)` for XLSX numbers written as text because an Excel number (an IEEE
/// double) cannot hold them exactly.
pub struct Written {
    pub staged: Staged,
    pub parts: Vec<(String, u64)>,
    pub as_text: Vec<(String, String, u64)>,
}

/// Write one export to a unique temp file next to its target. The target is replaced only by
/// [`commit`], so a failed run changes no output.
pub fn write(conn: &duckdb::Connection, hir: &Hir, export: &Export) -> Result<Written, String> {
    let path = &export.path;
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let tmp = unique_sibling(path, "tmp");
    let mut written = Vec::new();
    let mut as_text = Vec::new();
    let result = (|| -> Result<(), String> {
        match export.format {
            ExportFormat::Csv | ExportFormat::Parquet | ExportFormat::Json => {
                let part = &export.parts[0];
                let columns: Vec<String> = describe(conn, &part.relation)?
                    .into_iter()
                    .map(|(n, _)| n)
                    .collect();
                let options: Vec<(String, Expr)> = match export.format {
                    ExportFormat::Csv => vec![
                        ("FORMAT".into(), sql::col("csv")),
                        ("HEADER".into(), Expr::Lit(sql::Lit::Bool(true))),
                    ],
                    ExportFormat::Parquet => vec![("FORMAT".into(), sql::col("parquet"))],
                    _ => {
                        let array = path
                            .extension()
                            .is_some_and(|e| e.eq_ignore_ascii_case("json"));
                        vec![
                            ("FORMAT".into(), sql::col("json")),
                            ("ARRAY".into(), Expr::Lit(sql::Lit::Bool(array))),
                        ]
                    }
                };
                let stmt = Stmt::Copy {
                    query: ordered_query(hir, &part.relation, &columns),
                    path: tmp.display().to_string(),
                    options,
                };
                conn.execute_batch(&sql::render(&stmt))
                    .map_err(|e| e.to_string())?;
                written.push((part.relation.clone(), count(conn, &part.relation)?));
            }
            ExportFormat::DuckDb => {
                let alias = "__magi_out";
                conn.execute_batch(&sql::render(&Stmt::Attach {
                    path: tmp.display().to_string(),
                    alias: alias.into(),
                    read_only: false,
                }))
                .map_err(|e| e.to_string())?;
                let r = (|| {
                    for part in &export.parts {
                        let columns: Vec<String> = describe(conn, &part.relation)?
                            .into_iter()
                            .map(|(n, _)| n)
                            .collect();
                        let q = sql::render_query(&ordered_query(hir, &part.relation, &columns));
                        conn.execute_batch(&format!(
                            "CREATE TABLE {}.{} AS {q}",
                            sql::ident(alias),
                            sql::ident(&part.name)
                        ))
                        .map_err(|e| e.to_string())?;
                        written.push((part.relation.clone(), count(conn, &part.relation)?));
                    }
                    Ok::<(), String>(())
                })();
                let _ = conn.execute_batch(&sql::render(&Stmt::Detach {
                    alias: alias.into(),
                }));
                r?;
            }
            ExportFormat::Xlsx => {
                let mut sheets = Vec::new();
                for part in &export.parts {
                    let (sheet, text) = read_sheet(conn, hir, &part.relation, &part.name)?;
                    written.push((part.relation.clone(), sheet.rows.len() as u64));
                    as_text.extend(text.into_iter().map(|(col, n)| (part.name.clone(), col, n)));
                    sheets.push(sheet);
                }
                excel::write_xlsx(&tmp, &sheets)?;
            }
        }
        Ok(())
    })();
    let staged = Staged {
        tmp,
        path: path.clone(),
    };
    match result {
        Ok(()) => Ok(Written {
            staged,
            parts: written,
            as_text,
        }),
        Err(e) => {
            staged.discard();
            Err(e)
        }
    }
}

/// Read a relation into typed spreadsheet cells, with `(column, count)` of the numbers written
/// as text because an Excel number cannot hold them exactly.
fn read_sheet(
    conn: &duckdb::Connection,
    hir: &Hir,
    relation: &str,
    name: &str,
) -> Result<(Sheet, Vec<(String, u64)>), String> {
    let cols = describe(conn, relation)?;
    let names: Vec<String> = cols.iter().map(|(n, _)| n.clone()).collect();
    let types: Vec<Type> = cols.iter().map(|(_, t)| Type::from_duckdb(t)).collect();
    let items: Vec<(Expr, Option<String>)> = cols
        .iter()
        .zip(&types)
        .map(|((n, _), t)| {
            let c = sql::col(n);
            let e = match t {
                Type::Bool => c,
                Type::Float => Expr::Cast {
                    expr: Box::new(c),
                    ty: "DOUBLE".into(),
                    try_: false,
                },
                Type::Date => func("strftime", vec![c, str_lit("%Y-%m-%d")]),
                Type::Timestamp | Type::TimestampTz => func(
                    "strftime",
                    vec![
                        Expr::Cast {
                            expr: Box::new(c),
                            ty: "TIMESTAMP".into(),
                            try_: false,
                        },
                        str_lit("%Y-%m-%d %H:%M:%S.%f"),
                    ],
                ),
                _ => Expr::Cast {
                    expr: Box::new(c),
                    ty: "VARCHAR".into(),
                    try_: false,
                },
            };
            (e, Some(n.clone()))
        })
        .collect();
    let ordered = ordered_query(hir, relation, &names);
    // the outer projection keeps the inner order
    let q = Query::select(Select {
        items,
        from: Some((TableRef::Sub(Box::new(ordered)), Some("t".into()))),
        ..Select::default()
    });
    let mut stmt = conn
        .prepare(&sql::render_query(&q))
        .map_err(|e| e.to_string())?;
    let mut rows_out = Vec::new();
    let mut as_text = vec![0u64; types.len()];
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let mut cells = Vec::with_capacity(types.len());
        for (i, t) in types.iter().enumerate() {
            let cell = match t {
                Type::Bool => row
                    .get::<_, Option<bool>>(i)
                    .map_err(|e| e.to_string())?
                    .map(Cell::Bool),
                Type::Float => row
                    .get::<_, Option<f64>>(i)
                    .map_err(|e| e.to_string())?
                    .map(Cell::Float),
                // read as text: UBIGINT/HUGEINT and DECIMAL(38, s) exceed i64/f64
                Type::Int | Type::Decimal(..) => {
                    let text = row.get::<_, Option<String>>(i).map_err(|e| e.to_string())?;
                    text.map(|v| match number_cell(&v, *t) {
                        Some(cell) => cell,
                        None => {
                            as_text[i] += 1;
                            Cell::String(v)
                        }
                    })
                }
                Type::Date => row
                    .get::<_, Option<String>>(i)
                    .map_err(|e| e.to_string())?
                    .and_then(|v| parse_date(&v)),
                Type::Timestamp | Type::TimestampTz => row
                    .get::<_, Option<String>>(i)
                    .map_err(|e| e.to_string())?
                    .and_then(|v| parse_timestamp(&v)),
                Type::Time => row
                    .get::<_, Option<String>>(i)
                    .map_err(|e| e.to_string())?
                    .and_then(|v| parse_time(&v)),
                _ => row
                    .get::<_, Option<String>>(i)
                    .map_err(|e| e.to_string())?
                    .map(Cell::String),
            };
            cells.push(cell.unwrap_or(Cell::Null));
        }
        rows_out.push(cells);
    }
    let as_text = names
        .iter()
        .zip(as_text)
        .filter(|(_, n)| *n > 0)
        .map(|(c, n)| (c.clone(), n))
        .collect();
    Ok((
        Sheet {
            name: name.to_string(),
            columns: names,
            rows: rows_out,
        },
        as_text,
    ))
}

/// The Excel number for an int or decimal given as DuckDB text, or `None` when an IEEE double
/// cannot hold it exactly (ints beyond ±2^53, decimals with more than 15 significant digits).
fn number_cell(text: &str, ty: Type) -> Option<Cell> {
    match ty {
        Type::Decimal(_, scale) => {
            let digits: String = text.chars().filter(char::is_ascii_digit).collect();
            let significant = digits.trim_start_matches('0').trim_end_matches('0').len();
            if significant > 15 {
                return None;
            }
            let value = text.parse::<f64>().ok()?;
            Some(Cell::Decimal { value, scale })
        }
        _ => {
            let v = text.parse::<i64>().ok()?;
            (-excel::MAX_EXACT_INT..=excel::MAX_EXACT_INT)
                .contains(&v)
                .then_some(Cell::Int(v))
        }
    }
}

fn parse_date(s: &str) -> Option<Cell> {
    let mut it = s.splitn(3, '-');
    let year = it.next()?.parse().ok()?;
    let month = it.next()?.parse().ok()?;
    let day = it.next()?.parse().ok()?;
    Some(Cell::Date { year, month, day })
}

fn parse_timestamp(s: &str) -> Option<Cell> {
    let (d, t) = s.split_once(' ')?;
    let Cell::Date { year, month, day } = parse_date(d)? else {
        return None;
    };
    let Cell::Time {
        hour,
        minute,
        second,
    } = parse_time(t)?
    else {
        return None;
    };
    let micros = t
        .split_once('.')
        .and_then(|(_, f)| format!("{f:0<6}")[..6].parse().ok())
        .unwrap_or(0);
    Some(Cell::Timestamp {
        year,
        month,
        day,
        hour,
        minute,
        second,
        micros,
    })
}

fn parse_time(s: &str) -> Option<Cell> {
    let main = s.split('.').next()?;
    let mut it = main.splitn(3, ':');
    Some(Cell::Time {
        hour: it.next()?.parse().ok()?,
        minute: it.next()?.parse().ok()?,
        second: it.next()?.parse().ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// A rename failing after an earlier export was already moved into place puts every
    /// previous file back and removes all temp and backup files.
    #[test]
    fn a_failure_while_moving_into_place_restores_every_target() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, c) = (
            dir.path().join("a.csv"),
            dir.path().join("b.csv"),
            dir.path().join("c.csv"),
        );
        std::fs::write(&a, "old a").unwrap();
        std::fs::write(&b, "old b").unwrap();
        let staged: Vec<Staged> = [&a, &b, &c]
            .into_iter()
            .map(|p| Staged {
                tmp: unique_sibling(p, "tmp"),
                path: p.clone(),
            })
            .collect();
        std::fs::write(&staged[0].tmp, "new a").unwrap();
        std::fs::write(&staged[2].tmp, "new c").unwrap();
        // b's temp file is missing: its rename fails after a's succeeded
        let err = commit(staged).unwrap_err();
        assert_eq!(err.0, 1);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "old a");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "old b");
        assert_eq!(listing(dir.path()), ["a.csv", "b.csv"]);
    }

    #[test]
    fn file_identity_sees_through_dot_segments() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let id = |p: &str| file_identity(&base.join(p));
        assert_eq!(id("out/a.csv"), id("out/../out/a.csv"));
        assert_eq!(id("out/a.csv"), id("./out/./a.csv"));
        assert_ne!(id("out/a.csv"), id("out/b.csv"));
        std::fs::create_dir(base.join("out")).unwrap();
        assert_eq!(id("out/a.csv"), id("x/../out/a.csv"));
    }

    #[test]
    fn only_numbers_a_double_holds_exactly_stay_numbers() {
        let dec = Type::Decimal(38, 2);
        assert!(matches!(
            number_cell("123456789012.50", dec),
            Some(Cell::Decimal { .. })
        ));
        assert_eq!(number_cell("12345678901234567890.12", dec), None);
        assert_eq!(number_cell("18446744073709551615", Type::Int), None);
        assert_eq!(number_cell("9007199254740993", Type::Int), None);
        assert_eq!(
            number_cell("-9007199254740992", Type::Int),
            Some(Cell::Int(-9007199254740992))
        );
    }
}
