//! CSV sources. DuckDB reads the file; MAGI checks that DuckDB's sniffer found the table MAGI
//! promises to read and settles each column's type from every value, not from the sniffer's
//! sample:
//!
//! - With `section: N`, DuckDB reads a copy of the file's N-th run of non-blank lines (see
//!   [`CsvFile`]; blank lines include delimiter-only ones, quoting is DuckDB's, see [`Quoting`])
//!   with LF line breaks, and everything below applies to that section as to a whole file; line
//!   numbers in messages still count the file's lines, row numbers the section's data rows.
//! - The header is line 1 (or there is none: columns are then `column_1`.. `column_<n>`, as for
//!   Excel) and every line has the same number of fields (with `ragged: true`, at most as many
//!   as the header, or any number without one; missing fields are null). Anything else (a
//!   sniffer that skips leading lines, a single column named after a whole line, ragged rows,
//!   with or without a header) is an M212 error naming line numbers, never a silent guess; a
//!   file that blank lines split into sections gets them listed.
//! - `fill_down` columns are filled before the values are measured, as staging fills them.
//! - Plain decimals become `decimal(18, s)` or `decimal(38, s)`, never binary floats, so sums are
//!   exact and every run gives the same result; only exponent notation is read as `float`.
//! - Integers with leading zeros (`007`) and integers beyond 64 bits stay text, so codes and long
//!   identifiers keep every digit.
//! - Dates, times and timestamps are inferred only when every value is ISO; other layouts
//!   (`25/12/2026`) are inferred as text with a note naming the format to declare. Times with
//!   more than 6 decimal places of seconds stay text (DuckDB keeps microseconds).
//! - Booleans are inferred only when every value is one; a column the sniffer's sample showed as
//!   text (or only blanks) gets the type all its values share.
//!
//! Messages never contain values from the file; line and row numbers are fine.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::backend::sql::{self, Expr, Query, Select, TableRef, str_lit};
use crate::semantic::hir::CsvOptions;
use crate::semantic::schema::InferredSchema;
use crate::semantic::types::Type;
use crate::source::staged::{NoteLevel, SourceNote};
use crate::source::{FALSE_TEXT, SUB_MICROSECOND, SourceError, TRUE_TEXT};

/// Diagnostic code for a file that cannot be read as one rectangular table.
pub const SHAPE_ERROR: &str = "M212";
/// Diagnostic code for a `section:` the file does not have.
pub const SECTION_ERROR: &str = "M213";
/// How many line numbers a message lists before eliding the rest.
const MAX_LINES_LISTED: usize = 5;

/// Plain decimal text (`12`, `-0.50`, `+3.`, `.5`).
pub(crate) const PLAIN: &str = r"[+-]?(\d+(\.\d*)?|\.\d+)";
/// A plain number whose integer part has a leading zero (`007`, `00.5`), but not `0` or `0.5`.
const LEADING_ZERO: &str = r"[+-]?0\d+(\.\d*)?";
const ISO_DATE: &str = r"\d{4}-\d{2}-\d{2}";
const ISO_TIMESTAMP: &str = r"\d{4}-\d{2}-\d{2}([ T]\d{2}:\d{2}(:\d{2}(\.\d+)?)?)?";
const ISO_TIMESTAMP_TZ: &str =
    r"\d{4}-\d{2}-\d{2}([ T]\d{2}:\d{2}(:\d{2}(\.\d+)?)?)?\s*(Z|[+-]\d{2}(:?\d{2})?)?";
const ISO_TIME: &str = r"\d{2}:\d{2}(:\d{2}(\.\d+)?)?";

/// `read_csv(path, header = .., delim = .., all_varchar = ..)` with the user's options. `path` is
/// the file DuckDB reads (see [`CsvFile`]). `columns` are the source's columns, in file order,
/// given to DuckDB as `names`: MAGI names them (see [`headerless_names`] and [`header_names`]);
/// empty leaves DuckDB's own.
pub fn table(path: &Path, options: &CsvOptions, all_varchar: bool, columns: &[String]) -> TableRef {
    let mut t = table_fn("read_csv", path, options, all_varchar);
    if !columns.is_empty()
        && let TableRef::Func { named, .. } = &mut t
    {
        named.push((
            "names".into(),
            Expr::List(columns.iter().map(|c| str_lit(c)).collect()),
        ));
    }
    t
}

/// Column names of a CSV file without a header, as for Excel: `column_1` .. `column_<n>`.
fn headerless_names(n: usize) -> Vec<String> {
    (1..=n).map(|i| format!("column_{i}")).collect()
}

/// Column names of a CSV file with a header, as for Excel ([`super::header_names`]), with a
/// warning for each generated or renamed name. The header is read by DuckDB as the first row of
/// the file, so quoting applies.
fn header_names(
    conn: &duckdb::Connection,
    path: &Path,
    options: &CsvOptions,
    delim: u8,
    n: usize,
) -> Result<(Vec<String>, Vec<SourceNote>), String> {
    let raw = CsvOptions {
        header: false,
        delimiter: Some((delim as char).to_string()),
        ..options.clone()
    };
    let q = format!(
        "{} LIMIT 1",
        select_star(table_fn("read_csv", path, &raw, true))
    );
    let fields: Vec<Option<String>> = conn
        .query_row(&q, [], |r| (0..n).map(|i| r.get(i)).collect())
        .map_err(|e| e.to_string())?;
    let named = super::header_names(fields);
    let mut renamed: Vec<String> = Vec::new();
    for (i, first, base) in &named.renamed {
        renamed.push(format!(
            "duplicate header `{base}` in column {} (first in column {}) renamed `{}`",
            i + 1,
            first + 1,
            named.names[*i]
        ));
    }
    let blank: Vec<(usize, &str)> = named
        .blank
        .iter()
        .map(|&i| (i + 1, named.names[i].as_str()))
        .collect();
    let mut notes = Vec::new();
    let warn = |message: String| SourceNote {
        level: NoteLevel::Warning,
        message,
        type_hint_for: None,
    };
    if !blank.is_empty() {
        let columns: Vec<String> = blank.iter().map(|(c, _)| c.to_string()).collect();
        let generated: Vec<&str> = blank.iter().map(|(_, n)| *n).collect();
        notes.push(warn(format!(
            "blank header in {} {} named {}",
            if blank.len() == 1 {
                "column"
            } else {
                "columns"
            },
            columns.join(", "),
            generated.join(", ")
        )));
    }
    notes.extend(renamed.into_iter().map(warn));
    Ok((named.names, notes))
}

/// The reader call with the user's options. The quote character is always `"` and no line is a
/// comment: DuckDB's sniffer would otherwise choose them from a sample, so one malformed line
/// could switch quoting off for the whole file, `'` could become a quote, and data lines starting
/// with `#` (`#N/A`) could be dropped as comments. The escape (`"` or `\`) is still sniffed.
fn table_fn(name: &str, path: &Path, options: &CsvOptions, all_varchar: bool) -> TableRef {
    let mut named = vec![
        (
            "header".to_string(),
            Expr::Lit(sql::Lit::Bool(options.header)),
        ),
        ("quote".into(), str_lit("\"")),
        ("comment".into(), str_lit("")),
    ];
    if let Some(d) = &options.delimiter {
        named.push(("delim".into(), str_lit(d)));
    }
    if all_varchar {
        named.push(("all_varchar".into(), Expr::Lit(sql::Lit::Bool(true))));
    }
    if options.ragged {
        // DuckDB's parallel reader cannot pad lines when a quoted field spans lines
        named.push(("null_padding".into(), Expr::Lit(sql::Lit::Bool(true))));
        named.push(("parallel".into(), Expr::Lit(sql::Lit::Bool(false))));
    }
    TableRef::Func {
        name: name.into(),
        args: vec![str_lit(&super::literal_path(path))],
        named,
    }
}

fn select_star(from: TableRef) -> String {
    sql::render_query(&Query::select(Select {
        items: vec![(Expr::Star { table: None }, None)],
        from: Some((from, None)),
        ..Select::default()
    }))
}

/// What `sniff_csv` reports for the file read with the user's options.
struct Sniff {
    skip_rows: i64,
    /// The delimiter DuckDB splits lines on.
    delimiter: u8,
    date_format: Option<String>,
    timestamp_format: Option<String>,
}

