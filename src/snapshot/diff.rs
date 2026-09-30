//! `magi diff`: the relations of two runs compared, each run a snapshot file or a program run
//! now (see [`super`]).
//!
//! The comparison happens in one database: a program side's run (NEW's when both sides are
//! programs; OLD's relations are then first copied to a temporary snapshot file), or a new
//! empty database, with the snapshot files attached read-only. Relations and columns are
//! matched by name ignoring case, as DuckDB does, and a relation's rows are compared as
//! multisets on the columns both sides have, as text ([`crate::backend::compare`]).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use crate::backend::compare::{Res, count, db, except, list, plural, rendered};
use crate::backend::run;
use crate::backend::sql::{self, Stmt};
use crate::diagnostic::did_you_mean;
use crate::export;
use crate::semantic::hir::Hir;

use super::{META, Meta, Run, metadata};

/// One side of a diff.
pub enum Input {
    /// A snapshot file (`magi snapshot`).
    Snapshot(PathBuf),
    /// A program's completed run (see [`super::Run`]).
    Program(Box<Run>),
}

pub struct Options {
    /// Added and removed rows listed per differing relation (at most this many of each).
    pub rows: usize,
    /// Only these relations (all when empty).
    pub relations: Vec<String>,
}

pub struct Report {
    /// The lines to print: the metadata that differs, one report per differing relation, and
    /// a summary line.
    pub text: String,
    /// A compared relation differs.
    pub differs: bool,
}

/// Where a side's relations are in the comparing database.
#[derive(Clone)]
struct Side {
    /// The catalog of its tables (quoted): `temp` for a run's relations, else the alias of an
    /// attached snapshot.
    catalog: String,
    /// Its relations: a program's in program order, a snapshot's sorted by name.
    relations: Vec<String>,
    program: bool,
    meta: Meta,
}

impl Side {
    fn table(&self, relation: &str) -> String {
        format!("{}.{}", self.catalog, sql::ident(relation))
    }
}

/// Aliases the snapshots are attached as.
const OLD: &str = "__magi_old";
const NEW: &str = "__magi_new";

pub fn diff(old: Input, new: Input, options: &Options) -> Res<Report> {
    // the temporary copy of OLD's run when both sides are programs, removed after the
    // comparing database (declared later, dropped earlier) is closed
    let mut _copy = None;
    let (conn, old, new) = match (old, new) {
        (Input::Snapshot(o), Input::Snapshot(n)) => {
            let conn = run::open(&Hir::default())?;
            let old = attach(&conn, &o, OLD)?;
            let new = if export::file_identity(&o) == export::file_identity(&n) {
                old.clone()
            } else {
                attach(&conn, &n, NEW)?
            };
            (conn, old, new)
        }
        (Input::Program(o), Input::Snapshot(n)) => {
            let old = program(&o)?;
            let new = attach(&o.conn, &n, NEW)?;
            (o.conn, old, new)
        }
        (Input::Snapshot(o), Input::Program(n)) => {
            let new = program(&n)?;
            let old = attach(&n.conn, &o, OLD)?;
            (n.conn, old, new)
        }
        (Input::Program(o), Input::Program(n)) => {
            let mut old = program(&o)?;
            let dir = tempfile::tempdir()
                .map_err(|e| format!("cannot create a temporary directory: {e}"))?;
            let path = dir.path().join("old.duckdb");
            let tables: Vec<(&str, &str)> = old
                .relations
                .iter()
                .map(|r| (r.as_str(), r.as_str()))
                .collect();
            export::write_duckdb(&o.conn, &o.hir, &path, &tables)
                .map_err(|e| format!("cannot copy OLD's relations: {e}"))?;
            drop(o);
            attach_file(&n.conn, &path, OLD)?;
            old.catalog = sql::ident(OLD);
            _copy = Some(dir);
            let new = program(&n)?;
            (n.conn, old, new)
        }
    };

    let key = |r: &str| r.to_lowercase();
    let old_names: HashMap<String, &str> =
        old.relations.iter().map(|r| (key(r), r.as_str())).collect();
    let new_names: HashMap<String, &str> =
        new.relations.iter().map(|r| (key(r), r.as_str())).collect();
    // NEW's program order, else by name; relations only in OLD after NEW's program's
    let mut keys: Vec<String> = Vec::new();
    if new.program {
        keys.extend(new.relations.iter().map(|r| key(r)));
    }
    let mut rest: Vec<String> = new
        .relations
        .iter()
        .chain(&old.relations)
        .map(|r| key(r))
        .filter(|k| !keys.contains(k))
        .collect();
    rest.sort();
    rest.dedup();
    keys.extend(rest);
    if !options.relations.is_empty() {
        for name in &options.relations {
            if !keys.contains(&key(name)) {
                let names = old
                    .relations
                    .iter()
                    .chain(&new.relations)
                    .map(String::as_str);
                let hint = did_you_mean(name, names)
                    .map_or(String::new(), |s| format!(" (did you mean `{s}`?)"));
                return Err(format!("neither side has a relation `{name}`{hint}"));
            }
        }
        keys.retain(|k| options.relations.iter().any(|r| key(r) == *k));
    }

    let mut text = String::new();
    for line in header(&old.meta, &new.meta) {
        text.push_str(&line);
        text.push('\n');
    }
    let rows_of = |side: &Side, r: &str| count(&conn, &format!("SELECT * FROM {}", side.table(r)));
    let mut differing = 0;
    for k in &keys {
        let report = match (old_names.get(k), new_names.get(k)) {
            (Some(o), Some(n)) => relation(&conn, n, &old.table(o), &new.table(n), options.rows)?,
            (Some(o), None) => {
                let rows = rows_of(&old, o)?;
                Some(format!("{o}: only in OLD ({rows} {})", plural(rows, "row")))
            }
            (None, Some(n)) => {
                let rows = rows_of(&new, n)?;
                Some(format!("{n}: only in NEW ({rows} {})", plural(rows, "row")))
            }
            (None, None) => None,
        };
        if let Some(report) = report {
            differing += 1;
            text.push_str(&report);
            text.push('\n');
        }
    }
    let total = keys.len() as i64;
    if differing == 0 {
        text.push_str(&format!(
            "no differences in {total} {}\n",
            plural(total, "relation")
        ));
    } else {
        text.push_str(&format!("{differing} of {total} relations differ\n"));
    }
    Ok(Report {
        text,
        differs: differing > 0,
    })
}

