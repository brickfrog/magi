//! Excel (`.xlsx`) source reader.
//!
//! Reads one rectangular table from a worksheet and hands it over as a [`StagedTable`] of
//! canonical text (see [`crate::source::staged`]). The reader decides where the table is, what
//! the columns are called and which [`Type`] each column most plausibly has; it never guesses
//! around layout problems. Instead it reports them as notes so the analyst can declare a
//! `range:`/`header_row:` or a schema.
//!
//! Table boundaries:
//! - The table starts at the first row of `range`, at `header_row`, or at the first row of the
//!   sheet's used range; columns span `range` or the used range.
//! - `section: N` reads the N-th run of non-blank rows of the used range instead (a row is blank
//!   when its cells are all empty or whitespace-only), with the columns that have a value in it.
//! - With a header, the first row names the columns (trimmed, internal whitespace collapsed);
//!   blank names become `column_<N>`, duplicates get `_2`, `_3`, ... suffixes.
//! - Data ends at the first entirely blank row, like DuckDB's `read_xlsx`. Non-empty rows after
//!   that blank row are reported, never read. When `range` names an explicit end row, every
//!   non-blank row inside it is data and blank rows are skipped.
//!
//! Type inference looks at every data row, after `fill_down` columns took the last non-empty
//! cell above each empty (or whitespace-only) one. Numbers stored as text (`'12.5`) count as
//! numbers next to number cells; a column holding only numbers stored as text stays `string`
//! (with a note naming the numeric type to declare). Their text is kept exactly as written: a
//! `string` column gets `12.50`, and numeric columns cast that same text. Text numbers that look
//! like codes (leading zeros, `007`) or that no 64-bit int holds are text, so identifiers never
//! collapse into equal floats.
//! Columns mixing incompatible cell kinds (dates and text, numbers and text) are inferred as
//! `string` with a warning citing cell references (notes about a column's inferred type name the
//! column, so they can be left out when its type is declared). Merged cells overlapping the table
//! are reported (only their first cell has a value). Messages never contain cell values.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use calamine::{
    Data, Dimensions, ExcelDateTime, Range, Reader, SheetType, Sheets, open_workbook_auto,
};

use crate::semantic::types::Type;
use crate::source::SourceError;
use crate::source::csv::SECTION_ERROR;
use crate::source::staged::{NoteLevel, StagedColumn, StagedTable};

/// How to locate and interpret the table inside a workbook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExcelOptions {
    /// Worksheet name; `None` selects the first worksheet.
    pub sheet: Option<String>,
    /// A1 range such as `A3:L1184`; open-ended `A3:L` or `A3` extend to the end of the used
    /// range.
    pub range: Option<String>,
    /// 1-based worksheet row holding the header; an alternative to `range`.
    pub header_row: Option<u32>,
    /// 1-based section of the sheet (a run of non-blank rows) holding the table; an alternative
    /// to `range` and `header_row`.
    pub section: Option<u32>,
    /// Whether the first table row names the columns; `false` names them `column_1..N`.
    pub header: bool,
    /// Infer every column as `string` (values stay canonical text: dates ISO, not serials).
    pub all_text: bool,
    /// Columns whose empty cells take the nearest non-empty value above them.
    pub fill_down: Vec<String>,
    /// Name of the column holding each row's 1-based data row number (added at staging).
    pub row_number: Option<String>,
}

impl Default for ExcelOptions {
    fn default() -> Self {
        Self {
            sheet: None,
            range: None,
            header_row: None,
            section: None,
            header: true,
            all_text: false,
            fill_down: Vec::new(),
            row_number: None,
        }
    }
}

/// How many rows below the header are inspected for a wider row (header-trap detection).
const TRAP_LOOKAHEAD: u32 = 20;
/// How many cell references a message cites before eliding the rest.
const MAX_REFS: usize = 3;
/// How many row numbers a message lists before eliding the rest.
const MAX_ROWS_LISTED: usize = 5;
const MS_PER_DAY: i64 = 86_400_000;

#[cfg(test)]
/// Names of all sheets in workbook order.
pub fn sheet_names(path: &Path) -> Result<Vec<String>, String> {
    Ok(open(path)?.sheet_names())
}

/// Reads the table selected by `opts` from the workbook at `path`.
pub fn read_excel(path: &Path, opts: &ExcelOptions) -> Result<StagedTable, SourceError> {
    let mut workbook = open(path)?;
    let file = path.file_name().map_or_else(
        || path.display().to_string(),
        |f| f.to_string_lossy().into_owned(),
    );
    let sheet = pick_sheet(&workbook, opts.sheet.as_deref(), &file)?;
    let range = workbook
        .worksheet_range(&sheet)
        .map_err(|e| format!("{file}: cannot read sheet `{sheet}`: {e}"))?;
    let region = match opts.section {
        Some(n) => section_region(&range, n, &file, &sheet)?,
        None => resolve_region(&range, opts, &file, &sheet)?,
    };
    let merged = merged_cells(&mut workbook, &sheet);
    Ok(TableReader {
        range: &range,
        sheet: &sheet,
        region,
        opts,
        merged: &merged,
    }
    .read())
}

/// Whether a cell holds nothing: empty, or text of only whitespace.
fn is_blank_cell(data: Option<&Data>) -> bool {
    match data {
        None | Some(Data::Empty) => true,
        Some(Data::String(s)) => s.trim().is_empty(),
        Some(_) => false,
    }
}

/// The sheet's sections: maximal runs of rows that are not blank (every cell of the used range
/// empty or whitespace-only), as 0-based inclusive `(first row, last row, first column, last
/// column)`, the columns trimmed to those with a non-blank cell in the section.
fn sections(range: &Range<Data>) -> Vec<(u32, u32, u32, u32)> {
    let (Some((ur0, uc0)), Some((ur1, uc1))) = (range.start(), range.end()) else {
        return Vec::new();
    };
    let mut out: Vec<(u32, u32, u32, u32)> = Vec::new();
    let mut open = false;
    for row in ur0..=ur1 {
        let cols: Vec<u32> = (uc0..=uc1)
            .filter(|&c| !is_blank_cell(range.get_value((row, c))))
            .collect();
        let (Some(&first), Some(&last)) = (cols.first(), cols.last()) else {
            open = false;
            continue;
        };
        match out.last_mut().filter(|_| open) {
            Some(s) => *s = (s.0, row, s.2.min(first), s.3.max(last)),
            None => {
                out.push((row, row, first, last));
                open = true;
            }
        }
    }
    out
}