fn sniff(conn: &duckdb::Connection, path: &Path, options: &CsvOptions) -> Result<Sniff, String> {
    let q = format!(
        "SELECT CAST(SkipRows AS BIGINT), CAST(Delimiter AS VARCHAR), CAST(DateFormat AS VARCHAR), CAST(TimestampFormat AS VARCHAR) FROM ({})",
        select_star(table_fn("sniff_csv", path, options, false))
    );
    conn.query_row(&q, [], |r| {
        Ok(Sniff {
            skip_rows: r.get(0)?,
            delimiter: r
                .get::<_, Option<String>>(1)?
                .and_then(|d| d.bytes().next())
                .unwrap_or(b','),
            date_format: r.get::<_, Option<String>>(2)?.filter(|f| !f.is_empty()),
            timestamp_format: r.get::<_, Option<String>>(3)?.filter(|f| !f.is_empty()),
        })
    })
    .map_err(|e| e.to_string())
}

/// Columns of a CSV source and notes about how their types were settled. `declared` gives the
/// type a column is declared with: the type reported for it is what its text converts to exactly
/// (so M203 is precise), and inference notes about it are left out.
pub fn infer(
    conn: &duckdb::Connection,
    file: &CsvFile,
    options: &CsvOptions,
    declared: &dyn Fn(&str) -> Option<Type>,
) -> Result<InferredSchema, SourceError> {
    if is_blank(&file.source).unwrap_or(false) {
        return Err(empty_file(&file.source, options));
    }
    let read = file.path();
    let guessed = delimiter(read, options);
    let sniffed = sniff(conn, read, options).map_err(|e| read_error(file, options, &e))?;
    if sniffed.skip_rows > 0 {
        let (first, n) = (file.line(1), sniffed.skip_rows as u64);
        return Err(shape_error(
            file,
            options,
            guessed,
            &format!(
                "DuckDB's CSV reader would skip {} and start the table at line {}",
                if n == 1 {
                    format!("line {first}")
                } else {
                    format!("lines {first}-{}", first + n - 1)
                },
                first + n
            ),
        ));
    }
    let mut columns = super::describe(
        conn,
        &select_star(table(read, options, options.all_text, &[])),
    )
    .map_err(|e| read_error(file, options, &e))?;
    if options.header
        && let [(name, _)] = columns.as_slice()
        && name.contains([',', ';', '|', '\t'])
    {
        if let Some(e) =
            stray_quote_error(file, guessed).or_else(|| spaced_lines_error(file, guessed))
        {
            return Err(e);
        }
        // lines the guessed delimiter splits differently are the likelier cause
        let reason = match ragged(read, guessed) {
            Ok(r) if r.total > 0 => "lines differ in their number of fields".to_string(),
            _ => format!(
                "no delimiter splits every line the same way, so line {} was read as a single column",
                file.line(1)
            ),
        };
        return Err(shape_error(file, options, guessed, &reason));
    }
    // DuckDB reads some ragged files without an error: one without a header as a single column of
    // whole lines, or lines with one extra empty field at the end.
    let delim = if columns.len() > 1 {
        sniffed.delimiter
    } else {
        guessed
    };
    if let Ok(r) = ragged(read, delim)
        && let Some(reason) = misshapen(&r, options, Some(columns.len()), file)
    {
        return Err(shape_error(file, options, delim, &reason));
    }
    // columns are named like Excel's: `column_1`.. without a header; with one, blank headers get
    // that name and repeated ones (ignoring case) `_2`, `_3`, ... Every later read (and staging)
    // passes these names to DuckDB, whose own would be `column0` and `amount_1`.
    let (given, mut notes) = if options.header {
        header_names(conn, read, options, delim, columns.len())
            .map_err(|e| read_error(file, options, &e))?
    } else {
        (headerless_names(columns.len()), Vec::new())
    };
    for ((name, _), given) in columns.iter_mut().zip(given) {
        *name = given;
    }
    let names: Vec<String> = columns.iter().map(|(n, _)| n.clone()).collect();
    let probes: Vec<Probe> = columns
        .iter()
        .filter_map(|(name, ty)| Probe::for_column(name, *ty, declared(name), options.all_text))
        .collect();
    let counts =
        scan(conn, read, options, &names, &probes).map_err(|e| read_error(file, options, &e))?;
    for (probe, c) in probes.iter().zip(&counts) {
        let (ty, finding) = probe.settle(c, &sniffed);
        if let Some(f) = finding.filter(|_| probe.declared.is_none() && probe.kind != Kind::Text) {
            let first = match probe.condition(&f) {
                Some(cond) => first_row(conn, read, options, &names, &cond)
                    .map_err(|e| read_error(file, options, &e))?,
                None => None,
            };
            notes.push(probe.note(&f, first));
        }
        if let Some(column) = columns.iter_mut().find(|(n, _)| *n == probe.column) {
            column.1 = ty;
        }
    }
    if let Some(message) = file.continuation_warning() {
        notes.push(SourceNote {
            level: NoteLevel::Warning,
            message: message.to_string(),
            type_hint_for: None,
        });
    }
    Ok(InferredSchema { columns, notes })
}

/// The file DuckDB reads for a CSV source: the source's file, or a scratch copy of the section
/// `section:` selects (made once per process in the [`super::Scratch`] directory, which removes
/// it). Line numbers in messages always refer to the source's file, and messages never name the
/// scratch copy.
#[derive(Debug, Clone)]
pub struct CsvFile {
    source: PathBuf,
    read: PathBuf,
    /// Lines of the source's file before the part DuckDB reads.
    offset: u64,
    section: Option<u32>,
    /// The next section, when it has the table's number of fields: the table may continue there.
    continues: Option<String>,
}

impl CsvFile {
    /// The whole file.
    pub fn whole(path: &Path) -> CsvFile {
        CsvFile {
            source: path.to_path_buf(),
            read: path.to_path_buf(),
            offset: 0,
            section: None,
            continues: None,
        }
    }

    /// Section `n` of the file, copied to `target` with its line endings made LF (outside quoted
    /// fields: DuckDB cannot read a file that mixes CRLF and LF lines).
    pub fn section(
        path: &Path,
        options: &CsvOptions,
        n: u32,
        target: &Path,
    ) -> Result<CsvFile, SourceError> {
        let cannot = |e: std::io::Error| {
            SourceError::from(format!("cannot read `{}`: {e}", file_name(path)))
        };
        let delim = delimiter(path, options);
        let (all, backslash_quote) = sections_with(path, delim, false).map_err(cannot)?;
        if all.is_empty() {
            return Err(empty_file(path, options));
        }
        let i = (n as usize).checked_sub(1);
        let Some(section) = i.and_then(|i| all.get(i)) else {
            return Err(missing_section(path, n, &all, delim));
        };
        // `\"` in a quoted field: whether `\` escapes the quote may change where the section ends
        if backslash_quote {
            let (escaped, _) = sections_with(path, delim, true).map_err(cannot)?;
            let other = i.and_then(|i| escaped.get(i)).map(|s| s.lines);
            if other != Some(section.lines) {
                return Err(ambiguous_escape(path, n, section, other));
            }
        }
        copy_normalized(path, target, section.bytes, delim).map_err(|e| {
            let _ = std::fs::remove_file(target);
            SourceError::from(format!(
                "cannot copy section {n} of `{}` to a scratch file: {e}",
                file_name(path)
            ))
        })?;
        let width = section.first_fields;
        let continues = all
            .get(n as usize)
            .filter(|next| next.fields == (width, width))
            .map(|next| {
                format!(
                    "section {} ({}) has the table's {width} {}, so the table may continue there: a blank line or a line of only delimiters ends section {n} before it — remove that line, or read section {} as a source of its own",
                    n + 1,
                    next.line_range(),
                    if width == 1 { "field" } else { "fields" },
                    n + 1
                )
            });
        Ok(CsvFile {
            source: path.to_path_buf(),
            read: target.to_path_buf(),
            offset: section.lines.0 - 1,
            section: Some(n),
            continues,
        })
    }

    /// A warning about the lines read: the table may continue past the section.
    pub fn continuation_warning(&self) -> Option<&str> {
        self.continues.as_deref()
    }

    /// The file DuckDB reads.
    pub fn path(&self) -> &Path {
        &self.read
    }

    /// The source file's line number of line `n` of the file DuckDB reads.
    fn line(&self, n: u64) -> u64 {
        n + self.offset
    }

