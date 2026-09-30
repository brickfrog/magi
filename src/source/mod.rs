//! Source readers. CSV, Parquet and DuckDB files are read by DuckDB itself; Excel workbooks and
//! ODBC queries are extracted by MAGI and staged into DuckDB as text before typing.
//!
//! Staging always has two phases: a raw table (text for CSV/Excel/ODBC, native types for
//! Parquet/DuckDB) that keeps the source row number in `__magi_row`, then the typed table
//! `__magi_typed_<source>` built by casting each column to its declared or inferred type. Values
//! that do not convert become null and are listed in `<source>.rejects` instead of silently
//! disappearing. The typed table is stored in source-row order and keeps `__magi_row` (worksheet
//! row for Excel, 1-based data line of the file or its section for CSV, result row for queries);
//! the relation `<source>` is a view over it without that column, so checks can name each failing
//! row's source row. `fill_down` columns are filled in the raw table (by the Excel reader, or in
//! SQL for CSV); a `row_number` column is added to the typed table.

pub mod csv;
pub mod excel;
pub mod fixed;
pub mod odbc;
pub mod staged;

use std::collections::HashMap;

use crate::backend::sql::{self, Expr, Query, Select, Stmt, TableRef, func, str_lit};
use crate::semantic::hir::{
    Connection, ConnectionKind, CsvOptions, Relation, Secret, Source, SourceKind,
};
use crate::semantic::schema::{InferredSchema, SchemaProvider};
use crate::semantic::types::{ColType, Type};
use staged::StagedTable;

/// A source that cannot be read. `code` is the diagnostic code to report it under (`M200` unless
/// the reader knows better, e.g. `M212` for a CSV file that is not one table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceError {
    pub code: &'static str,
    pub message: String,
    pub help: Option<String>,
}

impl From<String> for SourceError {
    fn from(message: String) -> Self {
        SourceError {
            code: "M200",
            message,
            help: None,
        }
    }
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub struct Reader {
    /// Scratch connection used to sniff file schemas.
    conn: duckdb::Connection,
    /// Contact databases (SQL sources). Local files are always read.
    pub contact_external: bool,
    extracted: HashMap<String, StagedTable>,
    /// The file DuckDB reads for each CSV (file, section, delimiter): a section is copied once
    /// per process, for type inference and staging alike.
    csv_files: HashMap<(std::path::PathBuf, u32, Option<String>), csv::CsvFile>,
    scratch: Option<Scratch>,
}

impl Reader {
    pub fn new(contact_external: bool) -> Result<Self, String> {
        let conn = duckdb::Connection::open_in_memory()
            .map_err(|e| format!("cannot start DuckDB: {e}"))?;
        // stale scratch directories are swept when this reader creates its own (`Scratch::create`)
        Ok(Reader {
            conn,
            contact_external,
            extracted: HashMap::new(),
            csv_files: HashMap::new(),
            scratch: None,
        })
    }

    /// The file DuckDB reads for a CSV source (see [`csv::CsvFile`]).
    fn csv_file(
        &mut self,
        path: &std::path::Path,
        options: &CsvOptions,
    ) -> Result<csv::CsvFile, SourceError> {
        let Some(n) = options.section else {
            return Ok(csv::CsvFile::whole(path));
        };
        let key = (path.to_path_buf(), n, options.delimiter.clone());
        if let Some(f) = self.csv_files.get(&key) {
            return Ok(f.clone());
        }
        if self.scratch.is_none() {
            self.scratch = Some(Scratch::create(&std::env::temp_dir()).map_err(|e| {
                SourceError::from(format!("cannot create a scratch directory: {e}"))
            })?);
        }
        let target = self
            .scratch
            .as_ref()
            .expect("created above")
            .file(self.csv_files.len(), n);
        let file = csv::CsvFile::section(path, options, n, &target)?;
        self.csv_files.insert(key, file.clone());
        Ok(file)
    }

    /// Excel / ODBC rows for a source (extracted once per run).
    fn extracted(
        &mut self,
        src: &Source,
        connections: &[Connection],
    ) -> Result<&StagedTable, SourceError> {
        if !self.extracted.contains_key(&src.name) {
            let table = match &src.kind {
                SourceKind::Excel { path, options } => excel::read_excel(path, options)?,
                SourceKind::FixedWidth { path, options } => fixed::read_fixed(path, options)?,
                SourceKind::Sql { connection, query } => match &find(connections, connection)?.kind
                {
                    ConnectionKind::Odbc { .. } => {
                        odbc::extract(&odbc_target(find(connections, connection)?)?, query)?
                    }
                    ConnectionKind::DuckDb { .. } => {
                        unreachable!("duckdb connections are read by DuckDB")
                    }
                },
                _ => unreachable!("only Excel, fixed-width and ODBC sources are extracted"),
            };
            self.extracted.insert(src.name.clone(), table);
        }
        Ok(&self.extracted[&src.name])
    }

    fn describe(&self, sql: &str) -> Result<Vec<(String, Type)>, String> {
        describe(&self.conn, sql)
    }

    fn with_attached<T>(
        &self,
        path: &std::path::Path,
        f: impl FnOnce(&Self) -> Result<T, String>,
    ) -> Result<T, String> {
        self.conn
            .execute_batch(&sql::render(&Stmt::Attach {
                path: path.display().to_string(),
                alias: DESCRIBE_ALIAS.into(),
                read_only: true,
            }))
            .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        let result = f(self);
        let _ = self.conn.execute_batch(&sql::render(&Stmt::Detach {
            alias: DESCRIBE_ALIAS.into(),
        }));
        result
    }

    /// Columns of a user query over the attached database (its tables are named without catalog).
    fn describe_in_attached(&self, query: &str) -> Result<Vec<(String, Type)>, String> {
        let use_db = |db: &str| {
            self.conn
                .execute_batch(&sql::render(&Stmt::Use {
                    database: db.into(),
                }))
                .map_err(|e| e.to_string())
        };
        use_db(DESCRIBE_ALIAS)?;
        let out = self.describe(query);
        let _ = use_db("memory");
        out
    }
}

/// Prefix of the scratch directories in the system's temporary directory; the rest of the name is
/// [`SCRATCH_RANDOM`] random letters and digits.
const SCRATCH_PREFIX: &str = "__magi_scratch-";
const SCRATCH_RANDOM: usize = 12;
/// The lock file of a scratch directory: its owner holds an exclusive lock on it while it runs.
const SCRATCH_LOCK: &str = "magi.lock";
/// A scratch directory younger than this is never swept (its owner may not hold its lock yet).
const SCRATCH_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

/// A reader's scratch directory for CSV section copies: a new directory with a random name, only
/// its owner may read it (mode 0700 on Unix), removed when dropped. While it exists its owner
/// holds an exclusive lock on [`SCRATCH_LOCK`] inside it, so a directory left by a killed process
/// is recognized by its free lock and removed by the next MAGI process that makes one (see
/// [`sweep_stale_scratch`]).
struct Scratch {
    /// Dropped before `dir`, releasing the lock before the directory is removed.
    lock: Option<std::fs::File>,
    dir: tempfile::TempDir,
}

impl Scratch {
    fn create(root: &std::path::Path) -> std::io::Result<Scratch> {
        let mut builder = tempfile::Builder::new();
        builder.prefix(SCRATCH_PREFIX).rand_bytes(SCRATCH_RANDOM);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(std::fs::Permissions::from_mode(0o700));
        }
        // fails if the path exists: a directory or link planted there is never used
        let dir = builder.tempdir_in(root)?;
        let lock = private_file(&dir.path().join(SCRATCH_LOCK))?;
        lock.lock()?;
        sweep_stale_scratch(root, dir.path());
        Ok(Scratch {
            lock: Some(lock),
            dir,
        })
    }