/// The table of `section: n` (see [`sections`]); M213 when the sheet has no such section.
fn section_region(
    range: &Range<Data>,
    n: u32,
    file: &str,
    sheet: &str,
) -> Result<Region, SourceError> {
    let all = sections(range);
    if all.is_empty() {
        return Err(format!("{file}: sheet `{sheet}` is empty").into());
    }
    let Some(&(r0, r1, c0, c1)) = (n as usize).checked_sub(1).and_then(|i| all.get(i)) else {
        let listed: Vec<String> = all
            .iter()
            .enumerate()
            .map(|(i, &(r0, r1, c0, c1))| {
                let rows = if r0 == r1 {
                    format!("row {}", r0 + 1)
                } else {
                    format!("rows {}-{}", r0 + 1, r1 + 1)
                };
                format!(
                    "section {}: {rows} ({}:{})",
                    i + 1,
                    cell_ref(r0, c0),
                    cell_ref(r1, c1)
                )
            })
            .collect();
        let k = all.len();
        return Err(SourceError {
            code: SECTION_ERROR,
            message: format!(
                "{file}: sheet `{sheet}` has {k} {}, so there is no section {n}: {}",
                if k == 1 { "section" } else { "sections" },
                listed.join(", ")
            ),
            help: Some(
                "a section is a run of non-blank rows, numbered from 1; rows whose cells are all empty or whitespace-only separate sections".into(),
            ),
        });
    };
    Ok(Region {
        r0,
        c0,
        r1,
        c1,
        stop_at_blank: false,
        // a title touching the table is part of its section: check the first row is the header
        implicit_start: true,
    })
}

/// Merged ranges of a worksheet (`.xlsx` and `.xls`; other formats report none).
fn merged_cells(workbook: &mut Sheets<BufReader<File>>, sheet: &str) -> Vec<Dimensions> {
    match workbook {
        Sheets::Xlsx(x) => x.merge_cells_by_sheet_name(sheet).unwrap_or_default(),
        Sheets::Xls(x) => x.merge_cells_by_sheet_name(sheet).unwrap_or_default(),
        Sheets::Xlsb(_) | Sheets::Ods(_) => Vec::new(),
    }
}

fn open(path: &Path) -> Result<Sheets<BufReader<File>>, String> {
    open_workbook_auto(path).map_err(|e| format!("cannot open workbook `{}`: {e}", path.display()))
}

fn pick_sheet(
    workbook: &Sheets<BufReader<File>>,
    wanted: Option<&str>,
    file: &str,
) -> Result<String, String> {
    let sheets = workbook.sheets_metadata();
    let worksheets = || sheets.iter().filter(|s| s.typ == SheetType::WorkSheet);
    match wanted {
        Some(name) => {
            if worksheets().any(|s| s.name == name) {
                return Ok(name.to_owned());
            }
            let available: Vec<String> = worksheets().map(|s| format!("`{}`", s.name)).collect();
            let hint = worksheets()
                .find(|s| s.name.eq_ignore_ascii_case(name))
                .map(|s| format!(" (did you mean `{}`?)", s.name))
                .unwrap_or_default();
            Err(format!(
                "{file}: sheet `{name}` not found{hint}; available sheets: {}",
                if available.is_empty() {
                    "none".to_owned()
                } else {
                    available.join(", ")
                }
            ))
        }
        None => worksheets()
            .next()
            .map(|s| s.name.clone())
            .ok_or_else(|| format!("{file}: workbook has no worksheets")),
    }
}

/// Absolute, 0-based, inclusive worksheet coordinates of the table (header included).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Region {
    r0: u32,
    c0: u32,
    r1: u32,
    c1: u32,
    /// Stop at the first blank row (otherwise blank rows inside the region are skipped).
    stop_at_blank: bool,
    /// The table's first row was not named by the program (used range or `section:`), so the
    /// header-trap check applies.
    implicit_start: bool,
}

fn resolve_region(
    range: &Range<Data>,
    opts: &ExcelOptions,
    file: &str,
    sheet: &str,
) -> Result<Region, String> {
    let (Some((ur0, uc0)), Some((ur1, uc1))) = (range.start(), range.end()) else {
        return Err(format!("{file}: sheet `{sheet}` is empty"));
    };
    if opts.header_row.is_some() && !opts.header {
        return Err(format!(
            "{file}: `header_row` conflicts with `header: false`; use `range:` to say where headerless data starts"
        ));
    }
    if opts.header_row == Some(0) {
        return Err(format!(
            "{file}: `header_row` is 1-based; 0 is not a worksheet row"
        ));
    }
    let region = if let Some(text) = &opts.range {
        let spec = parse_range(text).ok_or_else(|| {
            format!("{file}: invalid range `{text}`; expected A1 notation such as `A3:L1184`, `A3:L` or `A3`")
        })?;
        let r0 = spec.start_row.unwrap_or(ur0);
        let (c1, r1, explicit_end) = match spec.end {
            Some((c, Some(r))) => (c, r, true),
            Some((c, None)) => (c, ur1, false),
            None => (uc1, ur1, false),
        };
        if let Some(h) = opts.header_row
            && h - 1 != r0
        {
            return Err(format!(
                "{file}: `header_row: {h}` disagrees with range `{text}`, which starts at row {}",
                r0 + 1
            ));
        }
        if spec.start_col > c1 || r0 > r1 {
            let why = if r0 > r1 && !explicit_end {
                format!(" (it starts below the last used row {})", ur1 + 1)
            } else {
                String::new()
            };
            return Err(format!(
                "{file}: range `{text}` on sheet `{sheet}` selects no cells{why}"
            ));
        }
        Region {
            r0,
            c0: spec.start_col,
            r1,
            c1,
            stop_at_blank: !explicit_end,
            implicit_start: false,
        }
    } else if let Some(h) = opts.header_row {
        if h - 1 > ur1 {
            return Err(format!(
                "{file}: `header_row: {h}` is below the last used row {} of sheet `{sheet}`",
                ur1 + 1
            ));
        }
        Region {
            r0: h - 1,
            c0: uc0,
            r1: ur1,
            c1: uc1,
            stop_at_blank: true,
            implicit_start: false,
        }
    } else {
        Region {
            r0: ur0,
            c0: uc0,
            r1: ur1,
            c1: uc1,
            stop_at_blank: true,
            implicit_start: true,
        }
    };
    Ok(region)
}

#[derive(Debug, PartialEq, Eq)]
struct RangeSpec {
    start_col: u32,
    start_row: Option<u32>,
    end: Option<(u32, Option<u32>)>,
}

/// Parses `A3:L1184`, `A3:L`, `A3`, `A:L` (with optional `$`), 0-based results.
fn parse_range(text: &str) -> Option<RangeSpec> {
    let text = text.trim();
    let (start, end) = match text.split_once(':') {
        Some((s, e)) => (s, Some(e)),
        None => (text, None),
    };
    let (start_col, start_row) = parse_ref(start)?;
    let end = match end {
        Some(e) => Some(parse_ref(e)?),
        None => None,
    };
    Some(RangeSpec {
        start_col,
        start_row,
        end,
    })
}

/// Parses `L1184`, `$L$1184` or `L` into a 0-based column and optional 0-based row.
fn parse_ref(text: &str) -> Option<(u32, Option<u32>)> {
    let text = text.trim().replace('$', "");
    let split = text
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(text.len());
    let (letters, digits) = text.split_at(split);
    if letters.is_empty() || letters.len() > 3 {
        return None;
    }
    let col = letters.bytes().try_fold(0u32, |acc, b| {
        Some(acc * 26 + u32::from(b.to_ascii_uppercase() - b'A') + 1)
    })? - 1;
    if col >= 16_384 {
        return None;
    }
    let row = if digits.is_empty() {
        None
    } else {
        if !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let row: u32 = digits.parse().ok()?;
        if row == 0 || row > 1_048_576 {
            return None;
        }
        Some(row - 1)
    };
    Some((col, row))
}