/// A program side: its run's relations, which are temporary tables and views.
fn program(run: &Run) -> Res<Side> {
    Ok(Side {
        catalog: "temp".into(),
        relations: run.hir.relations.iter().map(|r| r.name.clone()).collect(),
        program: true,
        meta: metadata(run)?,
    })
}

/// The snapshot at `path` attached to `conn` as `alias`.
fn attach(conn: &duckdb::Connection, path: &Path, alias: &str) -> Res<Side> {
    let not_snapshot = |why: &str| format!("{} is not a MAGI snapshot ({why})", path.display());
    // a DuckDB database file has `DUCK` at bytes 8..12
    let mut head = [0u8; 12];
    std::fs::File::open(path)
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut head))
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::UnexpectedEof => not_snapshot("not a DuckDB database"),
            _ => format!("cannot read {}: {e}", path.display()),
        })?;
    if &head[8..12] != b"DUCK" {
        return Err(not_snapshot("not a DuckDB database"));
    }
    attach_file(conn, path, alias)?;
    let not_snapshot = || not_snapshot(&format!("it has no `{META}` table"));
    let mut stmt = db(conn.prepare(
        "SELECT table_name FROM duckdb_tables() WHERE database_name = ? AND schema_name = 'main'",
    ))?;
    let tables = db(stmt.query_map(duckdb::params![alias], |r| r.get::<_, String>(0)))?;
    let mut relations: Vec<String> = db(tables.collect())?;
    let before = relations.len();
    relations.retain(|t| t != META);
    if relations.len() == before {
        return Err(not_snapshot());
    }
    relations.sort_by_key(|r| r.to_lowercase());
    let q = format!(
        "SELECT key, value FROM {}.{}",
        sql::ident(alias),
        sql::ident(META)
    );
    let mut stmt = db(conn.prepare(&q)).map_err(|_| not_snapshot())?;
    let rows = db(stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))))?;
    Ok(Side {
        catalog: sql::ident(alias),
        relations,
        program: false,
        meta: db(rows.collect())?,
    })
}

fn attach_file(conn: &duckdb::Connection, path: &Path, alias: &str) -> Res<()> {
    let stmt = Stmt::Attach {
        path: path.display().to_string(),
        alias: alias.into(),
        read_only: true,
    };
    conn.execute_batch(&sql::render(&stmt))
        .map_err(|e| format!("cannot open {}: {e}", path.display()))
}