    /// First line of a DuckDB error (the rest may quote file contents), with the line number it
    /// names counted in the source's file and the scratch copy named as the section it holds.
    pub fn error_line(&self, error: &str) -> String {
        let mut first = first_line(error);
        if let Some(n) = self.section {
            let shown = format!("section {n} of `{}`", file_name(&self.source));
            for p in [
                self.read.display().to_string(),
                super::literal_path(&self.read),
            ] {
                first = first
                    .replace(&format!("\"{p}\""), &shown)
                    .replace(&format!("'{p}'"), &shown)
                    .replace(&p, &shown);
            }
        }
        const AT: &str = "Line: ";
        let Some(i) = first.find(AT).map(|i| i + AT.len()) else {
            return first;
        };
        let digits = first[i..].bytes().take_while(u8::is_ascii_digit).count();
        match first[i..i + digits].parse::<u64>() {
            Ok(n) if self.offset > 0 => {
                format!("{}{}{}", &first[..i], self.line(n), &first[i + digits..])
            }
            _ => first,
        }
    }
}

/// Copy bytes `from..to` of `source` into a new file `target`, writing each line break outside a
/// quoted field (LF, CRLF or CR) as LF; breaks inside quoted fields are kept as they are.
fn copy_normalized(
    source: &Path,
    target: &Path,
    (from, to): (u64, u64),
    delim: u8,
) -> std::io::Result<()> {
    let mut file = File::open(source)?;
    file.seek(SeekFrom::Start(from))?;
    let mut reader = BufReader::new(file.take(to - from));
    let mut writer = std::io::BufWriter::new(super::private_file(target)?);
    // a section starts at the start of a record, outside quotes
    let mut q = Quoting::new(false);
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            break;
        }
        for &b in buf {
            match q.feed(b, delim) {
                Byte::Newline => writer.write_all(b"\n")?,
                Byte::BreakTail => {}
                _ => writer.write_all(&[b])?,
            }
        }
        let n = buf.len();
        reader.consume(n);
    }
    writer.flush()
}

/// The M200 error for a file with nothing to read.
fn empty_file(path: &Path, options: &CsvOptions) -> SourceError {
    SourceError::from(format!(
        "`{}` is empty: a CSV source needs {}",
        file_name(path),
        if options.header {
            "a header line"
        } else {
            "at least one line"
        }
    ))
}

/// The M213 error for a section the file does not have (it has at least one).
fn missing_section(path: &Path, n: u32, sections: &[Section], delim: u8) -> SourceError {
    let k = sections.len();
    SourceError {
        code: SECTION_ERROR,
        message: format!(
            "`{}` has {k} {} (fields split at {}), so there is no section {n}: {}",
            file_name(path),
            if k == 1 { "section" } else { "sections" },
            delimiter_name(delim),
            list_sections(sections)
        ),
        help: Some(format!(
            "{SECTION_HELP}; if the fields are split at another character, declare it with `delimiter:`"
        )),
    }
}

/// The M212 error for a section whose end depends on whether `\` escapes quotes: `lines` is where
/// it ends with `""` escapes (RFC 4180), `other` where it ends with `\"` escapes.
fn ambiguous_escape(
    path: &Path,
    n: u32,
    lines: &Section,
    other: Option<(u64, u64)>,
) -> SourceError {
    let other = match other {
        Some((a, b)) if a == b => format!("line {a}"),
        Some((a, b)) => format!("lines {a}-{b}"),
        None => "missing".to_string(),
    };
    SourceError {
        code: SHAPE_ERROR,
        message: format!(
            "cannot tell where section {n} of `{}` ends: a quoted field holds `\\\"`; if `\"\"` escapes a quote (RFC 4180) section {n} is {}, if `\\\"` does it is {other}",
            file_name(path),
            lines.line_range()
        ),
        help: Some(
            "`section:` needs one way of quoting: write a quote inside a quoted field as `\"\"`, or remove the blank lines inside quoted fields".into(),
        ),
    }
}

/// What a section is, for messages that list them.
const SECTION_HELP: &str = "a section is a run of non-blank lines, numbered from 1; lines that are empty or hold only whitespace or only delimiters (`,,,,`) separate sections";

/// `section 1: lines 1-4 (1-2 fields), section 2: lines 6-1194 (8 fields), ...`
fn list_sections(sections: &[Section]) -> String {
    sections
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let fields = match s.fields {
                (1, 1) => "1 field".to_string(),
                (a, b) if a == b => format!("{a} fields"),
                (a, b) => format!("{a}-{b} fields"),
            };
            format!("section {}: {} ({fields})", i + 1, s.line_range())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether the file holds nothing but whitespace (and a byte order mark).
fn is_blank(path: &Path) -> std::io::Result<bool> {
    let mut reader = BufReader::new(File::open(path)?);
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Ok(true);
        }
        if buf
            .iter()
            .any(|b| !b.is_ascii_whitespace() && ![0xEF, 0xBB, 0xBF].contains(b))
        {
            return Ok(false);
        }
        let n = buf.len();
        reader.consume(n);
    }
}

/// A column whose type is checked against every value.
struct Probe {
    column: String,
    sniffed: Type,
    /// The type the program declares the column with.
    declared: Option<Type>,
    kind: Kind,
}

/// What a probe measures, after the type DuckDB's sniffer found in its sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Number,
    Temporal,
    Bool,
    /// Text in the sample (or only blanks): the whole column may still hold one type.
    Text,
}

/// Aggregates of one full scan over a probed column (see [`Probe::aggregates`]).
type Counts = Vec<Option<i64>>;

/// Why an undeclared column is not given the type its values seem to have.
enum Finding {
    /// Dates, times or timestamps not written in ISO form, and the sniffer's format if it found
    /// one.
    NotIso {
        count: i64,
        format: Option<String>,
    },
    /// Times with non-zero digits beyond microseconds.
    SubMicrosecond(i64),
    NotBool(i64),
    NotNumbers(i64),
    LeadingZeros(i64),
    TooBigForInt(i64),
    TooManyDigits,
}

/// Number aggregates come first in every numeric and text probe.
const NUMBER_AGGREGATES: usize = 8;

impl Probe {
    /// Every column the sniffer typed from its sample. Text columns too (a sample of blanks or of
    /// a few odd values says nothing about the rest), unless the column is read as text anyway.
    fn for_column(
        name: &str,
        sniffed: Type,
        declared: Option<Type>,
        all_text: bool,
    ) -> Option<Probe> {
        let kind = match sniffed {
            Type::Int | Type::Decimal(..) | Type::Float => Kind::Number,
            Type::Date | Type::Timestamp | Type::TimestampTz | Type::Time => Kind::Temporal,
            Type::Bool => Kind::Bool,
            Type::String => match declared {
                Some(Type::String) => return None,
                None if all_text => return None,
                _ => Kind::Text,
            },
            _ => return None,
        };
        Some(Probe {
            column: name.to_string(),
            sniffed,
            declared,
            kind,
        })
    }

    /// The column's value: its text without surrounding whitespace, null when blank (as staging
    /// reads it, see [`super::WHITESPACE`]).
    fn value(&self) -> String {
        format!(
            "nullif({}, '')",
            super::trimmed_sql(&sql::ident(&self.column))
        )
    }

    fn plain(&self) -> String {
        format!("regexp_full_match({}, '{PLAIN}')", self.value())
    }