    /// Path of the `i`-th copy, which holds section `section`.
    fn file(&self, i: usize, section: u32) -> std::path::PathBuf {
        self.dir.path().join(format!("{i}-section-{section}.csv"))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // release the lock first: some systems cannot remove a file that is open
        self.lock.take();
    }
}

/// A new file only its owner may read and write (mode 0600 on Unix); fails if the path exists.
pub(crate) fn private_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Whether `name` is a scratch directory name MAGI makes.
fn is_scratch_name(name: &str) -> bool {
    name.strip_prefix(SCRATCH_PREFIX).is_some_and(|rest| {
        rest.len() == SCRATCH_RANDOM && rest.bytes().all(|b| b.is_ascii_alphanumeric())
    })
}

/// Whether `name` is a file MAGI puts in a scratch directory.
fn is_scratch_file(name: &str) -> bool {
    if name == SCRATCH_LOCK {
        return true;
    }
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    name.strip_suffix(".csv")
        .and_then(|n| n.split_once("-section-"))
        .is_some_and(|(i, s)| digits(i) && digits(s))
}

/// Remove the scratch directories of MAGI processes that were killed before they could clean up.
/// A directory in `root` is removed only when all of these hold, so nothing a running process or
/// another program uses is touched: its name is one MAGI makes; it is a real directory (not a
/// link) with the same owner as this process's scratch directory `own`; it is more than a minute
/// old; it holds only MAGI's files, all regular files; and its lock file exists and is not
/// locked (no process owns it).
fn sweep_stale_scratch(root: &std::path::Path, own: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let Ok(own_meta) = std::fs::symlink_metadata(own) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == own || !entry.file_name().to_str().is_some_and(is_scratch_name) {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        #[cfg(unix)]
        let same_owner = {
            use std::os::unix::fs::MetadataExt;
            meta.uid() == own_meta.uid()
        };
        #[cfg(not(unix))]
        let same_owner = {
            let _ = &own_meta;
            true
        };
        let old = meta
            .modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > SCRATCH_GRACE);
        if !meta.file_type().is_dir() || !same_owner || !old {
            continue;
        }
        let _ = remove_stale(&path);
    }
}

/// Remove a scratch directory whose lock is free and that holds only MAGI's files; leave it as is
/// otherwise.
fn remove_stale(dir: &std::path::Path) -> std::io::Result<()> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let regular = std::fs::symlink_metadata(entry.path())?
            .file_type()
            .is_file();
        if !regular || !entry.file_name().to_str().is_some_and(is_scratch_file) {
            return Ok(());
        }
        files.push(entry.path());
    }
    let lock_path = dir.join(SCRATCH_LOCK);
    let lock = std::fs::OpenOptions::new().write(true).open(&lock_path)?;
    if lock.try_lock().is_err() {
        return Ok(());
    }
    for f in files.iter().filter(|f| **f != lock_path) {
        std::fs::remove_file(f)?;
    }
    std::fs::remove_file(&lock_path)?;
    drop(lock);
    std::fs::remove_dir(dir)
}

/// Column names and types of a query, without running it.
fn describe(conn: &duckdb::Connection, sql: &str) -> Result<Vec<(String, Type)>, String> {
    let mut stmt = conn
        .prepare(&format!("DESCRIBE {sql}"))
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .map(|(n, t)| (n, Type::from_duckdb(&t)))
        .collect())
}

const DESCRIBE_ALIAS: &str = "__magi_describe";
/// Source row number column of the raw and typed staging tables.
pub const ROW: &str = "__magi_row";

fn find<'c>(connections: &'c [Connection], name: &str) -> Result<&'c Connection, String> {
    connections
        .iter()
        .find(|c| c.name == name)
        .ok_or_else(|| format!("unknown connection `{name}`"))
}

/// Whether MAGI extracts the source's rows itself (Excel, fixed-width, ODBC) instead of DuckDB
/// reading them.
fn is_extracted(src: &Source, connections: &[Connection]) -> Result<bool, String> {
    Ok(match &src.kind {
        SourceKind::Excel { .. } | SourceKind::FixedWidth { .. } => true,
        SourceKind::Sql { connection, .. } => matches!(
            find(connections, connection)?.kind,
            ConnectionKind::Odbc { .. }
        ),
        _ => false,
    })
}

fn secret(s: &Secret) -> Result<String, String> {
    match s {
        Secret::Literal(v) => Ok(v.clone()),
        Secret::Env(var) => {
            std::env::var(var).map_err(|_| format!("environment variable `{var}` is not set"))
        }
    }
}

/// How to reach an ODBC connection's data source; secrets are resolved here and never stored or
/// printed. A `dsn:` connection passes `user`/`password` as `SQLConnect` arguments. With a
/// `connection_string:` they are appended as `UID=`/`PWD=` attributes, so values containing `;`,
/// `{` or `}` are refused: drivers disagree on brace quoting (sqliteodbc ignores it), and such a
/// value could end the attribute and add others.
pub fn odbc_target(c: &Connection) -> Result<odbc::Target, String> {
    let ConnectionKind::Odbc {
        dsn,
        connection_string,
        user,
        password,
    } = &c.kind
    else {
        return Err(format!("connection `{}` is not an ODBC connection", c.name));
    };
    let resolved = |s: &Option<Secret>| s.as_ref().map(secret).transpose();
    let (user, password) = (resolved(user)?, resolved(password)?);
    let mut s = match (connection_string, dsn) {
        (Some(cs), _) => secret(cs)?,
        (None, Some(dsn)) => {
            return Ok(odbc::Target::Dsn {
                dsn: dsn.clone(),
                user,
                password,
            });
        }
        (None, None) => {
            return Err(format!(
                "connection `{}` has neither dsn nor connection_string",
                c.name
            ));
        }
    };
    if !s.is_empty() && !s.ends_with(';') {
        s.push(';');
    }
    for (key, attribute, value) in [("user", "UID", user), ("password", "PWD", password)] {
        let Some(value) = value else { continue };
        if value.contains([';', '{', '}']) {
            return Err(format!(
                "connection `{}`: the `{key}` value contains `;`, `{{` or `}}`, which a connection string cannot carry safely with every driver; declare the data source as `dsn:` (MAGI then passes `{key}` separately) or change the value",
                c.name
            ));
        }
        s += &format!("{attribute}={value};");
    }
    Ok(odbc::Target::ConnectionString(s))
}

/// A file path as DuckDB's file readers take it: they expand `*`, `?` and `[...]`, so each is
/// wrapped in a one-character class (`q[1].csv` → `q[[]1].csv`) to name exactly that file.
pub(crate) fn literal_path(path: &std::path::Path) -> String {
    let mut out = String::new();
    for c in path.display().to_string().chars() {
        match c {
            '[' | '*' | '?' => {
                out.push('[');
                out.push(c);
                out.push(']');
            }
            c => out.push(c),
        }
    }
    out
}

