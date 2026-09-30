//! Snapshots (`magi snapshot`): every relation of a run saved in one DuckDB database file, and
//! their comparison ([`diff`], `magi diff`).
//!
//! A snapshot's run is the program without outputs ([`Loaded::without_outputs`]: every
//! relation is computed and no file is written) run like `magi run --keep-going`. The database
//! has one table per relation, named exactly by the relation (`bank`, `bank.rejects`,
//! `rec.matches`, `v.checks`, ...) with the relation's column types and its rows in export
//! order, and the table [`META`] of `(key, value)` text rows saying what produced it:
//!
//! - `magi_version`: the version of MAGI;
//! - `created`: when the snapshot was taken (UTC, `2026-09-30T08:15:00Z`);
//! - `today`: the date `today()` returned;
//! - `program:<path>`: the MD5 of the text of each program file (the entry file, its imports);
//! - `file:<path>`: `<md5> <bytes>` of each file a source read (data files, fixed-width layouts,
//!   the database of a `duckdb` connection);
//! - `sql:<source>`: `<connection> <md5 of the query>` of each `sql(...)` source (no
//!   credentials).
//!
//! Paths are relative to the entry file's directory, `/`-separated.

pub mod diff;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::backend::compare::{Res, db};
use crate::backend::sql;
use crate::export;
use crate::semantic::hir::{ConnectionKind, Hir, SourceKind};
use crate::semantic::load::Loaded;

/// The table of a snapshot's metadata.
pub const META: &str = "__magi_snapshot";

/// A snapshot's metadata: `(key, value)` rows (see the module documentation).
pub type Meta = Vec<(String, String)>;

/// A completed run of a program without outputs.
pub struct Run {
    /// The program's entry file.
    pub file: PathBuf,
    pub loaded: Loaded,
    pub hir: Hir,
    /// The database holding every relation the run built.
    pub conn: duckdb::Connection,
}

/// The metadata of `run`: the MD5s are of the files as they are now, which the run has just
/// read.
pub fn metadata(run: &Run) -> Res<Meta> {
    let conn = &run.conn;
    let base = run.file.parent().unwrap_or(Path::new(""));
    // the run's session time zone is UTC
    let created: String = db(conn.query_row(
        "SELECT strftime(now(), '%Y-%m-%dT%H:%M:%SZ')",
        [],
        |r| r.get(0),
    ))?;
    let mut meta: Meta = vec![
        ("magi_version".into(), env!("CARGO_PKG_VERSION").into()),
        ("created".into(), created),
        ("today".into(), run.hir.today.clone()),
    ];
    for f in &run.loaded.sources.files {
        meta.push((
            format!("program:{}", relative(&f.path, base)),
            md5(conn, &f.text)?,
        ));
    }
    let mut files: Vec<&Path> = Vec::new();
    for s in &run.hir.sources {
        match &s.kind {
            SourceKind::Csv { path, .. }
            | SourceKind::Parquet { path }
            | SourceKind::Excel { path, .. }
            | SourceKind::DuckDb { path, .. } => files.push(path),
            SourceKind::FixedWidth { path, options } => {
                files.extend([path.as_path(), options.layout.as_path()])
            }
            SourceKind::Sql { connection, query } => {
                meta.push((
                    format!("sql:{}", s.name),
                    format!("{connection} {}", md5(conn, query)?),
                ));
                let database = run
                    .hir
                    .connections
                    .iter()
                    .find(|c| c.name.eq_ignore_ascii_case(connection));
                if let Some(ConnectionKind::DuckDb { path }) = database.map(|c| &c.kind) {
                    files.push(path);
                }
            }
        }
    }
    let mut seen = HashSet::new();
    for path in files {
        let rel = relative(path, base);
        if !seen.insert(rel.clone()) {
            continue;
        }
        let q = format!(
            "SELECT md5(content), size FROM read_blob({})",
            sql::string(&path.display().to_string())
        );
        let (digest, size): (String, i64) =
            db(conn.query_row(&q, [], |r| Ok((r.get(0)?, r.get(1)?))))
                .map_err(|e| format!("cannot read {rel}: {e}"))?;
        meta.push((format!("file:{rel}"), format!("{digest} {size}")));
    }
    Ok(meta)
}

fn md5(conn: &duckdb::Connection, text: &str) -> Res<String> {
    db(conn.query_row("SELECT md5(?)", duckdb::params![text], |r| r.get(0)))
}

/// `path` relative to directory `base`, `/`-separated (`../x.csv` outside it); both are
/// compared as [`export::file_identity`] keys.
fn relative(path: &Path, base: &Path) -> String {
    let path = export::file_identity(path);
    let base = export::file_identity(base);
    let (p, b): (Vec<_>, Vec<_>) = (path.components().collect(), base.components().collect());
    let common = p.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let up = std::iter::repeat_n("..".to_string(), b.len() - common);
    up.chain(
        p[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    )
    .collect::<Vec<_>>()
    .join("/")
}

/// Write `run`'s relations and `meta` to a new snapshot at `path`. The file there is replaced
/// only once the new one is complete (see [`export::commit`]).
pub fn write(run: &Run, meta: &Meta, path: &Path) -> Result<(), String> {
    let conn = &run.conn;
    db(conn.execute_batch(&format!(
        "CREATE OR REPLACE TEMP TABLE {} (key VARCHAR, value VARCHAR)",
        sql::ident(META)
    )))?;
    let mut insert = db(conn.prepare(&format!("INSERT INTO {} VALUES (?, ?)", sql::ident(META))))?;
    for (k, v) in meta {
        db(insert.execute(duckdb::params![k, v]))?;
    }
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let mut tables: Vec<(&str, &str)> = run
        .hir
        .relations
        .iter()
        .map(|r| (r.name.as_str(), r.name.as_str()))
        .collect();
    tables.push((META, META));
    let staged = export::Staged::new(path);
    if let Err(e) = export::write_duckdb(conn, &run.hir, staged.tmp(), &tables) {
        staged.discard();
        return Err(e);
    }
    export::commit(vec![staged]).map_err(|(_, e)| e)
}