    fn iso(ty: Type) -> &'static str {
        match ty {
            Type::Date => ISO_DATE,
            Type::Timestamp => ISO_TIMESTAMP,
            Type::Time => ISO_TIME,
            _ => ISO_TIMESTAMP_TZ,
        }
    }

    /// Condition on value `v` that holds unless it is ISO text of `ty` that is also a value of
    /// `ty`: `27:15` looks like a time and `2024-02-30` like a date, but neither is one.
    fn not_iso(v: &str, ty: Type) -> String {
        let sql_type = match ty {
            Type::Date => "DATE",
            Type::Timestamp => "TIMESTAMP",
            Type::Time => "TIME",
            _ => "TIMESTAMPTZ",
        };
        format!(
            "NOT (regexp_full_match({v}, '{}') AND TRY_CAST({v} AS {sql_type}) IS NOT NULL)",
            Self::iso(ty)
        )
    }

    /// Condition on the column's value that holds for the rows a finding is about (`None`: no
    /// single row shows it).
    fn condition(&self, finding: &Finding) -> Option<String> {
        let (v, plain) = (self.value(), self.plain());
        Some(match finding {
            Finding::NotIso { .. } => Self::not_iso(&v, self.sniffed),
            Finding::SubMicrosecond(_) => format!("regexp_matches({v}, '{SUB_MICROSECOND}')"),
            Finding::NotBool(_) => {
                let words: Vec<String> = TRUE_TEXT
                    .iter()
                    .chain(&FALSE_TEXT)
                    .map(|w| format!("'{w}'"))
                    .collect();
                format!("lower({v}) NOT IN ({})", words.join(", "))
            }
            Finding::NotNumbers(_) => format!("NOT {plain} AND TRY_CAST({v} AS DOUBLE) IS NULL"),
            Finding::LeadingZeros(_) => format!("regexp_full_match({v}, '{LEADING_ZERO}')"),
            Finding::TooBigForInt(_) => {
                format!("{plain} AND NOT contains({v}, '.') AND TRY_CAST({v} AS BIGINT) IS NULL")
            }
            Finding::TooManyDigits => return None,
        })
    }

    /// Aggregates over the column's text, in the order [`Probe::settle`] reads them.
    fn aggregates(&self) -> Vec<String> {
        let v = self.value();
        let count_where = |cond: String| format!("count({v}) FILTER (WHERE {cond})");
        let count = |f: Finding| count_where(self.condition(&f).expect("a row condition"));
        let not_iso = || Finding::NotIso {
            count: 0,
            format: None,
        };
        match self.kind {
            Kind::Temporal => vec![count(not_iso()), count(Finding::SubMicrosecond(0))],
            Kind::Bool => vec![count(Finding::NotBool(0))],
            Kind::Number => self.number_aggregates(),
            Kind::Text => {
                let mut out = self.number_aggregates();
                out.push(format!("count({v})"));
                out.push(count(Finding::NotBool(0)));
                for ty in [Type::Date, Type::Timestamp, Type::TimestampTz, Type::Time] {
                    out.push(count_where(Self::not_iso(&v, ty)));
                }
                out.push(count(Finding::SubMicrosecond(0)));
                out
            }
        }
    }

    /// [`NUMBER_AGGREGATES`] aggregates, read by [`Probe::settle_number`].
    fn number_aggregates(&self) -> Vec<String> {
        let (v, plain) = (self.value(), self.plain());
        let count = |f: Finding| {
            let cond = self.condition(&f).expect("findings with a row condition");
            format!("count({v}) FILTER (WHERE {cond})")
        };
        let fraction = format!("regexp_extract({v}, '\\.(\\d*)$', 1)");
        vec![
            count(Finding::NotNumbers(0)),
            format!("count({v}) FILTER (WHERE NOT {plain})"),
            count(Finding::LeadingZeros(0)),
            format!("count({v}) FILTER (WHERE {plain} AND contains({v}, '.'))"),
            format!("max(length(regexp_extract({v}, '^[+-]?0*(\\d*)', 1))) FILTER (WHERE {plain})"),
            format!("max(length({fraction})) FILTER (WHERE {plain})"),
            count(Finding::TooBigForInt(0)),
            // scale without trailing zeros: `12.500` fits a scale of 1
            format!("max(length(rtrim({fraction}, '0'))) FILTER (WHERE {plain})"),
        ]
    }

    /// The column's type from its scan results, and why it is not the obvious one.
    fn settle(&self, c: &Counts, sniffed: &Sniff) -> (Type, Option<Finding>) {
        let n = |i: usize| c[i].unwrap_or(0);
        match self.kind {
            Kind::Temporal if n(0) > 0 => {
                let format = match self.sniffed {
                    Type::Date => sniffed.date_format.clone(),
                    Type::Timestamp => sniffed.timestamp_format.clone(),
                    _ => None,
                };
                let format = format.filter(|f| !f.starts_with("%Y-%m-%d"));
                (
                    Type::String,
                    Some(Finding::NotIso {
                        count: n(0),
                        format,
                    }),
                )
            }
            Kind::Temporal if n(1) > 0 => (Type::String, Some(Finding::SubMicrosecond(n(1)))),
            Kind::Temporal => (self.sniffed, None),
            Kind::Bool if n(0) > 0 => (Type::String, Some(Finding::NotBool(n(0)))),
            Kind::Bool => (Type::Bool, None),
            Kind::Number => self.settle_number(c),
            Kind::Text => {
                let t = |i: usize| n(NUMBER_AGGREGATES + i);
                let (present, not_bool, sub_micro) = (t(0), t(1), t(6));
                // plain numbers only: exponents, `nan` and `inf` in a text column stay text
                let ty = if present == 0 {
                    Type::String
                } else if n(0) == 0 && n(1) == 0 {
                    self.settle_number(c).0
                } else if not_bool == 0 {
                    Type::Bool
                } else if t(2) == 0 {
                    Type::Date
                } else if sub_micro > 0 {
                    Type::String
                } else if t(3) == 0 {
                    Type::Timestamp
                } else if t(4) == 0 {
                    Type::TimestampTz
                } else if t(5) == 0 {
                    Type::Time
                } else {
                    Type::String
                };
                (ty, None)
            }
        }
    }

    /// A number column's type. A declared column gets the type its text converts to exactly:
    /// codes with leading zeros are numbers, long integers are decimals, only significant decimal
    /// places count, and a declared decimal learns how many digits the values need (M203 when
    /// they do not fit).
    fn settle_number(&self, c: &Counts) -> (Type, Option<Finding>) {
        let n = |i: usize| c[i].unwrap_or(0);
        let declared = self.declared.is_some();
        let (not_number, non_plain, leading, big) = (n(0), n(1), n(2), n(6));
        // an inferred decimal keeps the scale as written, so `12.50` stays `12.50`
        let (fractional, scale) = if declared {
            (n(7) > 0, n(7))
        } else {
            (n(3) > 0, n(5))
        };
        let int_digits = n(4);
        if not_number > 0 {
            return (Type::String, Some(Finding::NotNumbers(not_number)));
        }
        if leading > 0 && !declared {
            return (Type::String, Some(Finding::LeadingZeros(leading)));
        }
        if non_plain > 0 {
            return (Type::Float, None);
        }
        if let Some(Type::Decimal(..)) = self.declared {
            return match int_digits + scale {
                // both bounds were checked: the conversions cannot fail
                digits @ ..=38 => (Type::Decimal(digits.max(1) as u8, scale as u8), None),
                _ => (Type::String, Some(Finding::TooManyDigits)),
            };
        }
        if !fractional {
            return match (big, int_digits) {
                (0, _) => (Type::Int, None),
                (_, ..=38) if declared => (Type::Decimal(38, 0), None),
                (_, ..=38) => (Type::String, Some(Finding::TooBigForInt(big))),
                _ => (Type::String, Some(Finding::TooManyDigits)),
            };
        }
        match int_digits + scale {
            // both bounds were checked: the conversions cannot fail
            0..=18 => (Type::Decimal(18, scale as u8), None),
            19..=38 => (Type::Decimal(38, scale as u8), None),
            _ => (Type::String, Some(Finding::TooManyDigits)),
        }
    }

    /// The note for a finding; `first` is the first source row it concerns.
    fn note(&self, finding: &Finding, first: Option<i64>) -> SourceNote {
        let name = &self.column;
        let at = first.map_or_else(String::new, |r| format!(" (first at source row {r})"));
        let (level, message) = match finding {
            Finding::NotIso { count, format } => {
                let word = match self.sniffed {
                    Type::Date => "date",
                    Type::Timestamp => "timestamp",
                    Type::Time => "time",
                    _ => "timestamp_tz",
                };
                let message = match format {
                    Some(f) => format!(
                        "column {name}: {word}s are written as `{f}`, not ISO (YYYY-MM-DD); inferred string — declare `{name}: {word}(\"{f}\")` to read them as {word}s"
                    ),
                    None => format!(
                        "column {name}: {} not ISO {word}s{at}; inferred string — declare `{name}: {word}(\"<format>\")` to read them as {word}s",
                        count_are(*count, "value")
                    ),
                };
                (NoteLevel::Note, message)
            }
            Finding::SubMicrosecond(n) => (
                NoteLevel::Note,
                format!(
                    "column {name}: {} written with more than 6 decimal places of seconds{at}; read as text so no digit is lost (MAGI's times keep microseconds)",
                    count_are(*n, "value")
                ),
            ),
            Finding::NotBool(n) => (
                NoteLevel::Warning,
                format!(
                    "column {name}: {} not booleans (true/false, yes/no, t/f, y/n, 1/0){at}; inferred string — declare `{name}: bool` to read the rest as bools and list these in `.rejects`",
                    count_are(*n, "value")
                ),
            ),
            Finding::NotNumbers(n) => (
                NoteLevel::Warning,
                format!(
                    "column {name}: {} not numbers{at}; inferred string — declare the column's type to read the numbers and list the rest in `.rejects`",
                    count_are(*n, "value")
                ),
            ),
            Finding::LeadingZeros(n) => (
                NoteLevel::Note,
                format!(
                    "column {name}: {} written with leading zeros{at}; read as text so codes keep every digit — declare a numeric type to read them as numbers",
                    count_are(*n, "number")
                ),
            ),
            Finding::TooBigForInt(n) => (
                NoteLevel::Note,
                format!(
                    "column {name}: {} too large for a 64-bit int{at}; read as text so long identifiers keep every digit",
                    count_are(*n, "integer")
                ),
            ),
            Finding::TooManyDigits => (
                NoteLevel::Note,
                format!(
                    "column {name}: numbers need more than 38 digits; read as text so no digit is lost"
                ),
            ),
        };
        SourceNote {
            level,
            message,
            type_hint_for: Some(name.clone()),
        }
    }
}