fn parquet_table(path: &std::path::Path) -> TableRef {
    TableRef::Func {
        name: "read_parquet".into(),
        args: vec![str_lit(&literal_path(path))],
        named: Vec::new(),
    }
}

fn select_star(from: TableRef) -> Query {
    Query::select(Select {
        items: vec![(Expr::Star { table: None }, None)],
        from: Some((from, None)),
        ..Select::default()
    })
}

fn missing_file(path: &std::path::Path) -> Result<(), String> {
    if path.exists() {
        Ok(())
    } else {
        Err(format!("file not found: {}", path.display()))
    }
}

impl SchemaProvider for Reader {
    fn infer(
        &mut self,
        src: &Source,
        connections: &[Connection],
    ) -> Result<Option<InferredSchema>, SourceError> {
        let declared = |name: &str| {
            src.declared
                .as_ref()
                .and_then(|d| d.iter().find(|c| c.name == name))
                .map(|c| c.ty.ty)
        };
        let columns = match &src.kind {
            SourceKind::Csv { path, options } => {
                missing_file(path)?;
                let file = self.csv_file(path, options)?;
                return csv::infer(&self.conn, &file, options, &declared).map(Some);
            }
            SourceKind::Parquet { path } => {
                missing_file(path)?;
                self.describe(&sql::render_query(&select_star(parquet_table(path))))?
            }
            SourceKind::Excel { path, .. } | SourceKind::FixedWidth { path, .. } => {
                missing_file(path)?;
                if let SourceKind::FixedWidth { options, .. } = &src.kind {
                    missing_file(&options.layout)?;
                }
                let t = self.extracted(src, connections)?;
                // a declared column's type decides how its values are read
                let notes = t
                    .notes
                    .iter()
                    .filter(|n| {
                        n.type_hint_for
                            .as_deref()
                            .is_none_or(|c| declared(c).is_none())
                    })
                    .cloned()
                    .collect();
                // a column declared numeric is checked against what its values are as numbers
                // (numbers stored as text included), like CSV
                let columns = t
                    .columns
                    .iter()
                    .map(|c| {
                        let ty = match (declared(&c.name), c.exact) {
                            (Some(Type::Int | Type::Decimal(..) | Type::Float), Some(exact)) => {
                                exact
                            }
                            _ => c.inferred,
                        };
                        (c.name.clone(), ty)
                    })
                    .collect();
                return Ok(Some(InferredSchema { columns, notes }));
            }
            SourceKind::DuckDb { path, table, query } => {
                missing_file(path)?;
                self.with_attached(path, |r| match (table, query) {
                    (Some(t), _) => r.describe(&sql::render_query(&select_star(
                        TableRef::Qualified(DESCRIBE_ALIAS.into(), t.clone()),
                    ))),
                    (None, Some(q)) => r.describe_in_attached(q),
                    (None, None) => unreachable!(),
                })?
            }
            SourceKind::Sql { connection, query } => {
                if !self.contact_external {
                    return Ok(None);
                }
                let conn = find(connections, connection)?;
                match &conn.kind {
                    ConnectionKind::Odbc { .. } => odbc::describe(&odbc_target(conn)?, query)?
                        .into_iter()
                        .map(|c| (c.name, c.inferred))
                        .collect(),
                    ConnectionKind::DuckDb { path } => {
                        missing_file(path)?;
                        self.with_attached(path, |r| r.describe_in_attached(query))?
                    }
                }
            }
        };
        Ok(Some(InferredSchema {
            columns,
            notes: Vec::new(),
        }))
    }

    fn check_format(&mut self, format: &str) -> Result<(), String> {
        let q = format!(
            "SELECT try_strptime('', {})",
            sql::render_expr(&str_lit(format))
        );
        self.conn.query_row(&q, [], |_| Ok(())).map_err(|e| {
            // `Invalid Input Error: Failed to parse format specifier %Q: ...`
            let first = csv::first_line(&e.to_string());
            match first.split_once("Error: ") {
                Some((_, message)) => message.to_string(),
                None => first,
            }
        })
    }
}

// ---------------------------------------------------------------------------------------------
// staging

/// What staging found. Row numbers refer to source rows (worksheet rows for Excel).
#[derive(Debug, Default)]
pub struct StageReport {
    pub rows: u64,
    /// (column, expected type, count, first source rows)
    pub rejects: Vec<(String, String, u64, Vec<i64>)>,
    /// Declared non-null columns that are null: (column, count, first rows)
    pub null_violations: Vec<(String, u64, Vec<i64>)>,
    /// Decimal values whose value changed when cast to the declared scale: (column, scale, count)
    pub rounded: Vec<(String, u8, u64)>,
    /// Identity problems: (duplicate identity rows, null identity rows)
    pub identity: Option<(u64, u64)>,
}

pub fn raw_name(source: &str) -> String {
    format!("__magi_raw_{source}")
}

/// Table holding a staged source's typed rows in source-row order, with each row's source row in
/// [`ROW`]; the relation `<source>` is a view over it.
pub fn typed_name(source: &str) -> String {
    format!("__magi_typed_{source}")
}

/// RE2 class of the Unicode White_Space characters (what Rust's `str::trim` removes, so CSV and
/// Excel agree on what a blank value is): tab, line breaks, space, NEL, no-break and other spaces.
pub(crate) const WHITESPACE: &str = r"[\s\x{0B}\x{85}\p{Z}]";

/// `x` without leading and trailing [`WHITESPACE`] (SQL text).
pub(crate) fn trimmed_sql(x: &str) -> String {
    format!("regexp_replace({x}, '^{WHITESPACE}+|{WHITESPACE}+$', '', 'g')")
}

/// `x` without leading and trailing [`WHITESPACE`].
fn trimmed(raw: Expr) -> Expr {
    func(
        "regexp_replace",
        vec![
            raw,
            str_lit(&format!("^{WHITESPACE}+|{WHITESPACE}+$")),
            str_lit(""),
            str_lit("g"),
        ],
    )
}

/// Blank text (empty or whitespace-only) is a missing value; other text is [`trimmed`].
fn blank_null(raw: Expr) -> Expr {
    func("nullif", vec![trimmed(raw), str_lit("")])
}

fn cast(expr: Expr, ty: &str, try_: bool) -> Expr {
    Expr::Cast {
        expr: Box::new(expr),
        ty: ty.to_string(),
        try_,
    }
}

/// Text a `bool` column reads as true / false (compared lowercased).
pub(crate) const TRUE_TEXT: [&str; 5] = ["true", "t", "yes", "y", "1"];
pub(crate) const FALSE_TEXT: [&str; 5] = ["false", "f", "no", "n", "0"];
/// Time text whose seconds have non-zero digits beyond microseconds, which DuckDB would cut.
pub(crate) const SUB_MICROSECOND: &str = r":[0-9]{2}\.[0-9]{6}[0-9]*[1-9]";
/// Time text ending in a UTC offset or zone (`Z`, `UTC`, `+02`, `-0530`, `+02:00`), which DuckDB
/// drops when it casts to a type without a time zone.
const OFFSET: &str = r"(?i):[0-9]{2}(\.[0-9]*)?\s*(z|utc|[+-][0-9]{1,2}(:?[0-9]{2})?)$";