/// The lines saying what differs between two sides' metadata: program files and inputs
/// changed, added or removed (compared by MD5), `today` and the MAGI version.
fn header(old: &Meta, new: &Meta) -> Vec<String> {
    let old: BTreeMap<&str, &str> = old.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let new: BTreeMap<&str, &str> = new.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut lines = Vec::new();
    for (label, prefixes) in [
        ("program files", &["program:"][..]),
        ("inputs", &["file:", "sql:"][..]),
    ] {
        let ours = |k: &&str| prefixes.iter().any(|p| k.starts_with(p));
        let name = |k: &str| match k.split_once(':') {
            Some(("sql", source)) => format!("sql source {source}"),
            Some((_, path)) => path.to_string(),
            None => k.to_string(),
        };
        let (mut changed, mut added) = (Vec::new(), Vec::new());
        for (k, v) in new.iter().filter(|(k, _)| ours(k)) {
            match old.get(k) {
                None => added.push(name(k)),
                Some(o) if o != v => changed.push(name(k)),
                Some(_) => {}
            }
        }
        let removed: Vec<String> = old
            .keys()
            .filter(|k| ours(k) && !new.contains_key(*k))
            .map(|k| name(k))
            .collect();
        let parts: Vec<String> = [("changed", changed), ("added", added), ("removed", removed)]
            .into_iter()
            .filter(|(_, names)| !names.is_empty())
            .map(|(what, names)| format!("{what} {}", names.join(", ")))
            .collect();
        if !parts.is_empty() {
            lines.push(format!("{label}: {}", parts.join("; ")));
        }
    }
    for (key, label) in [("today", "today"), ("magi_version", "magi version")] {
        let (o, n) = (old.get(key), new.get(key));
        if o != n {
            lines.push(format!(
                "{label}: {} → {}",
                o.unwrap_or(&"?"),
                n.unwrap_or(&"?")
            ));
        }
    }
    lines
}

/// The report of relation `name`, tables `old` and `new`, or `None` when they are equal: rows
/// added and removed, columns added, removed and whose type changed, and up to `rows` of the
/// added and of the removed rows.
fn relation(
    conn: &duckdb::Connection,
    name: &str,
    old: &str,
    new: &str,
    rows: usize,
) -> Res<Option<String>> {
    let (old_cols, new_cols) = (columns(conn, old)?, columns(conn, new)?);
    let same = |a: &str, b: &str| a.to_lowercase() == b.to_lowercase();
    let mut common = Vec::new();
    let (mut added_cols, mut retyped) = (Vec::new(), Vec::new());
    for (c, t) in &new_cols {
        match old_cols.iter().find(|(o, _)| same(o, c)) {
            None => added_cols.push(c.clone()),
            Some((_, old_t)) => {
                if old_t != t {
                    retyped.push(format!(
                        "{c} {} → {}",
                        old_t.to_lowercase(),
                        t.to_lowercase()
                    ));
                }
                common.push(c.clone());
            }
        }
    }
    let removed_cols: Vec<String> = old_cols
        .iter()
        .filter(|(o, _)| !new_cols.iter().any(|(c, _)| same(o, c)))
        .map(|(o, _)| o.clone())
        .collect();
    let old_n = count(conn, &format!("SELECT * FROM {old}"))?;
    let new_n = count(conn, &format!("SELECT * FROM {new}"))?;
    let (old_q, new_q) = (rendered(old, &common), rendered(new, &common));
    let (added_q, removed_q) = (except(&new_q, &old_q), except(&old_q, &new_q));
    // without common columns every row is the same empty row
    let (added, removed) = if common.is_empty() {
        ((new_n - old_n).max(0), (old_n - new_n).max(0))
    } else {
        (count(conn, &added_q)?, count(conn, &removed_q)?)
    };
    let columns_changed = !(added_cols.is_empty() && removed_cols.is_empty() && retyped.is_empty());
    if added == 0 && removed == 0 && !columns_changed {
        return Ok(None);
    }
    let mut report = if added > 0 || removed > 0 {
        format!(
            "{name}: {added} {} added, {removed} removed ({old_n} → {new_n} rows)",
            plural(added, "row")
        )
    } else {
        format!(
            "{name}: no rows added or removed ({new_n} {})",
            plural(new_n, "row")
        )
    };
    if columns_changed {
        let changes: Vec<String> = [
            ("added", added_cols),
            ("removed", removed_cols),
            ("type changed", retyped),
        ]
        .into_iter()
        .filter(|(_, cols)| !cols.is_empty())
        .map(|(what, cols)| format!("{what}: {}", cols.join(", ")))
        .collect();
        report.push_str(&format!("\n  columns {}", changes.join("; ")));
    }
    if rows > 0 && !common.is_empty() {
        if added > 0 {
            list(
                conn,
                &mut report,
                "added:   ",
                &added_q,
                &common,
                added,
                rows,
            )?;
        }
        if removed > 0 {
            list(
                conn,
                &mut report,
                "removed: ",
                &removed_q,
                &common,
                removed,
                rows,
            )?;
        }
    }
    Ok(Some(report))
}

/// `(name, DuckDB type)` of the columns of `table`.
fn columns(conn: &duckdb::Connection, table: &str) -> Res<Vec<(String, String)>> {
    let mut stmt = db(conn.prepare(&format!("DESCRIBE SELECT * FROM {table}")))?;
    let rows = db(stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))))?;
    db(rows.collect())
}