fn count_are(n: i64, what: &str) -> String {
    if n == 1 {
        format!("1 {what} is")
    } else {
        format!("{n} {what}s are")
    }
}

/// One pass over every value of the file (as text). Also proves that DuckDB can read the whole
/// file, not only the sniffer's sample. Returns the aggregates of each probe. `columns` are the
/// source's columns (see [`table`]; `fill_down` needs them too).
fn scan(
    conn: &duckdb::Connection,
    path: &Path,
    options: &CsvOptions,
    columns: &[String],
    probes: &[Probe],
) -> Result<Vec<Counts>, String> {
    let per_probe: Vec<Vec<String>> = probes.iter().map(Probe::aggregates).collect();
    let mut items = vec!["count(*)".to_string()];
    items.extend(per_probe.iter().flatten().cloned());
    let rows = if options.fill_down.is_empty() {
        select_star(table(path, options, true, columns))
    } else {
        text_rows(path, options, columns)
    };
    let q = format!("SELECT {} FROM ({rows})", items.join(", "));
    conn.query_row(&q, [], |r| {
        let mut i = 1;
        let mut out = Vec::with_capacity(per_probe.len());
        for aggs in &per_probe {
            let mut counts = Vec::with_capacity(aggs.len());
            for _ in aggs {
                counts.push(r.get::<_, Option<i64>>(i)?);
                i += 1;
            }
            out.push(counts);
        }
        Ok(out)
    })
    .map_err(|e| e.to_string())
}

/// The file's rows as text, numbered in [`super::ROW`], with the `fill_down` columns filled: the
/// rows staging reads.
fn text_rows(path: &Path, options: &CsvOptions, columns: &[String]) -> String {
    sql::render_query(&super::filled(
        super::with_row(table(path, options, true, columns)),
        columns,
        &options.fill_down,
    ))
}

/// First source row (1-based data row, as in `<source>.rejects`) where `condition` holds. Row
/// numbering needs a sequential pass, so it is only computed for a note that cites a row.
fn first_row(
    conn: &duckdb::Connection,
    path: &Path,
    options: &CsvOptions,
    columns: &[String],
    condition: &str,
) -> Result<Option<i64>, String> {
    let q = format!(
        "SELECT min({}) FROM ({}) WHERE {condition}",
        sql::ident(super::ROW),
        text_rows(path, options, columns)
    );
    conn.query_row(&q, [], |r| r.get(0))
        .map_err(|e| e.to_string())
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |f| f.to_string_lossy().into_owned(),
    )
}

/// Why the file (or section) DuckDB reads is not one table: lines whose number of fields differs
/// from the first line's (with `ragged: true`, lines with more fields than the header, or a widest
/// line DuckDB did not read as columns; `columns` is how many DuckDB found, when known).
fn misshapen(
    r: &Ragged,
    options: &CsvOptions,
    columns: Option<usize>,
    file: &CsvFile,
) -> Option<String> {
    if !options.ragged {
        return (r.total > 0).then(|| "lines differ in their number of fields".to_string());
    }
    if options.header {
        return (r.wider > 0).then(|| "some lines have more fields than the header".to_string());
    }
    let n = columns?;
    (r.first_line > 0 && r.widest.1 != n).then(|| {
        format!(
            "DuckDB's CSV reader found {n} {} but line {} has {} fields",
            if n == 1 { "column" } else { "columns" },
            file.line(r.widest.0),
            r.widest.1
        )
    })
}

/// The M212 error: what went wrong, the lines whose field count is wrong, and how to tell MAGI
/// what the file looks like. A whole file that blank lines split into sections of different
/// shapes (a preamble, a table, a trailer) is explained by its sections instead, and `section:`
/// suggested; sections that all start with the table's number of fields are one table with a
/// blank or delimiter-only line in it, so its misshapen lines are named as for any file.
fn shape_error(file: &CsvFile, options: &CsvOptions, delim: u8, reason: &str) -> SourceError {
    if file.section.is_none()
        && let Ok(all) = sections(&file.source, delim)
        && all.len() > 1
    {
        // the most lines, the first of equals
        let (i, largest) = all
            .iter()
            .enumerate()
            .max_by_key(|(i, s)| (s.lines.1 - s.lines.0, std::cmp::Reverse(*i)))
            .expect("several sections");
        let width = largest.first_fields;
        if all.iter().any(|s| s.first_fields != width) {
            return SourceError {
                code: SHAPE_ERROR,
                message: format!(
                    "could not read `{}` as one table: blank lines split it into {} sections (fields split at {}): {}",
                    file_name(&file.source),
                    all.len(),
                    delimiter_name(delim),
                    list_sections(&all)
                ),
                help: Some(format!(
                    "MAGI reads one table per source: set `section: {}` to read {} (the largest section), and declare a source per section you need ({SECTION_HELP})",
                    i + 1,
                    largest.line_range()
                )),
            };
        }
    }
    let mut message = format!(
        "could not read `{}` as a table: {reason}",
        file_name(&file.source)
    );
    let r = ragged(file.path(), delim).ok();
    if let Some(r) = &r {
        let (what, lines, total) = if options.ragged {
            ("more fields than", &r.wider_lines, r.wider)
        } else {
            ("a different number of fields than", &r.lines, r.total)
        };
        if total > 0 {
            let listed: Vec<String> = lines.iter().map(|&l| file.line(l).to_string()).collect();
            message += &format!(
                "; {} {what} line {} ({} {}): {} {}{}",
                if total == 1 {
                    "1 line has".to_string()
                } else {
                    format!("{total} lines have")
                },
                file.line(r.first_line),
                r.fields,
                if r.fields == 1 { "field" } else { "fields" },
                if total == 1 { "line" } else { "lines" },
                listed.join(", "),
                if total > listed.len() as u64 {
                    ", ..."
                } else {
                    ""
                },
            );
        }
    }
    let shown = match delim {
        b'\t' => "\\t".to_string(),
        d => (d as char).to_string(),
    };
    let fix = format!(
        "fix the lines listed, or set `delimiter: \"{shown}\"` (or the file's real separator)"
    );
    let help = if options.ragged {
        format!(
            "with `ragged: true`, lines may have fewer fields than {} but not more: {fix}",
            if options.header {
                "the header"
            } else {
                "the widest line"
            }
        )
    } else {
        let header = if options.header {
            format!("MAGI takes line {} as the header and needs", file.line(1))
        } else {
            "MAGI needs".to_string()
        };
        let mut help = format!("{header} the same number of fields on every line: {fix}");
        // every line that differs is short: padding them may be what the file means
        if r.is_some_and(|r| r.total > 0 && r.wider == 0) {
            help += ", or set `ragged: true` to read the missing fields as null";
        }
        help
    };
    SourceError {
        code: SHAPE_ERROR,
        message,
        help: Some(help),
    }
}