/// Cast a text column to a MAGI type: null for blanks and for text that is not exactly a value of
/// the type (the caller lists those in the rejects).
fn cast_text(raw: Expr, ty: Type, formats: &[String]) -> Expr {
    let text = blank_null(raw.clone());
    let try_cast = |t: &str| cast(text.clone(), t, true);
    let strptime = |fmts: Expr| func("try_strptime", vec![text.clone(), fmts]);
    let user_formats = || strptime(Expr::List(formats.iter().map(|f| str_lit(f)).collect()));
    let matches = |pattern: &str| func("regexp_matches", vec![text.clone(), str_lit(pattern)]);
    // null where `bad` holds
    let unless = |bad: Expr, value: Expr| Expr::Case {
        whens: vec![(bad, Expr::Lit(sql::Lit::Null))],
        otherwise: Some(Box::new(value)),
    };
    // A plain time or timestamp has no zone: ISO text with an offset is not one (declared formats
    // cannot read offsets into them, see `resolve`). No time keeps more than microseconds.
    let zoneless = |ty: &str| {
        unless(
            sql::bin("OR", matches(OFFSET), matches(SUB_MICROSECOND)),
            try_cast(ty),
        )
    };
    let whole_micros = |value: Expr| unless(matches(SUB_MICROSECOND), value);
    // `TRY_CAST(x AS DATE)` also accepts `2026-01-05 13:45`, dropping the time
    let iso_date = cast(strptime(str_lit("%Y-%m-%d")), "DATE", false);
    // Declared formats never replace ISO text: Excel date and time cells arrive as ISO text and
    // may share a column with text cells in the declared format. The declared formats are tried
    // first, so one that reads ISO text differently (`%Y-%d-%m`) decides such values.
    let with_iso = |parsed: Expr, iso: Expr| func("coalesce", vec![parsed, iso]);
    match ty {
        Type::String | Type::Unknown | Type::Null => raw,
        Type::Bool => {
            let l = func("lower", vec![text.clone()]);
            let words = |w: [&str; 5]| w.iter().map(|v| str_lit(v)).collect();
            Expr::Case {
                whens: vec![
                    (
                        Expr::InList {
                            expr: Box::new(l.clone()),
                            list: words(TRUE_TEXT),
                            negated: false,
                        },
                        Expr::Lit(sql::Lit::Bool(true)),
                    ),
                    (
                        Expr::InList {
                            expr: Box::new(l),
                            list: words(FALSE_TEXT),
                            negated: false,
                        },
                        Expr::Lit(sql::Lit::Bool(false)),
                    ),
                ],
                otherwise: None,
            }
        }
        // `TRY_CAST('1.5' AS BIGINT)` rounds; only whole numbers (`12`, `12.00`) are integers
        Type::Int => Expr::Case {
            whens: vec![(
                func(
                    "regexp_full_match",
                    vec![text.clone(), str_lit(r"[+-]?[0-9]+(\.0*)?")],
                ),
                try_cast("BIGINT"),
            )],
            otherwise: None,
        },
        Type::Date if formats.is_empty() => iso_date,
        // a format with a time part reads a date only when that time is midnight
        Type::Date => {
            let parsed = user_formats();
            let date = cast(parsed.clone(), "DATE", false);
            let exact = Expr::Case {
                whens: vec![(sql::bin("=", date.clone(), parsed), date)],
                otherwise: None,
            };
            with_iso(exact, iso_date)
        }
        Type::Time if formats.is_empty() => zoneless("TIME"),
        Type::Time => whole_micros(with_iso(
            cast(user_formats(), "TIME", false),
            zoneless("TIME"),
        )),
        Type::Timestamp if formats.is_empty() => zoneless("TIMESTAMP"),
        Type::Timestamp => whole_micros(with_iso(
            cast(user_formats(), "TIMESTAMP", false),
            zoneless("TIMESTAMP"),
        )),
        // formats with `%z` read the offset; others are UTC (the session time zone)
        Type::TimestampTz if formats.is_empty() => whole_micros(try_cast("TIMESTAMPTZ")),
        Type::TimestampTz => whole_micros(with_iso(
            cast(user_formats(), "TIMESTAMPTZ", false),
            try_cast("TIMESTAMPTZ"),
        )),
        other => try_cast(&other.duckdb_name()),
    }
}

/// Cast a natively typed column (Parquet, DuckDB) to another type; null where the value would
/// change beyond rounding a decimal (which is reported separately, M209).
fn cast_native(raw: Expr, from: Type, to: Type) -> Expr {
    let exact_when = |same: Expr, value: Expr| Expr::Case {
        whens: vec![(same, value)],
        otherwise: None,
    };
    match (from, to) {
        (Type::Decimal(..) | Type::Float, Type::Int) => exact_when(
            sql::bin("=", func("trunc", vec![raw.clone()]), raw.clone()),
            cast(raw, "BIGINT", true),
        ),
        (Type::Timestamp | Type::TimestampTz, Type::Date) => exact_when(
            sql::bin("=", cast(raw.clone(), "DATE", true), raw.clone()),
            cast(raw, "DATE", true),
        ),
        _ => cast(raw, &to.duckdb_name(), true),
    }
}

/// Whether a decimal cast changed a value (M209): `12.500` into scale 2 is exact, `1.005` is not.
/// Plain decimal text is judged by its significant decimal places, so no wider decimal is needed
/// (there is none beyond 38 digits); other number text (`1.2345e-1`) is compared as a double.
fn rounding_check(raw: &Expr, typed: &Expr, source: Type, s: u8) -> Option<Expr> {
    let exact = if source == Type::String {
        let text = blank_null(raw.clone());
        let places = func(
            "length",
            vec![func(
                "rtrim",
                vec![
                    func(
                        "regexp_extract",
                        vec![text.clone(), str_lit(r"\.([0-9]*)$"), sql::int(1)],
                    ),
                    str_lit("0"),
                ],
            )],
        );
        let changed = Expr::Case {
            whens: vec![(
                func("regexp_full_match", vec![text.clone(), str_lit(csv::PLAIN)]),
                sql::bin(">", places, sql::int(i64::from(s))),
            )],
            otherwise: Some(Box::new(sql::bin(
                "<>",
                cast(typed.clone(), "DOUBLE", false),
                cast(text, "DOUBLE", true),
            ))),
        };
        return Some(sql::bin(
            "AND",
            Expr::IsNull {
                expr: Box::new(typed.clone()),
                negated: true,
            },
            changed,
        ));
    } else if matches!(source, Type::Int | Type::Decimal(..) | Type::Float) {
        raw.clone()
    } else {
        return None;
    };
    Some(sql::bin("<>", exact, typed.clone()))
}