/// `A1`-style reference for 0-based coordinates.
fn cell_ref(row: u32, col: u32) -> String {
    format!("{}{}", col_letters(col), row + 1)
}

fn col_letters(col: u32) -> String {
    let mut n = col + 1;
    let mut out = Vec::new();
    while n > 0 {
        let rem = (n - 1) % 26;
        out.push(b'A' + rem as u8);
        n = (n - 1) / 26;
    }
    out.reverse();
    String::from_utf8(out).expect("ASCII letters")
}

/// A classified cell with its canonical text.
#[derive(Debug, Clone, PartialEq)]
enum Cell {
    Empty,
    Error,
    /// Numeric cell; canonical shortest round-trip text.
    Number(String),
    /// Text that is a plain number: original text (what is staged) and normalized number text
    /// (what inference measures).
    NumText {
        raw: String,
        norm: String,
    },
    Date(String),
    DateTime(String),
    Time(String),
    Bool(bool),
    Text(String),
}

/// Cell kinds that can share a column without making it `string`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Numeric,
    Date,
    Time,
    Bool,
    Text,
}

impl Family {
    const ALL: [Family; 5] = [
        Family::Numeric,
        Family::Date,
        Family::Time,
        Family::Bool,
        Family::Text,
    ];

    fn label(self) -> &'static str {
        match self {
            Family::Numeric => "numeric",
            Family::Date => "date",
            Family::Time => "time",
            Family::Bool => "boolean",
            Family::Text => "text",
        }
    }
}

impl Cell {
    /// The cell's family; numbers stored as text are counted apart (see [`ColStats::families`]).
    fn family(&self) -> Option<Family> {
        match self {
            Cell::Empty | Cell::Error | Cell::NumText { .. } => None,
            Cell::Number(_) => Some(Family::Numeric),
            Cell::Date(_) | Cell::DateTime(_) => Some(Family::Date),
            Cell::Time(_) => Some(Family::Time),
            Cell::Bool(_) => Some(Family::Bool),
            Cell::Text(_) => Some(Family::Text),
        }
    }

    /// An empty cell (whitespace-only text is one, see [`classify`]): what `fill_down` fills.
    fn is_blank(&self) -> bool {
        matches!(self, Cell::Empty)
    }

    /// Text for staging. Numbers stored as text keep their exact text whatever the column's
    /// type: casting `00123` or `1.50` to a number reads the same value, and a `string` column
    /// must not be rewritten.
    fn into_text(self) -> Option<String> {
        match self {
            Cell::Empty | Cell::Error => None,
            Cell::Number(s)
            | Cell::NumText { raw: s, .. }
            | Cell::Date(s)
            | Cell::DateTime(s)
            | Cell::Time(s)
            | Cell::Text(s) => Some(s),
            Cell::Bool(b) => Some(if b { "true" } else { "false" }.to_owned()),
        }
    }
}

fn classify(data: Option<&Data>) -> Cell {
    match data {
        None | Some(Data::Empty) => Cell::Empty,
        Some(Data::Error(_)) => Cell::Error,
        Some(Data::Int(i)) => Cell::Number(i.to_string()),
        Some(Data::Float(f)) => Cell::Number(format_f64(*f)),
        Some(Data::Bool(b)) => Cell::Bool(*b),
        // whitespace-only text (spaces, no-break spaces, ...) is blank, as in CSV
        Some(Data::String(s)) if s.trim().is_empty() => Cell::Empty,
        Some(Data::String(s)) => match normalize_numeric_text(s.trim()) {
            Some(norm) if !is_code(s.trim(), &norm) => Cell::NumText {
                raw: s.clone(),
                norm,
            },
            _ => Cell::Text(s.clone()),
        },
        Some(Data::DateTime(dt)) => classify_datetime(dt),
        Some(Data::DateTimeIso(s)) => classify_iso(s),
        Some(Data::DurationIso(s)) => Cell::Text(s.clone()),
    }
}

/// Shortest round-trip decimal text; Rust's `Display` for `f64` never uses exponent notation.
fn format_f64(f: f64) -> String {
    if f == 0.0 {
        "0".to_owned()
    } else {
        f.to_string()
    }
}

/// Numeric-looking text that must stay text: a leading zero (`007`, `00.5`; not `0` or `0.5`),
/// an integer no 64-bit int holds, or more than 38 digits.
fn is_code(text: &str, norm: &str) -> bool {
    let int = text.trim_start_matches(['-', '+']);
    let int = int.split_once('.').map_or(int, |(i, _)| i);
    let digits = norm.bytes().filter(u8::is_ascii_digit).count();
    (int.len() > 1 && int.starts_with('0'))
        || (!norm.contains('.') && norm.parse::<i64>().is_err())
        || digits > 38
}

/// Normalizes text matching `^-?\d+(\.\d+)?$` (leading zeros, trailing fraction zeros, `-0`).
fn normalize_numeric_text(t: &str) -> Option<String> {
    let (neg, body) = match t.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, t),
    };
    let (int, frac) = match body.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (body, None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(int) || frac.is_some_and(|f| !digits(f)) {
        return None;
    }
    let int = match int.trim_start_matches('0') {
        "" => "0",
        i => i,
    };
    let frac = frac
        .map(|f| f.trim_end_matches('0'))
        .filter(|f| !f.is_empty());
    let mut out = String::with_capacity(t.len());
    if neg && (int != "0" || frac.is_some()) {
        out.push('-');
    }
    out.push_str(int);
    if let Some(f) = frac {
        out.push('.');
        out.push_str(f);
    }
    Some(out)
}

fn classify_datetime(dt: &ExcelDateTime) -> Cell {
    let total_ms = (dt.as_f64() * MS_PER_DAY as f64).round() as i64;
    if dt.is_duration() {
        return if (0..MS_PER_DAY).contains(&total_ms) {
            Cell::Time(format_time(total_ms))
        } else {
            // Longer or negative durations have no TIME equivalent.
            let sign = if total_ms < 0 { "-" } else { "" };
            let ms = total_ms.unsigned_abs();
            let secs = ms / 1000;
            Cell::Text(format!(
                "{sign}{}:{:02}:{:02}",
                secs / 3600,
                secs / 60 % 60,
                secs % 60
            ))
        };
    }
    if dt.as_f64() < 1.0 && dt.as_f64() >= 0.0 {
        return Cell::Time(format_time(total_ms.rem_euclid(MS_PER_DAY)));
    }
    let (y, m, d, hour, min, sec, milli) = dt.to_ymd_hms_milli();
    let (mut y, mut m, mut d) = (i32::from(y), u32::from(m), u32::from(d));
    let mut ms_of_day =
        ((i64::from(hour) * 60 + i64::from(min)) * 60 + i64::from(sec)) * 1000 + i64::from(milli);
    if ms_of_day >= MS_PER_DAY {
        // Rounding carried the time to midnight of the next day.
        ms_of_day -= MS_PER_DAY;
        (y, m, d) = next_day(y, m, d);
    }
    let date = format!("{y:04}-{m:02}-{d:02}");
    if d > days_in_month(y, m) {
        // Excel's fictitious 1900-02-29.
        return Cell::Text(date);
    }
    if ms_of_day == 0 {
        Cell::Date(date)
    } else {
        Cell::DateTime(format!("{date} {}", format_time(ms_of_day)))
    }
}