/// A DuckDB failure while reading the file. DuckDB's CSV errors quote the offending line, so they
/// are never passed on: ragged files become an M212 error, anything else keeps only the first line
/// of DuckDB's message (which names the problem and a line number).
fn read_error(file: &CsvFile, options: &CsvOptions, error: &str) -> SourceError {
    let delim = delimiter(file.path(), options);
    if let Some(e) = stray_quote_error(file, delim).or_else(|| spaced_lines_error(file, delim)) {
        return e;
    }
    if let Ok(r) = ragged(file.path(), delim)
        && let Some(reason) = misshapen(&r, options, None, file)
    {
        return shape_error(file, options, delim, &reason);
    }
    let mut e = SourceError::from(file.error_line(error));
    // DuckDB's sniffer gives up on a file whose lines end in more than one way
    if let Ok(endings) = line_endings(file.path(), delim)
        && let Some(mixed) = endings.mixed()
    {
        let first = e.message.trim_end_matches('.').to_string();
        e.message = format!(
            "{first}; `{}` mixes line breaks: {mixed}",
            file_name(&file.source)
        );
        e.help = Some(
            "DuckDB's CSV reader needs one kind of line break: convert the file to CRLF, LF or CR (MAGI converts the line breaks of a `section:` it reads)".into(),
        );
    }
    e
}

/// First line of a DuckDB error; the rest may quote file contents.
pub fn first_line(error: &str) -> String {
    error.lines().next().unwrap_or_default().trim().to_string()
}

/// The M212 error for lines holding only whitespace (`   `) in a file read whole: DuckDB reads
/// such a line as a record of one field, while an empty line is skipped. `None` when there is
/// none (a `section:` never holds one: blank lines end sections).
fn spaced_lines_error(file: &CsvFile, delim: u8) -> Option<SourceError> {
    let (mut lines, mut total) = (Vec::new(), 0u64);
    records(file.path(), delim, |l| {
        if let Line::Blank {
            line, spaced: true, ..
        } = l
        {
            total += 1;
            if lines.len() < MAX_LINES_LISTED {
                lines.push(file.line(line).to_string());
            }
        }
    })
    .ok()?;
    if total == 0 {
        return None;
    }
    Some(SourceError {
        code: SHAPE_ERROR,
        message: format!(
            "could not read `{}` as a table: {} only whitespace: {} {}{}",
            file_name(&file.source),
            if total == 1 { "1 line holds" } else { "lines hold" },
            if total == 1 { "line" } else { "lines" },
            lines.join(", "),
            if total > lines.len() as u64 { ", ..." } else { "" }
        ),
        help: Some(
            "empty the line (an empty line is skipped) or remove it, or read the part of the file you need with `section:`".into(),
        ),
    })
}

/// The M212 error for a quoted field whose closing `"` is followed by more text before the next
/// delimiter (`"12"" pipe"x`), which CSV readers split in different ways, when neither `""` nor
/// `\"` escapes explain it; `None` when the file has no such field.
fn stray_quote_error(file: &CsvFile, delim: u8) -> Option<SourceError> {
    let stray = |backslash| stray_quote(file.path(), delim, backslash).ok().flatten();
    let line = stray(false)?;
    stray(true)?;
    Some(SourceError {
        code: SHAPE_ERROR,
        message: format!(
            "could not read `{}` as a table: on line {}, a quoted field goes on after its closing quote",
            file_name(&file.source),
            file.line(line)
        ),
        help: Some(
            "inside a quoted field write a quote as `\"\"` (`\"12\"\" pipe\"`), and end the field at its closing quote".into(),
        ),
    })
}

/// The first line (1-based) holding a quoted field whose closing `"` is followed by something
/// other than the delimiter, whitespace or a line break, read with [`Quoting`].
fn stray_quote(path: &Path, delim: u8, backslash: bool) -> std::io::Result<Option<u64>> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut q = Quoting::new(backslash);
    let (mut line, mut prev) = (1u64, 0u8);
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Ok(None);
        }
        for &b in buf {
            let closing = q.closing;
            q.feed(b, delim);
            if closing && b != b'"' && b != delim && !b.is_ascii_whitespace() {
                return Ok(Some(line));
            }
            if b == b'\r' || (b == b'\n' && prev != b'\r') {
                line += 1;
            }
            prev = b;
        }
        let n = buf.len();
        reader.consume(n);
    }
}

/// How many bytes at the start of a file the delimiter is guessed from.
const GUESS_BYTES: u64 = 1 << 20;

/// The declared delimiter, else the one of `,` `;` tab `|` that splits the most lines of the
/// file's start into the same number of fields (more than one); ties go to the earlier one. A
/// title or preamble line with stray commas does not outvote the table below it.
fn delimiter(path: &Path, options: &CsvOptions) -> u8 {
    if let Some(d) = &options.delimiter {
        return match d.as_str() {
            "\\t" => b'\t',
            d => d.bytes().next().unwrap_or(b','),
        };
    }
    let mut head = Vec::new();
    if File::open(path)
        .and_then(|f| f.take(GUESS_BYTES).read_to_end(&mut head))
        .is_err()
    {
        return b',';
    }
    let mut best = (0usize, b',');
    for d in b",;\t|".iter().copied() {
        let mut counts: std::collections::HashMap<usize, usize> = Default::default();
        let _ = records_in(head.as_slice(), d, false, |l| {
            if let Line::Record {
                fields,
                empty: false,
                ..
            } = l
                && fields > 1
            {
                *counts.entry(fields).or_default() += 1;
            }
        });
        let score = counts.values().copied().max().unwrap_or(0);
        if score > best.0 {
            best = (score, d);
        }
    }
    best.1
}

/// `,` or `tab`, for messages.
fn delimiter_name(d: u8) -> String {
    match d {
        b'\t' => "tab".to_string(),
        d => format!("`{}`", d as char),
    }
}

/// What a byte of a CSV file is, by [`Quoting::feed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Byte {
    /// A line break outside quoted fields (`\n`, or `\r`): the end of a record.
    Newline,
    /// The `\n` of a CRLF break outside quoted fields (its `\r` was the [`Byte::Newline`]).
    BreakTail,
    /// A field separator.
    Delimiter,
    /// Whitespace outside quoted fields.
    Space,
    /// Part of a value (an opening quote included).
    Value,
    /// Inside a quoted field (line breaks included).
    Quoted,
}

/// Quoting as DuckDB reads it (RFC 4180): `"` opens a quoted field only as the first non-blank
/// byte of a field (`PIPE 12" STEEL` is plain text); inside, `""` is a quote and `"` followed by
/// anything else ends the quoted part, so a quoted field may span lines. Outside quotes, LF, CRLF
/// and a lone CR (classic Mac files) each end a line. With `backslash`, `\` inside a quoted field
/// escapes the next byte (`\"` is a quote), as in files DuckDB reads with `escape = '\'`.
struct Quoting {
    backslash: bool,
    quoted: bool,
    /// A `"` was read inside a quoted field: the next byte decides whether it ends it.
    closing: bool,
    field_start: bool,
    /// The previous byte was a `\r` line break: a `\n` now completes it.
    after_cr: bool,
    /// The previous byte inside a quoted field was `\` (escaping the next one with `backslash`).
    after_backslash: bool,
    /// A `\"` was read inside a quoted field.
    saw_backslash_quote: bool,
}

impl Quoting {
    fn new(backslash: bool) -> Quoting {
        Quoting {
            backslash,
            quoted: false,
            closing: false,
            field_start: true,
            after_cr: false,
            after_backslash: false,
            saw_backslash_quote: false,
        }
    }

    fn feed(&mut self, b: u8, delim: u8) -> Byte {
        if std::mem::take(&mut self.after_cr) && b == b'\n' {
            return Byte::BreakTail;
        }
        if self.closing {
            self.closing = false;
            if b == b'"' {
                return Byte::Quoted;
            }
            self.quoted = false;
        } else if self.quoted {
            let escaped = std::mem::take(&mut self.after_backslash);
            if escaped && b == b'"' {
                self.saw_backslash_quote = true;
            }
            if escaped && self.backslash {
                return Byte::Quoted;
            }
            self.after_backslash = b == b'\\';
            self.closing = b == b'"';
            return Byte::Quoted;
        }
        match b {
            b'\n' | b'\r' => {
                self.field_start = true;
                self.after_cr = b == b'\r';
                Byte::Newline
            }
            b if b == delim => {
                self.field_start = true;
                Byte::Delimiter
            }
            b if b.is_ascii_whitespace() => Byte::Space,
            b'"' if self.field_start => {
                (self.quoted, self.field_start) = (true, false);
                Byte::Value
            }
            _ => {
                self.field_start = false;
                Byte::Value
            }
        }
    }
}

/// How a line ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Break {
    Lf,
    CrLf,
    Cr,
}