/// How a raw table is filled.
enum RawLoad {
    /// SQL creating the raw table; `cleanup` runs afterwards whether or not it succeeded. `text`:
    /// every column is text (CSV).
    Sql {
        create: Vec<Stmt>,
        cleanup: Vec<Stmt>,
        text: bool,
    },
    /// An empty text table the MAGI-side reader appends its rows to.
    Appended { create: Stmt, reader: &'static str },
}

/// `SELECT row_number() OVER () AS __magi_row, * FROM from`.
fn with_row(from: TableRef) -> Query {
    Query::select(Select {
        items: vec![
            (
                Expr::Window {
                    func: Box::new(func("row_number", Vec::new())),
                    partition: Vec::new(),
                    order: Vec::new(),
                },
                Some(ROW.into()),
            ),
            (Expr::Star { table: None }, None),
        ],
        from: Some((from, None)),
        ..Select::default()
    })
}

/// `SELECT __magi_row, <columns> FROM (rows)` where each `fill` column's blank values (null or
/// whitespace-only text) take the nearest non-blank value above them in [`ROW`] order, and blanks
/// before the first non-blank become null. `rows` (which has [`ROW`]) is returned as is when no
/// column is filled. `fill` names match columns ignoring ASCII case, as all names do.
fn filled(rows: Query, columns: &[String], fill: &[String]) -> Query {
    let is_filled = |c: &str| fill.iter().any(|f| f.eq_ignore_ascii_case(c));
    if !columns.iter().any(|c| is_filled(c)) {
        return rows;
    }
    let mut items = vec![(sql::col(ROW), Some(ROW.to_string()))];
    for c in columns {
        let value = if is_filled(c) {
            let non_blank = sql::bin("<>", trimmed(sql::col(c)), str_lit(""));
            Expr::Window {
                func: Box::new(Expr::Func {
                    name: "arg_max".into(),
                    args: vec![sql::col(c), sql::col(ROW)],
                    distinct: false,
                    filter: Some(Box::new(non_blank)),
                }),
                partition: Vec::new(),
                order: vec![sql::OrderBy {
                    expr: sql::col(ROW),
                    desc: false,
                }],
            }
        } else {
            sql::col(c)
        };
        items.push((value, Some(c.clone())));
    }
    Query::select(Select {
        items,
        from: Some((TableRef::Sub(Box::new(rows)), None)),
        ..Select::default()
    })
}

/// Statements creating the raw table of `src`. `columns` names the text columns of sources whose
/// rows MAGI extracts (Excel, ODBC) and the file's columns of a CSV source; `csv_read` is the file
/// DuckDB reads for a CSV source (see [`csv::CsvFile`]).
fn raw_load(
    src: &Source,
    connections: &[Connection],
    columns: &[String],
    csv_read: Option<&std::path::Path>,
) -> Result<RawLoad, String> {
    let raw = raw_name(&src.name);
    let create_raw = |q: Query| Stmt::CreateTable {
        name: raw.clone(),
        query: q,
        temp: true,
    };
    let from_duckdb = |path: &std::path::Path, table: Option<&str>, query: Option<&str>| {
        let alias = format!("__magi_src_{}", src.name);
        let attach = Stmt::Attach {
            path: path.display().to_string(),
            alias: alias.clone(),
            read_only: true,
        };
        let detach = Stmt::Detach {
            alias: alias.clone(),
        };
        match (table, query) {
            (Some(t), _) => RawLoad::Sql {
                create: vec![
                    attach,
                    create_raw(with_row(TableRef::Qualified(alias, t.into()))),
                ],
                cleanup: vec![detach],
                text: false,
            },
            // the user's query names tables of the attached database without a catalog
            (None, Some(q)) => RawLoad::Sql {
                create: vec![
                    attach,
                    Stmt::Use {
                        database: alias.clone(),
                    },
                    create_raw(with_row(TableRef::Sub(Box::new(Query {
                        ctes: Vec::new(),
                        body: sql::Body::Raw(q.to_string()),
                    })))),
                ],
                cleanup: vec![
                    Stmt::Use {
                        database: "memory".into(),
                    },
                    detach,
                ],
                text: false,
            },
            (None, None) => unreachable!("a duckdb source has a table or a query"),
        }
    };
    let appended = |reader| {
        let mut cols = vec![(ROW.to_string(), "BIGINT".to_string())];
        cols.extend(columns.iter().map(|c| (c.clone(), "VARCHAR".to_string())));
        RawLoad::Appended {
            create: Stmt::CreateEmptyTable {
                name: raw.clone(),
                columns: cols,
            },
            reader,
        }
    };
    Ok(match &src.kind {
        SourceKind::Csv { path, options } => RawLoad::Sql {
            create: vec![create_raw(filled(
                with_row(csv::table(csv_read.unwrap_or(path), options, true, columns)),
                columns,
                &options.fill_down,
            ))],
            cleanup: Vec::new(),
            text: true,
        },
        SourceKind::Parquet { path } => RawLoad::Sql {
            create: vec![create_raw(with_row(parquet_table(path)))],
            cleanup: Vec::new(),
            text: false,
        },
        SourceKind::DuckDb { path, table, query } => {
            from_duckdb(path, table.as_deref(), query.as_deref())
        }
        SourceKind::Excel { .. } => appended("Excel"),
        SourceKind::FixedWidth { .. } => appended("fixed-width"),
        SourceKind::Sql { connection, query } => match &find(connections, connection)?.kind {
            ConnectionKind::DuckDb { path } => from_duckdb(path, None, Some(query)),
            ConnectionKind::Odbc { .. } => appended("ODBC"),
        },
    })
}

/// Everything staging runs once the raw table exists.
struct Typing {
    rejects: Stmt,
    typed: Stmt,
    relation: Stmt,
    cleanup: Stmt,
    /// Decimal columns whose values may be rounded: (column, scale, predicate over the raw table).
    rounding: Vec<(String, u8, Expr)>,
}

/// Typing of `rel`'s columns from the raw table. `raw_type` is a raw column's type (ignored when
/// `text`, where every raw column is text).
fn typing(src: &Source, rel: &Relation, raw_type: &dyn Fn(&str) -> Type, text: bool) -> Typing {
    let raw = raw_name(&src.name);
    let typed = typed_name(&src.name);
    let declared = |name: &str| {
        src.declared
            .as_ref()
            .and_then(|d| d.iter().find(|c| c.name == name))
    };
    let mut typed_items = vec![(sql::col(ROW), Some(ROW.to_string()))];
    let mut reject_parts = Vec::new();
    let mut rounding = Vec::new();
    for col in &rel.columns {
        // the data row's position among the source's rows (Excel's `__magi_row` is the
        // worksheet row)
        if src.kind.row_number() == Some(col.name.as_str()) {
            let position = Expr::Window {
                func: Box::new(func("row_number", Vec::new())),
                partition: Vec::new(),
                order: vec![sql::OrderBy {
                    expr: sql::col(ROW),
                    desc: false,
                }],
            };
            typed_items.push((position, Some(col.name.clone())));
            continue;
        }
        let rawc = sql::col(&col.name);
        let ty = col.ty.ty;
        let source = if text {
            Type::String
        } else {
            raw_type(&col.name)
        };
        let formats = declared(&col.name)
            .map(|d| d.formats.as_slice())
            .unwrap_or_default();
        // a whitespace-only value in a file is blank, also in a `string` column: null, like an
        // empty field (other text is kept as written)
        let from_file = matches!(
            src.kind,
            SourceKind::Csv { .. } | SourceKind::Excel { .. } | SourceKind::FixedWidth { .. }
        );
        let (expr, present) = if text && ty == Type::String && from_file {
            let blank = sql::bin("=", trimmed(rawc.clone()), str_lit(""));
            let value = Expr::Case {
                whens: vec![(blank, Expr::Lit(sql::Lit::Null))],
                otherwise: Some(Box::new(rawc.clone())),
            };
            (value, None)
        } else if source == ty || matches!(ty, Type::Unknown | Type::Null) {
            (rawc.clone(), None)
        } else if source == Type::String {
            let present = Expr::IsNull {
                expr: Box::new(blank_null(rawc.clone())),
                negated: true,
            };
            (cast_text(rawc.clone(), ty, formats), Some(present))
        } else {
            let present = Expr::IsNull {
                expr: Box::new(rawc.clone()),
                negated: true,
            };
            (cast_native(rawc.clone(), source, ty), Some(present))
        };
        if let Some(present) = present {
            let failed = sql::bin(
                "AND",
                present,
                Expr::IsNull {
                    expr: Box::new(expr.clone()),
                    negated: false,
                },
            );
            reject_parts.push(Query::select(Select {
                items: vec![
                    (sql::col(ROW), Some("row".into())),
                    (str_lit(&col.name), Some("column".into())),
                    (
                        str_lit(&ColType::new(ty, true).ty.to_string()),
                        Some("expected".into()),
                    ),
                    (cast(rawc.clone(), "VARCHAR", false), Some("value".into())),
                ],
                from: Some((TableRef::Named(raw.clone()), None)),
                where_: Some(failed),
                ..Select::default()
            }));
            if let Type::Decimal(_, s) = ty
                && let Some(check) = rounding_check(&rawc, &expr, source, s)
            {
                rounding.push((col.name.clone(), s, check));
            }
        }
        typed_items.push((expr, Some(col.name.clone())));
    }
    let rejects_name = format!("{}.rejects", src.name);
    let rejects = if reject_parts.is_empty() {
        Stmt::CreateEmptyTable {
            name: rejects_name,
            columns: vec![
                ("row".into(), "BIGINT".into()),
                ("column".into(), "VARCHAR".into()),
                ("expected".into(), "VARCHAR".into()),
                ("value".into(), "VARCHAR".into()),
            ],
        }
    } else {
        Stmt::CreateTable {
            name: rejects_name,
            query: Query {
                ctes: Vec::new(),
                body: sql::Body::UnionAllByName(reject_parts),
            },
            temp: true,
        }
    };
    // The typed table is stored in source-row order; the relation is a view over it without the
    // source row, and scans of it keep that order.
    let relation = Stmt::CreateView {
        name: rel.name.clone(),
        query: Query::select(Select {
            items: rel
                .columns
                .iter()
                .map(|c| (sql::col(&c.name), Some(c.name.clone())))
                .collect(),
            from: Some((TableRef::Named(typed.clone()), None)),
            ..Select::default()
        }),
    };
    Typing {
        typed: Stmt::CreateTable {
            name: typed,
            query: Query::select(Select {
                items: typed_items,
                from: Some((TableRef::Named(raw.clone()), None)),
                order_by: vec![sql::OrderBy {
                    expr: sql::col(ROW),
                    desc: false,
                }],
                ..Select::default()
            }),
            temp: true,
        },
        rejects,
        relation,
        cleanup: Stmt::Drop {
            name: raw,
            view: false,
        },
        rounding,
    }
}

/// Columns the source itself provides: `rel`'s, without the row number column staging adds.
fn source_columns(src: &Source, rel: &Relation) -> Vec<String> {
    rel.columns
        .iter()
        .filter(|c| src.kind.row_number() != Some(c.name.as_str()))
        .map(|c| c.name.clone())
        .collect()
}

/// The SQL `magi run` executes to stage `src` as `rel`, as `;`-terminated statements with `--`
/// comments. For Parquet and DuckDB sources the file is described (not read) to find the raw
/// column types, so the typed projection shows the casts `run` applies.
pub fn staging_sql(src: &Source, rel: &Relation, connections: &[Connection]) -> String {
    let names = source_columns(src, rel);
    let mut out: Vec<String> = Vec::new();
    let mut described = HashMap::new();
    // a CSV section is read from a scratch copy that exists only while `run` stages it
    let section = match &src.kind {
        SourceKind::Csv { path, options } => csv::section_display(path, options),
        _ => None,
    };
    let csv_read = section.map(|(comment, shown)| {
        out.push(comment);
        shown
    });
    let text = match raw_load(src, connections, &names, csv_read.as_deref()) {
        Ok(RawLoad::Sql {
            create,
            cleanup,
            text,
        }) => {
            if !text {
                match describe_raw(&create, &cleanup) {
                    Ok(types) => described = types,
                    Err(e) => out.push(format!(
                        "-- the source could not be opened ({}); its column types are assumed to be the declared/inferred ones",
                        csv::first_line(&e)
                    )),
                }
            }
            out.extend(create.iter().chain(&cleanup).map(sql::render));
            text
        }
        Ok(RawLoad::Appended { create, reader }) => {
            out.push(sql::render(&create));
            out.push(format!(
                "-- the {reader} reader appends every row as text, with {ROW} = its source row"
            ));
            let fill = src.kind.fill_down();
            if !fill.is_empty() {
                let listed: Vec<String> = fill.iter().map(|c| format!("`{c}`")).collect();
                out.push(format!(
                    "-- it fills {} down: an empty cell takes the value of the nearest non-empty cell above it",
                    listed.join(", ")
                ));
            }
            true
        }
        Err(e) => {
            out.push(format!("-- {e}"));
            true
        }
    };
    if rel.open {
        out.push(
            "-- the columns are known only when the source is contacted (`--sources`)".to_string(),
        );
    }
    let declared: HashMap<&str, Type> = rel
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.ty.ty))
        .collect();
    let raw_type = |name: &str| {
        described
            .get(name)
            .or_else(|| declared.get(name))
            .copied()
            .unwrap_or(Type::String)
    };
    let t = typing(src, rel, &raw_type, text);
    out.extend([&t.rejects, &t.typed, &t.relation, &t.cleanup].map(sql::render));
    out.iter()
        .map(|s| {
            if s.starts_with("--") {
                s.clone()
            } else {
                format!("{s};")
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Column types of the raw table `create` builds, from a scratch DuckDB: the source is described,
/// not read. `cleanup` detaches what `create` attached.
fn describe_raw(create: &[Stmt], cleanup: &[Stmt]) -> Result<HashMap<String, Type>, String> {
    let conn = duckdb::Connection::open_in_memory().map_err(|e| e.to_string())?;
    let exec = |s: &Stmt| {
        conn.execute_batch(&sql::render(s))
            .map_err(|e| e.to_string())
    };
    let Some((Stmt::CreateTable { query, .. }, setup)) = create.split_last() else {
        return Err("the raw table is not created by a query".into());
    };
    let result = setup
        .iter()
        .try_for_each(exec)
        .and_then(|()| describe(&conn, &sql::render_query(query)));
    for s in cleanup {
        let _ = exec(s);
    }
    Ok(result?.into_iter().collect())
}

/// Stage one source into DuckDB: its typed table and the relation `rel.name` over it, plus
/// `<name>.rejects`.
pub fn stage(
    conn: &duckdb::Connection,
    src: &Source,
    rel: &Relation,
    reader: &mut Reader,
    connections: &[Connection],
) -> Result<StageReport, String> {
    let raw = raw_name(&src.name);
    let exec = |s: &Stmt| {
        conn.execute_batch(&sql::render(s))
            .map_err(|e| e.to_string())
    };
    // the file DuckDB reads for a CSV source (a section's copy is shared with type inference)
    let csv_file = match &src.kind {
        SourceKind::Csv { path, options } => {
            Some(reader.csv_file(path, options).map_err(|e| e.message)?)
        }
        _ => None,
    };
    let extracted = if is_extracted(src, connections)? {
        Some(reader.extracted(src, connections).map_err(|e| e.message)?)
    } else {
        None
    };
    let names = match extracted {
        Some(t) => t.columns.iter().map(|c| c.name.clone()).collect(),
        None => source_columns(src, rel),
    };
    let text = match raw_load(
        src,
        connections,
        &names,
        csv_file.as_ref().map(|f| f.path()),
    )? {
        RawLoad::Sql {
            create,
            cleanup,
            text,
        } => {
            let result = create.iter().try_for_each(exec);
            for s in &cleanup {
                let _ = exec(s);
            }
            // DuckDB's CSV errors quote the offending line
            result.map_err(|e| match &csv_file {
                Some(f) => f.error_line(&e),
                None => e,
            })?;
            text
        }
        RawLoad::Appended { create, .. } => {
            exec(&create)?;
            append_staged(
                conn,
                &raw,
                extracted.expect("extracted sources are appended"),
            )?;
            true
        }
    };
    // native types of the raw table
    let raw_types: HashMap<String, Type> = describe(conn, &sql::ident(&raw))?.into_iter().collect();
    let raw_type = |name: &str| raw_types.get(name).copied().unwrap_or(Type::String);
    let t = typing(src, rel, &raw_type, text);

    let mut report = StageReport::default();
    for (column, scale, check) in &t.rounding {
        let n = count(conn, &raw, check)?;
        if n > 0 {
            report.rounded.push((column.clone(), *scale, n));
        }
    }
    exec(&t.rejects)?;
    if let Stmt::CreateTable { name, .. } = &t.rejects {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT \"column\", any_value(\"expected\"), count(*), CAST(list(\"row\" ORDER BY \"row\")[1:5] AS VARCHAR) FROM {} GROUP BY \"column\" ORDER BY min(\"row\")",
                sql::ident(name)
            ))
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        for row in rows {
            let (c, e, n, first) = row.map_err(|e| e.to_string())?;
            report.rejects.push((c, e, n as u64, parse_list(&first)));
        }
    }
    // typed relation (with source row numbers for the null checks)
    exec(&t.typed)?;
    let typed = typed_name(&src.name);
    let declared = |name: &str| {
        src.declared
            .as_ref()
            .and_then(|d| d.iter().find(|c| c.name == name))
    };
    for col in &rel.columns {
        if declared(&col.name).is_some_and(|d| !d.ty.nullable) {
            let (n, first) = count_rows(
                conn,
                &typed,
                &Expr::IsNull {
                    expr: Box::new(sql::col(&col.name)),
                    negated: false,
                },
            )?;
            if n > 0 {
                report.null_violations.push((col.name.clone(), n, first));
            }
        }
    }
    if let Some(ids) = &src.identity {
        let key = if ids.len() == 1 {
            sql::col(&ids[0])
        } else {
            func("row", ids.iter().map(|i| sql::col(i)).collect())
        };
        let any_null = ids
            .iter()
            .map(|i| Expr::IsNull {
                expr: Box::new(sql::col(i)),
                negated: false,
            })
            .reduce(|a, b| sql::bin("OR", a, b))
            .unwrap();
        let q = format!(
            "SELECT count(*) - count(DISTINCT {k}) FILTER (WHERE NOT ({n})) - count(*) FILTER (WHERE {n}), count(*) FILTER (WHERE {n}) FROM {t}",
            k = sql::render_expr(&key),
            n = sql::render_expr(&any_null),
            t = sql::ident(&typed)
        );
        let (dups, nulls): (i64, i64) = conn
            .query_row(&q, [], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| e.to_string())?;
        if dups > 0 || nulls > 0 {
            report.identity = Some((dups as u64, nulls as u64));
        }
    }
    exec(&t.relation)?;
    report.rows = conn
        .query_row(
            &format!("SELECT count(*) FROM {}", sql::ident(&rel.name)),
            [],
            |r| r.get::<_, i64>(0),
        )
        .map_err(|e| e.to_string())? as u64;
    exec(&t.cleanup)?;
    Ok(report)
}

fn append_staged(conn: &duckdb::Connection, raw: &str, table: &StagedTable) -> Result<(), String> {
    let mut app = conn.appender(raw).map_err(|e| e.to_string())?;
    for (row, src_row) in table.rows.iter().zip(&table.source_rows) {
        let mut values: Vec<duckdb::types::Value> = Vec::with_capacity(row.len() + 1);
        values.push(duckdb::types::Value::BigInt(*src_row as i64));
        for v in row {
            values.push(match v {
                Some(s) => duckdb::types::Value::Text(s.clone()),
                None => duckdb::types::Value::Null,
            });
        }
        app.append_row(duckdb::appender_params_from_iter(values))
            .map_err(|e| e.to_string())?;
    }
    app.flush().map_err(|e| e.to_string())
}

fn count(conn: &duckdb::Connection, table: &str, pred: &Expr) -> Result<u64, String> {
    let q = format!(
        "SELECT count(*) FROM {} WHERE {}",
        sql::ident(table),
        sql::render_expr(pred)
    );
    conn.query_row(&q, [], |r| r.get::<_, i64>(0))
        .map(|n| n as u64)
        .map_err(|e| e.to_string())
}

fn count_rows(
    conn: &duckdb::Connection,
    table: &str,
    pred: &Expr,
) -> Result<(u64, Vec<i64>), String> {
    let q = format!(
        "SELECT count(*), CAST(list({row} ORDER BY {row})[1:5] AS VARCHAR) FROM {} WHERE {}",
        sql::ident(table),
        sql::render_expr(pred),
        row = sql::ident(ROW),
    );
    conn.query_row(&q, [], |r| {
        Ok((
            r.get::<_, i64>(0)? as u64,
            r.get::<_, Option<String>>(1)?.unwrap_or_default(),
        ))
    })
    .map(|(n, l)| (n, parse_list(&l)))
    .map_err(|e| e.to_string())
}

/// Parse DuckDB's text form of an integer list, `[1, 2, 3]`.
fn parse_list(s: &str) -> Vec<i64> {
    s.trim_matches(|c| c == '[' || c == ']')
        .split(',')
        .filter_map(|x| x.trim().parse().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::hir::{Column, RelKind};
    use crate::syntax::span::Span;

    fn odbc(dsn: Option<&str>, cs: Option<&str>, user: &str, password: &str) -> Connection {
        Connection {
            name: "w".into(),
            kind: ConnectionKind::Odbc {
                dsn: dsn.map(Into::into),
                connection_string: cs.map(|c| Secret::Literal(c.into())),
                user: Some(Secret::Literal(user.into())),
                password: Some(Secret::Literal(password.into())),
            },
        }
    }

    #[test]
    fn odbc_credentials_never_become_connection_string_attributes() {
        let injection = "x};Database=/tmp/other.db;{";
        // a DSN gets the credentials as separate SQLConnect arguments, exactly as given
        let dsn = odbc_target(&odbc(Some("prod"), None, "ana", injection)).unwrap();
        assert!(matches!(
            &dsn,
            odbc::Target::Dsn { dsn, user, password }
                if dsn == "prod" && user.as_deref() == Some("ana") && password.as_deref() == Some(injection)
        ));
        // a connection string refuses values that could end their attribute
        let base = Some("Driver=/x.so;Database=/tmp/prod.db");
        for bad in [injection, "a;b", "{a", "a}"] {
            let error = odbc_target(&odbc(None, base, "ana", bad)).err().unwrap();
            assert!(error.contains("`password`"), "{error}");
            assert!(!error.contains(bad), "{error}");
            let error = odbc_target(&odbc(None, base, bad, "pw")).err().unwrap();
            assert!(error.contains("`user`") && !error.contains(bad), "{error}");
        }
        let ok = odbc_target(&odbc(None, base, "ana", "p=w")).unwrap();
        assert!(matches!(
            &ok,
            odbc::Target::ConnectionString(s)
                if s == "Driver=/x.so;Database=/tmp/prod.db;UID=ana;PWD=p=w;"
        ));
    }

    #[test]
    fn a_section_is_copied_once_per_process_and_removed_with_the_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.csv");
        std::fs::write(&path, "title\n\na,b\n1,2\n").unwrap();
        let options = CsvOptions {
            section: Some(2),
            ..CsvOptions::default()
        };
        let mut reader = Reader::new(false).unwrap();
        let first = reader.csv_file(&path, &options).unwrap();
        let again = reader.csv_file(&path, &options).unwrap();
        assert_eq!(first.path(), again.path());
        assert_eq!(std::fs::read(first.path()).unwrap(), b"a,b\n1,2\n");
        let scratch = first.path().parent().unwrap().to_path_buf();
        // the copy and the lock file, readable by their owner only
        assert_eq!(std::fs::read_dir(&scratch).unwrap().count(), 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode();
            assert_eq!(mode(&scratch) & 0o777, 0o700);
            assert_eq!(mode(first.path()) & 0o777, 0o600);
        }
        drop(reader);
        assert!(!scratch.exists());
    }

    #[test]
    fn each_scratch_directory_is_new_and_randomly_named() {
        let root = tempfile::tempdir().unwrap();
        let a = Scratch::create(root.path()).unwrap();
        let b = Scratch::create(root.path()).unwrap();
        assert_ne!(a.dir.path(), b.dir.path());
        assert!(is_scratch_name(
            a.dir.path().file_name().unwrap().to_str().unwrap()
        ));
    }

    #[test]
    fn only_unlocked_magi_scratch_directories_are_swept() {
        let root = tempfile::tempdir().unwrap();
        let old = std::time::SystemTime::now() - 2 * SCRATCH_GRACE;
        let make = |name: &str, files: &[&str], aged: bool| {
            let d = root.path().join(name);
            std::fs::create_dir(&d).unwrap();
            for f in files {
                std::fs::write(d.join(f), "a\n").unwrap();
            }
            if aged {
                std::fs::File::open(&d).unwrap().set_modified(old).unwrap();
            }
            d
        };
        let name = |tail: &str| format!("{SCRATCH_PREFIX}{tail}");
        let stale = make(
            &name("aaaaaaaaaaaa"),
            &[SCRATCH_LOCK, "0-section-2.csv"],
            true,
        );
        let live = make(
            &name("bbbbbbbbbbbb"),
            &[SCRATCH_LOCK, "0-section-2.csv"],
            true,
        );
        let held = std::fs::OpenOptions::new()
            .write(true)
            .open(live.join(SCRATCH_LOCK))
            .unwrap();
        held.lock().unwrap();
        let foreign = make(&name("cccccccccccc"), &[SCRATCH_LOCK, "notes.txt"], true);
        let unlocked = make(&name("dddddddddddd"), &["0-section-2.csv"], true);
        let young = make(&name("eeeeeeeeeeee"), &[SCRATCH_LOCK], false);
        let other_name = make(&name("999998-notmagi"), &[SCRATCH_LOCK], true);
        let _own = Scratch::create(root.path()).unwrap();
        assert!(!stale.exists());
        for kept in [&live, &foreign, &unlocked, &young, &other_name] {
            assert!(kept.exists(), "{}", kept.display());
        }
        drop(held);
    }

    fn csv_source(dir: &std::path::Path, text: &str) -> (Source, Relation) {
        let path = dir.join("s.csv");
        std::fs::write(&path, text).unwrap();
        let src = Source {
            name: "s".into(),
            kind: SourceKind::Csv {
                path,
                options: CsvOptions::default(),
            },
            declared: None,
            identity: None,
            span: Span::default(),
        };
        let rel = Relation {
            name: "s".into(),
            kind: RelKind::Source,
            columns: vec![
                Column {
                    name: "id".into(),
                    ty: ColType::nullable(Type::Int),
                    lineage: 0,
                },
                Column {
                    name: "amount".into(),
                    ty: ColType::nullable(Type::Decimal(18, 2)),
                    lineage: 0,
                },
            ],
            open: false,
            identity: None,
            sort: Vec::new(),
            span: Span::default(),
        };
        (src, rel)
    }

    #[test]
    fn typed_rows_keep_source_rows_and_order() {
        let dir = tempfile::tempdir().unwrap();
        // ids run backwards so that source row and id order disagree; enough rows for parallel
        // insertion into several row groups
        let n = 300_000;
        let mut text = String::from("id,amount\n");
        for i in 0..n {
            text += &format!("{},{}.25\n", n - i, i % 1000);
        }
        let (src, rel) = csv_source(dir.path(), &text);
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let mut reader = Reader::new(false).unwrap();
        let report = stage(&conn, &src, &rel, &mut reader, &[]).unwrap();
        assert_eq!(report.rows, n);
        let mismatched: i64 = conn
            .query_row(
                "SELECT count(*) FILTER (WHERE id <> ? - __magi_row + 1) FROM __magi_typed_s",
                [n as i64],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mismatched, 0);
        // the relation is scanned in source-row order
        let mut stmt = conn.prepare("SELECT id FROM s").unwrap();
        let ids: Vec<i64> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(ids.iter().rev().copied().eq(1..=n as i64));
    }

    #[test]
    fn staging_sql_is_what_stage_runs() {
        let dir = tempfile::tempdir().unwrap();
        let (src, rel) = csv_source(dir.path(), "id,amount\n1,2.50\n2,x\n");
        let script = staging_sql(&src, &rel, &[]);
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch(&script).unwrap();
        let rejects: i64 = conn
            .query_row("SELECT count(*) FROM \"s.rejects\"", [], |r| r.get(0))
            .unwrap();
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM s", [], |r| r.get(0))
            .unwrap();
        assert_eq!((rejects, rows), (1, 2));
        assert!(script.contains("read_csv("), "{script}");
    }
}