/// `HH:MM:SS[.ffffff]` for milliseconds since midnight.
fn format_time(ms: i64) -> String {
    let secs = ms / 1000;
    let frac = ms % 1000;
    let base = format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
    if frac == 0 {
        base
    } else {
        format!("{base}.{:06}", frac * 1000)
    }
}

fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn next_day(y: i32, m: u32, d: u32) -> (i32, u32, u32) {
    if d < days_in_month(y, m) {
        (y, m, d + 1)
    } else if m < 12 {
        (y, m + 1, 1)
    } else {
        (y + 1, 1, 1)
    }
}

/// Cells stored as ISO 8601 text (`t="d"` cells).
fn classify_iso(s: &str) -> Cell {
    let s = s.trim();
    let is_date = |d: &str| {
        d.len() == 10
            && d.bytes().enumerate().all(|(i, b)| {
                if i == 4 || i == 7 {
                    b == b'-'
                } else {
                    b.is_ascii_digit()
                }
            })
    };
    if is_date(s) {
        return Cell::Date(s.to_owned());
    }
    if let Some((date, time)) = s.split_once('T')
        && is_date(date)
    {
        let time = time.trim_end_matches('Z');
        if time.bytes().all(|b| matches!(b, b'0' | b':' | b'.')) {
            return Cell::Date(date.to_owned());
        }
        return Cell::DateTime(format!("{date} {time}"));
    }
    if s.contains(':')
        && s.bytes()
            .all(|b| b.is_ascii_digit() || b == b':' || b == b'.')
    {
        return Cell::Time(s.to_owned());
    }
    Cell::Text(s.to_owned())
}

/// Per-column observations over all data rows.
#[derive(Debug, Default)]
struct ColStats {
    counts: [usize; 5],
    /// First few (row, col) positions per family, in row order.
    refs: [Vec<(u32, u32)>; 5],
    datetimes: usize,
    num_text: usize,
    num_text_refs: Vec<(u32, u32)>,
    errors: usize,
    error_refs: Vec<(u32, u32)>,
    /// Every numeric value is an integer within i64.
    all_int: bool,
    /// Some numeric cell (not text) holds a fraction or an integer beyond i64: a binary float.
    float_cells: bool,
    max_scale: usize,
    max_int_digits: usize,
}

impl ColStats {
    fn new() -> Self {
        Self {
            all_int: true,
            ..Self::default()
        }
    }

    fn observe(&mut self, cell: &Cell, pos: (u32, u32)) {
        match cell {
            Cell::Error => {
                self.errors += 1;
                push_ref(&mut self.error_refs, pos);
            }
            Cell::Number(s) => {
                if !self.observe_number(s) {
                    self.float_cells = true;
                }
            }
            Cell::NumText { norm, .. } => {
                self.num_text += 1;
                push_ref(&mut self.num_text_refs, pos);
                self.observe_number(norm);
            }
            Cell::DateTime(_) => self.datetimes += 1,
            _ => {}
        }
        if let Some(f) = cell.family() {
            let i = f as usize;
            self.counts[i] += 1;
            push_ref(&mut self.refs[i], pos);
        }
    }

    /// Measures a normalized number; whether it is an integer within i64.
    fn observe_number(&mut self, s: &str) -> bool {
        let digits = s.trim_start_matches('-');
        let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
        self.max_int_digits = self.max_int_digits.max(int.len());
        self.max_scale = self.max_scale.max(frac.len());
        let int = frac.is_empty() && s.parse::<i64>().is_ok();
        self.all_int &= int;
        int
    }

    /// Cell counts and first references per family. Numbers stored as text join the number cells
    /// of a column that has some, and are text cells otherwise: a column of text cells is never
    /// rewritten as numbers (`42` as `42.000`), and warnings call text cells text.
    fn families(&self) -> ([usize; 5], [Vec<(u32, u32)>; 5]) {
        let (mut counts, mut refs) = (self.counts, self.refs.clone());
        if self.num_text > 0 {
            let to = if counts[Family::Numeric as usize] > 0 {
                Family::Numeric
            } else {
                Family::Text
            } as usize;
            counts[to] += self.num_text;
            refs[to].extend(&self.num_text_refs);
            refs[to].sort_unstable();
            refs[to].truncate(MAX_REFS);
        }
        (counts, refs)
    }

    /// Every non-empty cell is a number stored as text.
    fn only_numbers_as_text(&self) -> bool {
        self.num_text > 0 && self.counts.iter().all(|&n| n == 0)
    }

    fn numeric_type(&self) -> Type {
        let digits = self.max_int_digits + self.max_scale;
        if self.all_int {
            Type::Int
        } else if digits <= 18 && (self.max_scale <= 6 || !self.float_cells) {
            Type::Decimal(18, self.max_scale as u8)
        } else if !self.float_cells && digits <= 38 {
            // numbers stored as text are exact decimals, whatever their length
            Type::Decimal(38, self.max_scale as u8)
        } else {
            Type::Float
        }
    }

    /// When every non-empty cell is a number (stored as text or not): the narrowest type that
    /// holds each exactly, a decimal with the digits the values need.
    fn exact_numeric(&self) -> Option<Type> {
        let others = Family::ALL
            .into_iter()
            .filter(|f| *f != Family::Numeric)
            .any(|f| self.counts[f as usize] > 0);
        if others || self.num_text + self.counts[Family::Numeric as usize] == 0 {
            return None;
        }
        Some(match self.numeric_type() {
            Type::Decimal(..) => Type::Decimal(
                (self.max_int_digits + self.max_scale).max(1) as u8,
                self.max_scale as u8,
            ),
            other => other,
        })
    }

    /// Inferred type, plus the families present when they conflict.
    fn infer(&self) -> (Type, Vec<Family>) {
        let (counts, _) = self.families();
        let present: Vec<Family> = Family::ALL
            .into_iter()
            .filter(|f| counts[*f as usize] > 0)
            .collect();
        let ty = match present.as_slice() {
            [] => Type::String,
            [Family::Numeric] => self.numeric_type(),
            [Family::Date] if self.datetimes > 0 => Type::Timestamp,
            [Family::Date] => Type::Date,
            [Family::Time] => Type::Time,
            [Family::Bool] => Type::Bool,
            [Family::Text] => Type::String,
            _ => return (Type::String, present),
        };
        (ty, Vec::new())
    }

    fn mixed_warning(&self, column: &str, mut present: Vec<Family>) -> String {
        let (counts, family_refs) = self.families();
        present.sort_by_key(|f| std::cmp::Reverse(counts[*f as usize]));
        let parts: Vec<String> = present
            .iter()
            .map(|f| {
                let n = counts[*f as usize];
                format!(
                    "{n} {} {}",
                    f.label(),
                    if n == 1 { "cell" } else { "cells" }
                )
            })
            .collect();
        let minority = &present[1..];
        let mut refs: Vec<(u32, u32)> = minority
            .iter()
            .flat_map(|f| family_refs[*f as usize].iter().copied())
            .collect();
        refs.sort_unstable();
        let total: usize = minority.iter().map(|f| counts[*f as usize]).sum();
        format!(
            "column {column}: {}; inferred string — declare a schema or parse explicitly",
            with_refs(&join_and(&parts), &refs, total)
        )
    }
}