/// A line event of [`records`].
enum Line {
    /// A record: its first and last line, number of fields, bytes (from the start of its first
    /// line to the end of its last line, line break included), whether it holds nothing but
    /// delimiters and whitespace (`,,,,`, how spreadsheets save an empty row), and its line break
    /// (`None`: the file ends without one).
    Record {
        lines: (u64, u64),
        fields: usize,
        bytes: (u64, u64),
        empty: bool,
        ending: Option<Break>,
    },
    /// A line holding only whitespace, outside a quoted field: its number, whether it holds any
    /// whitespace (`   `, which DuckDB reads as a record, not an empty line), and its line break.
    Blank {
        line: u64,
        spaced: bool,
        ending: Break,
    },
}

/// A line that has ended: where its record starts (`None`: a blank line), its last line, number
/// of fields, end byte, whether it holds a value, and whether it holds whitespace.
type Ended = (Option<(u64, u64)>, u64, usize, u64, bool, bool);

/// One pass over a CSV file with DuckDB's quoting and line breaks (see [`Quoting`]), reporting
/// each record and each blank line in file order. Line numbers are 1-based; a UTF-8 byte order
/// mark at the start is skipped.
fn records(path: &Path, delim: u8, on: impl FnMut(Line)) -> std::io::Result<()> {
    records_in(BufReader::new(File::open(path)?), delim, false, on).map(|_| ())
}

/// [`records`] over any reader, with `backslash` escapes (see [`Quoting`]); whether a `\"` was
/// read inside a quoted field.
fn records_in(
    mut reader: impl BufRead,
    delim: u8,
    backslash: bool,
    mut on: impl FnMut(Line),
) -> std::io::Result<bool> {
    let (mut line, mut line_start, mut pos) = (1u64, 0u64, 0u64);
    if reader.fill_buf()?.starts_with(&[0xEF, 0xBB, 0xBF]) {
        reader.consume(3);
        (line_start, pos) = (3, 3);
    }
    let mut q = Quoting::new(backslash);
    let (mut fields, mut value, mut spaced, mut prev) = (1usize, false, false, 0u8);
    // line and byte where the current record starts; `None` while a line holds only whitespace
    let mut start: Option<(u64, u64)> = None;
    // a line ended by `\r`: reported once the next byte shows whether the break is CRLF
    let mut pending: Option<Ended> = None;
    let event =
        |(start, line, fields, end, value, spaced): Ended, ending: Option<Break>| match start {
            Some((first, from)) => Line::Record {
                lines: (first, line),
                fields,
                bytes: (from, end),
                empty: !value,
                ending,
            },
            None => Line::Blank {
                line,
                spaced,
                ending: ending.unwrap_or(Break::Lf),
            },
        };
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            break;
        }
        for &b in buf {
            let kind = q.feed(b, delim);
            if let Some(ended) = pending.take() {
                if kind == Byte::BreakTail {
                    let (s, l, f, _, v, sp) = ended;
                    on(event((s, l, f, pos + 1, v, sp), Some(Break::CrLf)));
                    line_start = pos + 1;
                } else {
                    on(event(ended, Some(Break::Cr)));
                }
            }
            match kind {
                Byte::Newline => {
                    let ended = (start.take(), line, fields, pos + 1, value, spaced);
                    if b == b'\r' {
                        pending = Some(ended);
                    } else {
                        on(event(ended, Some(Break::Lf)));
                    }
                    (fields, value, spaced) = (1, false, false);
                    line += 1;
                    line_start = pos + 1;
                }
                Byte::Space => spaced = true,
                Byte::BreakTail => {}
                Byte::Delimiter => {
                    fields += 1;
                    start.get_or_insert((line, line_start));
                }
                Byte::Value => {
                    value = true;
                    start.get_or_insert((line, line_start));
                }
                // a CRLF inside a quoted field is one line break
                Byte::Quoted => {
                    if b == b'\r' || (b == b'\n' && prev != b'\r') {
                        line += 1;
                    }
                }
            }
            prev = b;
            pos += 1;
        }
        let n = buf.len();
        reader.consume(n);
    }
    if let Some(ended) = pending {
        on(event(ended, Some(Break::Cr)));
    }
    if start.is_some() {
        on(event((start, line, fields, pos, value, spaced), None));
    }
    Ok(q.saw_backslash_quote)
}

/// How many line breaks outside quoted fields there are of each kind.
#[derive(Debug, Default, PartialEq)]
struct Endings {
    lf: u64,
    crlf: u64,
    cr: u64,
}

impl Endings {
    /// `2 lines end in CRLF and 1 in LF`, when the file mixes kinds.
    fn mixed(&self) -> Option<String> {
        let kinds: Vec<(u64, &str)> = [(self.crlf, "CRLF"), (self.lf, "LF"), (self.cr, "CR")]
            .into_iter()
            .filter(|(n, _)| *n > 0)
            .collect();
        let [(n, first), rest @ ..] = kinds.as_slice() else {
            return None;
        };
        if rest.is_empty() {
            return None;
        }
        let rest: Vec<String> = rest.iter().map(|(n, k)| format!("{n} in {k}")).collect();
        Some(format!(
            "{n} {} in {first} and {}",
            if *n == 1 { "line ends" } else { "lines end" },
            rest.join(" and ")
        ))
    }
}

fn line_endings(path: &Path, delim: u8) -> std::io::Result<Endings> {
    let mut out = Endings::default();
    records(path, delim, |l| {
        let ending = match l {
            Line::Record {
                ending: Some(e), ..
            }
            | Line::Blank { ending: e, .. } => e,
            Line::Record { ending: None, .. } => return,
        };
        match ending {
            Break::Lf => out.lf += 1,
            Break::CrLf => out.crlf += 1,
            Break::Cr => out.cr += 1,
        }
    })?;
    Ok(out)
}

/// Records whose number of fields differs from the first record's.
#[derive(Debug, Default, PartialEq)]
struct Ragged {
    /// Line where the first record starts, and its number of fields.
    first_line: u64,
    fields: usize,
    /// Start lines of the first differing records (at most [`MAX_LINES_LISTED`]).
    lines: Vec<u64>,
    total: u64,
    /// The same for records with more fields than the first.
    wider_lines: Vec<u64>,
    wider: u64,
    /// Start line and number of fields of the first record with the most fields.
    widest: (u64, usize),
}

/// Counts fields per record (see [`records`]); blank lines are skipped. Quotes inside quoted
/// fields are read as `""`; a file that also has `\"` inside quoted fields and whose lines differ
/// that way is counted again with `\` escaping, and the count with fewer differing lines is kept
/// (DuckDB's sniffer chooses the escape the same way).
fn ragged(path: &Path, delim: u8) -> std::io::Result<Ragged> {
    let plain = ragged_with(path, delim, false)?;
    if plain.1 && plain.0.total > 0 {
        let escaped = ragged_with(path, delim, true)?;
        if escaped.0.total < plain.0.total {
            return Ok(escaped.0);
        }
    }
    Ok(plain.0)
}

/// [`ragged`] with the given escape, and whether a `\"` was read inside a quoted field.
fn ragged_with(path: &Path, delim: u8, backslash: bool) -> std::io::Result<(Ragged, bool)> {
    let mut out = Ragged::default();
    let reader = BufReader::new(File::open(path)?);
    let saw = records_in(reader, delim, backslash, |l| {
        let Line::Record {
            lines: (start, _),
            fields,
            ..
        } = l
        else {
            return;
        };
        if out.first_line == 0 {
            (out.first_line, out.fields, out.widest) = (start, fields, (start, fields));
            return;
        }
        if fields > out.widest.1 {
            out.widest = (start, fields);
        }
        if fields != out.fields {
            out.total += 1;
            if out.lines.len() < MAX_LINES_LISTED {
                out.lines.push(start);
            }
        }
        if fields > out.fields {
            out.wider += 1;
            if out.wider_lines.len() < MAX_LINES_LISTED {
                out.wider_lines.push(start);
            }
        }
    })?;
    Ok((out, saw))
}

/// A section of a CSV file: a maximal run of non-blank lines. A blank line holds only whitespace
/// or only delimiters and whitespace (`,,,,`, a spreadsheet's empty row); a line break inside a
/// quoted field does not end a line.
#[derive(Debug, Clone, PartialEq)]
struct Section {
    /// First and last line (1-based).
    lines: (u64, u64),
    /// From the start of the first line to the end of the last (newline included).
    bytes: (u64, u64),
    /// Fewest and most fields of its records.
    fields: (usize, usize),
    /// Fields of its first record (the header, for a table).
    first_fields: usize,
}

impl Section {
    /// `line 5` or `lines 6-1194`.
    fn line_range(&self) -> String {
        match self.lines {
            (a, b) if a == b => format!("line {a}"),
            (a, b) => format!("lines {a}-{b}"),
        }
    }
}

/// The file's sections in file order (see [`Section`]).
fn sections(path: &Path, delim: u8) -> std::io::Result<Vec<Section>> {
    sections_with(path, delim, false).map(|(all, _)| all)
}

/// [`sections`] with `backslash` escapes in quoted fields (see [`Quoting`]), and whether a `\"`
/// was read inside a quoted field.
fn sections_with(path: &Path, delim: u8, backslash: bool) -> std::io::Result<(Vec<Section>, bool)> {
    let mut out: Vec<Section> = Vec::new();
    let mut open = false;
    let reader = BufReader::new(File::open(path)?);
    let saw = records_in(reader, delim, backslash, |l| match l {
        Line::Blank { .. } | Line::Record { empty: true, .. } => open = false,
        Line::Record {
            lines,
            fields,
            bytes,
            ..
        } => match out.last_mut().filter(|_| open) {
            Some(s) => {
                s.lines.1 = lines.1;
                s.bytes.1 = bytes.1;
                s.fields = (s.fields.0.min(fields), s.fields.1.max(fields));
            }
            None => {
                out.push(Section {
                    lines,
                    bytes,
                    fields: (fields, fields),
                    first_fields: fields,
                });
                open = true;
            }
        },
    })?;
    Ok((out, saw))
}

/// For `magi sql`: a comment saying which lines DuckDB reads for a `section:` source, and the name
/// its scratch copy is shown under (`None` without `section:`).
pub fn section_display(path: &Path, options: &CsvOptions) -> Option<(String, PathBuf)> {
    let n = options.section?;
    let lines = sections(path, delimiter(path, options))
        .ok()
        .and_then(|all| {
            all.get((n as usize).checked_sub(1)?)
                .map(Section::line_range)
        })
        .map_or_else(String::new, |l| format!(" ({l})"));
    let name = file_name(path);
    Some((
        format!(
            "-- section {n} of `{name}`{lines} is copied (with LF line breaks) to a temporary file that DuckDB reads as the CSV; `run` makes that copy, so the read_csv below does not run as printed"
        ),
        PathBuf::from(format!("<section {n} of {name}>")),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(dir: &tempfile::TempDir, name: &str, text: &str) -> std::path::PathBuf {
        let p = dir.path().join(name);
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn ragged_lines_respect_quotes_and_blank_lines() {
        let dir = tempfile::tempdir().unwrap();
        let p = file(
            &dir,
            "r.csv",
            "id,name\r\n1,\"a,b\"\r\n\r\n2,\"multi\nline\"\n3,x,y\n4\n5,z\n",
        );
        assert_eq!(
            ragged(&p, b',').unwrap(),
            Ragged {
                first_line: 1,
                fields: 2,
                lines: vec![6, 7],
                total: 2,
                wider_lines: vec![6],
                wider: 1,
                widest: (6, 3),
            }
        );
    }

    #[test]
    fn sections_end_at_blank_lines_outside_quotes() {
        let dir = tempfile::tempdir().unwrap();
        // BOM, CRLF, a whitespace-only line, and a quoted field holding an empty line
        let text = "\u{feff}title\r\n \r\na,b\r\n1,\"x\r\n\r\ny\"\r\n\n\nend,1,2";
        let p = file(&dir, "s.csv", text);
        let all = sections(&p, b',').unwrap();
        let lines: Vec<(u64, u64)> = all.iter().map(|s| s.lines).collect();
        assert_eq!(lines, vec![(1, 1), (3, 6), (9, 9)]);
        let fields: Vec<(usize, usize)> = all.iter().map(|s| s.fields).collect();
        assert_eq!(fields, vec![(1, 1), (2, 2), (3, 3)]);
        let bytes = |s: &Section| &text.as_bytes()[s.bytes.0 as usize..s.bytes.1 as usize];
        assert_eq!(bytes(&all[0]), b"title\r\n");
        assert_eq!(bytes(&all[1]), b"a,b\r\n1,\"x\r\n\r\ny\"\r\n");
        assert_eq!(bytes(&all[2]), b"end,1,2");
    }

    #[test]
    fn a_quote_inside_a_field_is_text_and_does_not_join_sections() {
        let dir = tempfile::tempdir().unwrap();
        // `12"` is a literal quote; `  "x, ""y"""` opens a quoted field after spaces
        let text = "t\n\nd,desc\n1,PIPE 12\" STEEL\n2,  \"x, \"\"y\"\"\"\n\nend,1\n";
        let p = file(&dir, "q.csv", text);
        let lines: Vec<(u64, u64)> = sections(&p, b',')
            .unwrap()
            .iter()
            .map(|s| s.lines)
            .collect();
        assert_eq!(lines, vec![(1, 1), (3, 5), (7, 7)]);
        assert_eq!(ragged(&p, b',').unwrap().fields, 1);
        let r = ragged(&p, b',').unwrap();
        // every record after line 1 has 2 fields: only the 1-field title differs from them
        assert_eq!((r.first_line, r.total), (1, 4));
    }

    #[test]
    fn lines_of_only_delimiters_separate_sections() {
        let dir = tempfile::tempdir().unwrap();
        let p = file(&dir, "c.csv", "t,x\n,,\na,b,c\n1,2,3\n , ,\t\nend\n");
        let lines: Vec<(u64, u64)> = sections(&p, b',')
            .unwrap()
            .iter()
            .map(|s| s.lines)
            .collect();
        assert_eq!(lines, vec![(1, 1), (3, 4), (6, 6)]);
    }

    #[test]
    fn a_section_copy_has_lf_breaks_except_inside_quoted_fields() {
        let dir = tempfile::tempdir().unwrap();
        let text = "t\r\n\r\na,b\r\n1,\"x\r\ny\"\n2,z\r\n3,w\n\r\nend\r\n";
        let p = file(&dir, "m.csv", text);
        let s = &sections(&p, b',').unwrap()[1];
        let out = dir.path().join("out.csv");
        copy_normalized(&p, &out, s.bytes, b',').unwrap();
        assert_eq!(
            std::fs::read(&out).unwrap(),
            b"a,b\n1,\"x\r\ny\"\n2,z\n3,w\n".to_vec()
        );
        assert_eq!(
            line_endings(&p, b',').unwrap(),
            Endings {
                lf: 2,
                crlf: 6,
                cr: 0
            }
        );
    }

    #[test]
    fn a_lone_cr_ends_a_line_like_lf_and_crlf() {
        let dir = tempfile::tempdir().unwrap();
        // classic Mac breaks, a CRLF, and a quoted field holding a CR
        let text = "title\r\ra,b\r1,\"x\ry\"\r2,z\r\n3,w\r\rend,1,2\r";
        let p = file(&dir, "cr.csv", text);
        let all = sections(&p, b',').unwrap();
        let lines: Vec<(u64, u64)> = all.iter().map(|s| s.lines).collect();
        assert_eq!(lines, vec![(1, 1), (3, 7), (9, 9)]);
        let fields: Vec<(usize, usize)> = all.iter().map(|s| s.fields).collect();
        assert_eq!(fields, vec![(1, 1), (2, 2), (3, 3)]);
        let r = ragged(&p, b',').unwrap();
        assert_eq!((r.lines.clone(), r.total), (vec![3, 4, 6, 7, 9], 5));
        let out = dir.path().join("out.csv");
        copy_normalized(&p, &out, all[1].bytes, b',').unwrap();
        assert_eq!(
            std::fs::read(&out).unwrap(),
            b"a,b\n1,\"x\ry\"\n2,z\n3,w\n".to_vec()
        );
        assert_eq!(
            line_endings(&p, b',').unwrap(),
            Endings {
                lf: 0,
                crlf: 1,
                cr: 7
            }
        );
    }

    #[test]
    fn delimiter_is_guessed_from_the_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let options = CsvOptions::default();
        let p = file(&dir, "s.csv", "\na;b;c\n1;2;3\n");
        assert_eq!(delimiter(&p, &options), b';');
        let p = file(&dir, "c.csv", "a,b\n");
        assert_eq!(delimiter(&p, &options), b',');
        let tab = CsvOptions {
            delimiter: Some("\t".into()),
            ..options
        };
        assert_eq!(delimiter(&p, &tab), b'\t');
    }
}