fn push_ref(refs: &mut Vec<(u32, u32)>, pos: (u32, u32)) {
    if refs.len() < MAX_REFS {
        refs.push(pos);
    }
}

/// `what (first: B75, B210, B300, ...)` citing at most [`MAX_REFS`] references out of `total`.
fn with_refs(what: &str, refs: &[(u32, u32)], total: usize) -> String {
    let shown: Vec<String> = refs
        .iter()
        .take(MAX_REFS)
        .map(|&(r, c)| cell_ref(r, c))
        .collect();
    let more = if total > shown.len() { ", ..." } else { "" };
    let label = if total == 1 { "at" } else { "first" };
    format!("{what} ({label}: {}{more})", shown.join(", "))
}

fn join_and(parts: &[String]) -> String {
    match parts {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

fn list_rows(rows: &[u32]) -> String {
    let shown: Vec<String> = rows
        .iter()
        .take(MAX_ROWS_LISTED)
        .map(|r| (r + 1).to_string())
        .collect();
    let more = if rows.len() > MAX_ROWS_LISTED {
        ", ..."
    } else {
        ""
    };
    format!("{}{more}", shown.join(", "))
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

struct TableReader<'a> {
    range: &'a Range<Data>,
    sheet: &'a str,
    region: Region,
    opts: &'a ExcelOptions,
    /// Merged ranges of the sheet, 0-based absolute coordinates.
    merged: &'a [Dimensions],
}

impl TableReader<'_> {
    fn cell(&self, row: u32, col: u32) -> Cell {
        classify(self.range.get_value((row, col)))
    }

    /// Whether a cell is empty or whitespace-only, without materializing its text.
    fn is_empty(&self, row: u32, col: u32) -> bool {
        is_blank_cell(self.range.get_value((row, col)))
    }

    fn non_empty(&self, row: u32, cols: std::ops::RangeInclusive<u32>) -> usize {
        cols.filter(|&c| !self.is_empty(row, c)).count()
    }

    fn is_blank(&self, row: u32) -> bool {
        (self.region.c0..=self.region.c1).all(|c| self.is_empty(row, c))
    }

    fn read(self) -> StagedTable {
        let Region {
            r0,
            c0,
            r1,
            c1,
            stop_at_blank,
            ..
        } = self.region;
        let mut table = StagedTable::default();
        let width = (c1 - c0 + 1) as usize;

        let names = if self.opts.header {
            self.check_header_trap(&mut table);
            self.header_names(&mut table)
        } else {
            (1..=width).map(|i| format!("column_{i}")).collect()
        };

        // Collect data rows. Columns filled down take the last non-empty cell above an empty
        // one before anything looks at the values.
        let data_start = if self.opts.header { r0 + 1 } else { r0 };
        let fill: Vec<usize> = (0..width)
            .filter(|&i| {
                self.opts
                    .fill_down
                    .iter()
                    .any(|f| f.eq_ignore_ascii_case(&names[i]))
            })
            .collect();
        let mut last: Vec<Cell> = vec![Cell::Empty; width];
        let mut cells: Vec<Vec<Cell>> = Vec::new();
        let mut stats: Vec<ColStats> = (0..width).map(|_| ColStats::new()).collect();
        let mut blank_skipped = 0usize;
        let mut ended_at: Option<u32> = None;
        for row in data_start..=r1 {
            if self.is_blank(row) {
                if stop_at_blank {
                    ended_at = Some(row);
                    break;
                }
                blank_skipped += 1;
                continue;
            }
            let mut row_cells: Vec<Cell> = (c0..=c1).map(|c| self.cell(row, c)).collect();
            for &i in &fill {
                if row_cells[i].is_blank() {
                    row_cells[i] = last[i].clone();
                } else {
                    last[i] = row_cells[i].clone();
                }
            }
            for (i, cell) in row_cells.iter().enumerate() {
                stats[i].observe(cell, (row, c0 + i as u32));
            }
            cells.push(row_cells);
            table.source_rows.push(u64::from(row) + 1);
        }
        if let Some(blank) = ended_at {
            let ignored: Vec<u32> = (blank + 1..=r1).filter(|&r| !self.is_blank(r)).collect();
            if !ignored.is_empty() {
                table.warn(format!(
                    "sheet `{}`: data ends at blank row {}; {} after it {} ignored ({} {}) — use `range:` to include {}",
                    self.sheet,
                    blank + 1,
                    plural(ignored.len(), "non-empty row", "non-empty rows"),
                    if ignored.len() == 1 { "was" } else { "were" },
                    if ignored.len() == 1 { "row" } else { "rows" },
                    list_rows(&ignored),
                    if ignored.len() == 1 { "it" } else { "them" },
                ));
            }
        }
        if blank_skipped > 0 {
            table.note(format!(
                "sheet `{}`: skipped {} inside the range",
                self.sheet,
                plural(blank_skipped, "blank row", "blank rows")
            ));
        }
        self.check_merged(&mut table);

        // Infer column types.
        let mut types = Vec::with_capacity(width);
        for (name, st) in names.iter().zip(&stats) {
            if st.errors > 0 {
                table.warn(format!(
                    "column {name}: {} read as null",
                    with_refs(
                        &plural(st.errors, "error cell", "error cells"),
                        &st.error_refs,
                        st.errors
                    )
                ));
            }
            if self.opts.all_text {
                types.push((Type::String, st.exact_numeric()));
                continue;
            }
            let (ty, conflict) = st.infer();
            let stored_as_text = || {
                with_refs(
                    &plural(
                        st.num_text,
                        "number stored as text",
                        "numbers stored as text",
                    ),
                    &st.num_text_refs,
                    st.num_text,
                )
            };
            if !conflict.is_empty() {
                table.type_hint(NoteLevel::Warning, name, st.mixed_warning(name, conflict));
            } else if st.only_numbers_as_text() {
                let declare = st.exact_numeric().map_or(Type::Float, |t| match t {
                    Type::Decimal(p, s) => Type::Decimal(if p <= 18 { 18 } else { 38 }, s),
                    t => t,
                });
                table.type_hint(
                    NoteLevel::Note,
                    name,
                    format!(
                        "column {name}: {} and no other values; kept as text — declare `{name}: {declare}` to read them as numbers",
                        stored_as_text()
                    ),
                );
            } else if st.num_text > 0 && ty != Type::String {
                table.type_hint(
                    NoteLevel::Note,
                    name,
                    format!("column {name}: {} read as numbers", stored_as_text()),
                );
            }
            types.push((ty, st.exact_numeric()));
        }

        table.rows = cells
            .into_iter()
            .map(|row| row.into_iter().map(Cell::into_text).collect())
            .collect();
        table.columns = names
            .into_iter()
            .zip(types)
            .map(|(name, (inferred, exact))| StagedColumn {
                name,
                inferred,
                exact,
            })
            .collect();
        table
    }

    /// Column names from the header row ([`super::header_names`]), with a warning for each
    /// generated or renamed name.
    fn header_names(&self, table: &mut StagedTable) -> Vec<String> {
        let Region { r0, c0, c1, .. } = self.region;
        let named = super::header_names((c0..=c1).map(|col| match self.cell(r0, col) {
            Cell::Error => None,
            cell => cell.into_text(),
        }));
        let at = |i: usize| cell_ref(r0, c0 + i as u32);
        if !named.blank.is_empty() {
            let refs: Vec<String> = named.blank.iter().map(|&i| at(i)).collect();
            let generated: Vec<&str> = named
                .blank
                .iter()
                .map(|&i| named.names[i].as_str())
                .collect();
            table.warn(format!(
                "sheet `{}`: blank header {} {} named {}",
                self.sheet,
                if named.blank.len() == 1 {
                    "cell"
                } else {
                    "cells"
                },
                refs.join(", "),
                generated.join(", ")
            ));
        }
        for (i, first, base) in &named.renamed {
            table.warn(format!(
                "sheet `{}`: duplicate header `{base}` at {} (first at {}) renamed `{}`",
                self.sheet,
                at(*i),
                at(*first),
                named.names[*i]
            ));
        }
        named.names
    }

    /// Warns about merged ranges overlapping the table: only their first cell holds the value,
    /// so the other cells read as empty.
    fn check_merged(&self, table: &mut StagedTable) {
        let Region { r0, c0, c1, .. } = self.region;
        let last = table
            .source_rows
            .last()
            .map_or(r0, |&r| u32::try_from(r - 1).unwrap_or(u32::MAX));
        let mut hits: Vec<&Dimensions> = self
            .merged
            .iter()
            .filter(|m| m.start.0 <= last && m.end.0 >= r0 && m.start.1 <= c1 && m.end.1 >= c0)
            .collect();
        if hits.is_empty() {
            return;
        }
        hits.sort_unstable();
        let shown: Vec<String> = hits
            .iter()
            .take(MAX_REFS)
            .map(|m| {
                format!(
                    "{}:{}",
                    cell_ref(m.start.0, m.start.1),
                    cell_ref(m.end.0, m.end.1)
                )
            })
            .collect();
        let more = if hits.len() > shown.len() {
            ", ..."
        } else {
            ""
        };
        table.warn(format!(
            "sheet `{}`: {} the table ({}{more}); a merged range keeps its value only in its first cell, so the other cells read as empty — unmerge the cells and fill them in",
            self.sheet,
            if hits.len() == 1 {
                "1 merged range overlaps".to_string()
            } else {
                format!("{} merged ranges overlap", hits.len())
            },
            shown.join(", "),
        ));
    }

    /// Warns when the first used row looks like a title rather than the header.
    fn check_header_trap(&self, table: &mut StagedTable) {
        let Region {
            r0,
            c0,
            r1,
            c1,
            implicit_start,
            ..
        } = self.region;
        if !implicit_start {
            return;
        }
        let header_count = self.non_empty(r0, c0..=c1);
        let (mut best_row, mut best_count) = (r0, header_count);
        for row in r0 + 1..=r1.min(r0 + TRAP_LOOKAHEAD) {
            let n = self.non_empty(row, c0..=c1);
            if n > best_count {
                (best_row, best_count) = (row, n);
            }
        }
        if best_row == r0 || header_count * 2 > best_count || best_count - header_count < 2 {
            return;
        }
        let cols: Vec<u32> = (c0..=c1).filter(|&c| !self.is_empty(best_row, c)).collect();
        let (first_col, last_col) = (cols[0], cols[cols.len() - 1]);
        let last_row = (best_row + 1..=r1)
            .take_while(|&r| !self.is_blank(r))
            .last()
            .unwrap_or(best_row);
        let instead = if self.opts.section.is_some() {
            " instead of `section:`, which cannot split a title from the table it touches"
        } else {
            ""
        };
        table.warn(format!(
            "sheet `{}`: header row {} has {} but row {} has {}; the table probably starts at row {} — add `header_row: {}` or `range: \"{}:{}\"`{instead}",
            self.sheet,
            r0 + 1,
            plural(header_count, "non-empty cell", "non-empty cells"),
            best_row + 1,
            best_count,
            best_row + 1,
            best_row + 1,
            cell_ref(best_row, first_col),
            cell_ref(last_row, last_col),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::staged::NoteLevel;

    fn data(file: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("data")
            .join(file)
    }

    fn opts(range: Option<&str>, header_row: Option<u32>) -> ExcelOptions {
        ExcelOptions {
            sheet: Some("Data".into()),
            range: range.map(Into::into),
            header_row,
            ..ExcelOptions::default()
        }
    }

    fn col<'a>(t: &'a StagedTable, name: &str) -> (usize, &'a StagedColumn) {
        t.columns
            .iter()
            .enumerate()
            .find(|(_, c)| c.name == name)
            .unwrap_or_else(|| panic!("no column {name}"))
    }

    fn warnings(t: &StagedTable) -> Vec<&str> {
        t.notes
            .iter()
            .filter(|n| n.level == NoteLevel::Warning)
            .map(|n| n.message.as_str())
            .collect()
    }

    /// No note may repeat a cell value from the table.
    fn assert_no_values_in_notes(t: &StagedTable) {
        for note in &t.notes {
            for row in &t.rows {
                for v in row.iter().flatten() {
                    if v.len() >= 3 && v.chars().any(|c| c.is_alphabetic() || c == '/' || c == ',')
                    {
                        assert!(
                            !note.message.contains(v.as_str()),
                            "note leaks a cell value: {}",
                            note.message
                        );
                    }
                }
            }
        }
    }

    fn is_iso_date(s: &str) -> bool {
        s.len() == 10
            && s.as_bytes()[4] == b'-'
            && s.as_bytes()[7] == b'-'
            && s[..4].parse::<u32>().is_ok()
    }

    #[test]
    fn a_xlsx_stops_at_blank_row_and_reports_total_row() {
        let t = read_excel(&data("a.xlsx"), &opts(None, None)).unwrap();
        assert_eq!(t.rows.len(), 1237);
        assert_eq!(t.columns.len(), 10);
        assert_eq!(t.source_rows.len(), 1237);
        assert_eq!(t.source_rows[0], 2);
        assert!(t.rows.iter().all(|r| r.len() == 10));
        let w = warnings(&t);
        assert!(
            w.iter()
                .any(|m| m.contains("data ends at blank row 1239") && m.contains("row")),
            "missing ignored-row warning: {w:#?}"
        );
        assert!(!w.iter().any(|m| m.contains("header row 1 has")), "{w:#?}");
        assert_no_values_in_notes(&t);
    }

    #[test]
    fn b_xlsx_without_range_warns_about_header_trap() {
        let t = read_excel(&data("b.xlsx"), &opts(None, None)).unwrap();
        let w = warnings(&t);
        let trap = w
            .iter()
            .find(|m| m.contains("header row 1 has"))
            .unwrap_or_else(|| panic!("{w:#?}"));
        assert!(trap.contains("header_row: 3"), "{trap}");
        assert!(trap.contains("range: \"A3:L1184\""), "{trap}");
    }

    fn check_b(t: &StagedTable) {
        assert_eq!(t.rows.len(), 1181);
        assert_eq!(t.columns.len(), 12);
        assert_eq!(t.source_rows[0], 4);
        assert_eq!(*t.source_rows.last().unwrap(), 1184);
        let w = warnings(t);
        for name in ["event_date", "amount"] {
            let (_, c) = col(t, name);
            assert_eq!(c.inferred, Type::String, "{name}");
            let msg = w
                .iter()
                .find(|m| {
                    m.starts_with(&format!("column {name}:")) && m.contains("inferred string")
                })
                .unwrap_or_else(|| panic!("no mixed warning for {name}: {w:#?}"));
            assert!(msg.contains("(first: "), "{msg}");
        }
        let date_msg = w
            .iter()
            .find(|m| m.starts_with("column event_date:"))
            .unwrap();
        assert!(
            date_msg.contains("date cells") && date_msg.contains("text cells"),
            "{date_msg}"
        );
        assert_no_values_in_notes(t);
        // Date cells in a string column still come out as ISO text.
        let (i, _) = col(t, "event_date");
        let iso = t
            .rows
            .iter()
            .filter_map(|r| r[i].as_deref())
            .filter(|s| is_iso_date(s))
            .count();
        assert!(iso > 1100, "only {iso} ISO dates");
    }

    #[test]
    fn b_xlsx_with_range_or_header_row() {
        let by_range = read_excel(&data("b.xlsx"), &opts(Some("A3:L1184"), None)).unwrap();
        check_b(&by_range);
        let by_header = read_excel(&data("b.xlsx"), &opts(None, Some(3))).unwrap();
        check_b(&by_header);
        assert_eq!(by_range.rows, by_header.rows);
        let open_ended = read_excel(&data("b.xlsx"), &opts(Some("A3:L"), None)).unwrap();
        assert_eq!(open_ended.rows, by_range.rows);
    }

    #[test]
    fn a_xlsx_types_are_canonical() {
        let t = read_excel(&data("a.xlsx"), &opts(None, None)).unwrap();
        for c in &t.columns {
            let (i, _) = col(&t, &c.name);
            for v in t.rows.iter().filter_map(|r| r[i].as_deref()) {
                match c.inferred {
                    Type::Date => assert!(is_iso_date(v), "{}: {v}", c.name),
                    Type::Int => assert!(v.parse::<i64>().is_ok(), "{}: {v}", c.name),
                    Type::Decimal(_, s) => {
                        let frac = v.split_once('.').map_or(0, |(_, f)| f.len());
                        assert!(
                            frac <= s as usize && v.parse::<f64>().is_ok() && !v.contains('e'),
                            "{}: {v}",
                            c.name
                        );
                    }
                    _ => {}
                }
            }
        }
        assert!(
            t.columns.iter().any(|c| c.inferred == Type::Date),
            "{:?}",
            t.columns
        );
    }

    #[test]
    fn all_text_keeps_dates_iso() {
        let o = ExcelOptions {
            all_text: true,
            ..opts(None, None)
        };
        let t = read_excel(&data("a.xlsx"), &o).unwrap();
        assert!(t.columns.iter().all(|c| c.inferred == Type::String));
        let typed = read_excel(&data("a.xlsx"), &opts(None, None)).unwrap();
        let (i, _) = typed
            .columns
            .iter()
            .enumerate()
            .find(|(_, c)| c.inferred == Type::Date)
            .unwrap();
        let v = t.rows[0][i].as_deref().unwrap();
        assert!(is_iso_date(v), "{v}");
        assert!(!warnings(&t).iter().any(|m| m.contains("inferred string")));
    }

    #[test]
    fn missing_sheet_lists_available() {
        let o = ExcelOptions {
            sheet: Some("Nope".into()),
            ..ExcelOptions::default()
        };
        let err = read_excel(&data("a.xlsx"), &o).unwrap_err().message;
        assert!(
            err.contains("`Nope` not found") && err.contains("`Data`"),
            "{err}"
        );
        assert!(
            sheet_names(&data("a.xlsx"))
                .unwrap()
                .contains(&"Data".to_owned())
        );
    }

    #[test]
    fn inconsistent_range_and_header_row() {
        let err = read_excel(&data("b.xlsx"), &opts(Some("A3:L1184"), Some(2)))
            .unwrap_err()
            .message;
        assert!(err.contains("disagrees"), "{err}");
        assert!(read_excel(&data("b.xlsx"), &opts(Some("A3:L1184"), Some(3))).is_ok());
        assert!(
            read_excel(&data("b.xlsx"), &opts(Some("3A:L"), None))
                .unwrap_err()
                .message
                .contains("invalid range")
        );
    }

    #[test]
    fn range_parsing() {
        assert_eq!(
            parse_range("A3:L1184"),
            Some(RangeSpec {
                start_col: 0,
                start_row: Some(2),
                end: Some((11, Some(1183)))
            })
        );
        assert_eq!(
            parse_range("$b$3:aa"),
            Some(RangeSpec {
                start_col: 1,
                start_row: Some(2),
                end: Some((26, None))
            })
        );
        assert_eq!(
            parse_range("A3"),
            Some(RangeSpec {
                start_col: 0,
                start_row: Some(2),
                end: None
            })
        );
        assert_eq!(parse_range("A0"), None);
        assert_eq!(parse_range("3"), None);
        assert_eq!(parse_range("A3:"), None);
        assert_eq!(col_letters(0), "A");
        assert_eq!(col_letters(25), "Z");
        assert_eq!(col_letters(26), "AA");
        assert_eq!(col_letters(16_383), "XFD");
    }

    #[test]
    fn numeric_text_normalization() {
        assert_eq!(normalize_numeric_text("12.50").as_deref(), Some("12.5"));
        assert_eq!(normalize_numeric_text("007").as_deref(), Some("7"));
        assert_eq!(normalize_numeric_text("-0.0").as_deref(), Some("0"));
        assert_eq!(normalize_numeric_text("-3.000").as_deref(), Some("-3"));
        assert_eq!(normalize_numeric_text("1,234.5"), None);
        assert_eq!(normalize_numeric_text("12."), None);
        assert_eq!(normalize_numeric_text(".5"), None);
        assert_eq!(normalize_numeric_text("12.5 kg"), None);
        assert_eq!(format_f64(12.3), "12.3");
        assert_eq!(format_f64(5.0), "5");
        assert_eq!(format_f64(-0.0), "0");
        assert_eq!(format_f64(1e20), "100000000000000000000");
    }

    #[test]
    fn numeric_inference() {
        let infer = |vals: &[&str]| {
            let mut st = ColStats::new();
            for (i, v) in vals.iter().enumerate() {
                st.observe(&Cell::Number((*v).to_owned()), (i as u32, 0));
            }
            st.infer().0
        };
        assert_eq!(infer(&["1", "-5", "9223372036854775807"]), Type::Int);
        assert_eq!(infer(&["1", "2.25", "-0.5"]), Type::Decimal(18, 2));
        assert_eq!(infer(&["12.300000000000001"]), Type::Float);
        assert_eq!(infer(&["9223372036854775808"]), Type::Float);
        assert_eq!(infer(&["12345678901234567.5", "0.25"]), Type::Float);
        assert_eq!(infer(&["1234567890123456.5", "0.25"]), Type::Decimal(18, 2));
    }

    #[test]
    fn numbers_stored_as_text_keep_codes_and_long_ids() {
        let text = |s: &str| classify(Some(&Data::String(s.to_owned())));
        for code in ["007", "00123", "-01.5", "12345678901234567890"] {
            assert_eq!(text(code), Cell::Text(code.to_owned()), "{code}");
        }
        assert_eq!(
            text(" 0.50 "),
            Cell::NumText {
                raw: " 0.50 ".into(),
                norm: "0.5".into()
            }
        );
        assert_eq!(text("0").into_text().as_deref(), Some("0"));
        // a column of numbers stored as text stays text; read as numbers (declared), they are
        // exact decimals, never floats, however many digits
        let mut st = ColStats::new();
        for (i, v) in ["12345678901234567890.5", "0.1234567"].iter().enumerate() {
            st.observe(&text(v), (i as u32, 0));
        }
        assert_eq!(st.infer().0, Type::String);
        assert_eq!(st.exact_numeric(), Some(Type::Decimal(27, 7)));
        // next to real number cells they are numbers
        st.observe(&Cell::Number("1".into()), (2, 0));
        assert_eq!(st.infer().0, Type::Decimal(38, 7));
    }

    #[test]
    fn merged_cells_overlapping_the_table_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.xlsx");
        let mut wb = rust_xlsxwriter::Workbook::new();
        let ws = wb.add_worksheet();
        for (c, h) in ["id", "grp"].iter().enumerate() {
            ws.write_string(0, c as u16, *h).unwrap();
        }
        for r in 1..=3 {
            ws.write_number(r, 0, f64::from(r)).unwrap();
        }
        let plain = rust_xlsxwriter::Format::new();
        ws.merge_range(1, 1, 2, 1, "secret-group", &plain).unwrap();
        ws.write_string(3, 1, "group-b").unwrap();
        // far below the table: not reported
        ws.merge_range(40, 0, 40, 1, "footer", &plain).unwrap();
        wb.save(&path).unwrap();

        let t = read_excel(&path, &ExcelOptions::default()).unwrap();
        assert_eq!(t.rows[1][1], None);
        let w = warnings(&t);
        let merged: Vec<&&str> = w.iter().filter(|m| m.contains("merged")).collect();
        assert_eq!(merged.len(), 1, "{w:#?}");
        assert!(
            merged[0].contains("1 merged range overlaps the table (B2:B3)"),
            "{w:#?}"
        );
        assert_no_values_in_notes(&t);
    }

    #[test]
    fn datetime_classification() {
        use calamine::ExcelDateTimeType::{DateTime, TimeDelta};
        let c = |v: f64, t| classify_datetime(&ExcelDateTime::new(v, t, false));
        assert_eq!(c(46144.0, DateTime), Cell::Date("2026-05-02".into()));
        assert_eq!(
            c(46144.5, DateTime),
            Cell::DateTime("2026-05-02 12:00:00".into())
        );
        assert_eq!(
            c(46_144.999_999_999, DateTime),
            Cell::Date("2026-05-03".into())
        );
        assert_eq!(c(0.25, DateTime), Cell::Time("06:00:00".into()));
        assert_eq!(c(1.5, TimeDelta), Cell::Text("36:00:00".into()));
        assert_eq!(
            c(0.5 + 0.0005 / 86_400.0 * 1000.0, TimeDelta),
            Cell::Time("12:00:00.500000".into())
        );
        assert_eq!(c(60.0, DateTime), Cell::Text("1900-02-29".into()));
        assert_eq!(
            classify_iso("2026-05-01T00:00:00"),
            Cell::Date("2026-05-01".into())
        );
        assert_eq!(
            classify_iso("2026-05-01T08:30:00Z"),
            Cell::DateTime("2026-05-01 08:30:00".into())
        );
    }

    #[test]
    fn header_names_are_normalized_and_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.xlsx");
        let mut wb = rust_xlsxwriter::Workbook::new();
        let ws = wb.add_worksheet();
        for (c, h) in ["  Order   Id ", "", "amount", "AMOUNT", "amount_2", "when"]
            .iter()
            .enumerate()
        {
            if !h.is_empty() {
                ws.write_string(0, c as u16, *h).unwrap();
            }
        }
        ws.write_number(1, 0, 1).unwrap();
        ws.write_string(1, 2, "12.50").unwrap();
        ws.write_number(1, 3, 1.5).unwrap();
        ws.write_boolean(1, 4, true).unwrap();
        ws.write_string(1, 5, "secret-text").unwrap();
        ws.write_number(2, 0, 2).unwrap();
        ws.write_number(2, 2, 3).unwrap();
        ws.write_boolean(2, 4, false).unwrap();
        let date = rust_xlsxwriter::Format::new().set_num_format("yyyy-mm-dd");
        ws.write_datetime_with_format(
            2,
            5,
            rust_xlsxwriter::ExcelDateTime::from_ymd(2026, 3, 4).unwrap(),
            &date,
        )
        .unwrap();
        wb.save(&path).unwrap();

        let t = read_excel(&path, &ExcelOptions::default()).unwrap();
        let names: Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Order Id", "column_2", "amount", "AMOUNT_3", "amount_2", "when"
            ]
        );
        let types: Vec<Type> = t.columns.iter().map(|c| c.inferred).collect();
        assert_eq!(
            types,
            [
                Type::Int,
                Type::String,
                Type::Decimal(18, 1),
                Type::Decimal(18, 1),
                Type::Bool,
                Type::String
            ]
        );
        // numbers stored as text are staged as written, whatever the column's type
        assert_eq!(t.rows[0][2].as_deref(), Some("12.50"));
        assert_eq!(t.rows[1][5].as_deref(), Some("2026-03-04"));
        assert_eq!(t.rows[1][1], None);
        let w = warnings(&t);
        assert!(
            w.iter()
                .any(|m| m.contains("blank header cell B1 named column_2")),
            "{w:#?}"
        );
        assert!(
            w.iter()
                .any(|m| m.contains("`AMOUNT` at D1 (first at C1) renamed `AMOUNT_3`")),
            "{w:#?}"
        );
        let mixed = w.iter().find(|m| m.starts_with("column when:")).unwrap();
        assert!(
            mixed.contains("1 text cell") && mixed.contains("1 date cell"),
            "{mixed}"
        );
        assert!(!mixed.contains("secret"), "{mixed}");
        assert!(
            t.notes
                .iter()
                .any(|n| n.message.contains("1 number stored as text (at: C2)")),
            "{:#?}",
            t.notes
        );

        let headerless = read_excel(
            &path,
            &ExcelOptions {
                header: false,
                ..ExcelOptions::default()
            },
        )
        .unwrap();
        assert_eq!(headerless.columns[0].name, "column_1");
        assert_eq!(headerless.rows.len(), 3);
        assert_eq!(headerless.columns[0].inferred, Type::String);
    }
}
