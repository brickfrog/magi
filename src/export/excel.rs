//! Excel (`.xlsx`) export.
//!
//! Writes one workbook with one worksheet per [`Sheet`], in order. Each sheet gets a bold,
//! frozen header row, an autofilter over the data and column widths sized to the content.
//! Cells keep their types: numbers are numbers, dates/times are real Excel date values with an
//! ISO-like display format, booleans are booleans and nulls are empty cells.
//!
//! Representation limits (Excel stores every number as an IEEE double):
//! - [`Cell::Int`] values beyond ±2^53 would lose precision, so they are written as text. The
//!   export hands such ints (including those beyond i64) and decimals with more than 15
//!   significant digits over as text cells, and warns (M506) naming the column; so it does with
//!   times and timestamps finer than a millisecond (Excel's limit).
//! - Non-finite [`Cell::Float`] values (NaN, ±infinity) are written as text.
//! - Dates and timestamps outside Excel's range (1900-01-01..9999-12-31) are written as ISO text.
//! - Times and timestamps keep their milliseconds; the display format shows seconds.
//!
//! Output is byte-for-byte reproducible for identical input: the document creation time is
//! pinned and the archive uses fixed entry timestamps.

use std::path::Path;

use rust_xlsxwriter::{DocProperties, ExcelDateTime, Format, Workbook, Worksheet, XlsxError};

/// One typed cell value.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// A decimal value displayed with exactly `scale` fractional digits.
    Decimal {
        value: f64,
        scale: u8,
    },
    String(String),
    Date {
        year: i32,
        month: u32,
        day: u32,
    },
    Timestamp {
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
        second: u32,
        micros: u32,
    },
    Time {
        hour: u32,
        minute: u32,
        second: u32,
        micros: u32,
    },
}

/// A worksheet to write: a header row naming `columns`, then `rows` (each `columns.len()` long).
#[derive(Debug, Clone, PartialEq)]
pub struct Sheet {
    pub name: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
}

/// Rows available below the header row (Excel has 1,048,576 rows).
pub const MAX_DATA_ROWS: usize = 1_048_575;
/// Excel's column limit (`XFD`).
pub const MAX_COLUMNS: usize = 16_384;
const MAX_SHEET_NAME_CHARS: usize = 31;
/// Integers with magnitude above this cannot be stored exactly as an Excel number.
pub(crate) const MAX_EXACT_INT: i64 = 1 << 53;
const MIN_WIDTH: f64 = 8.0;
const MAX_WIDTH: f64 = 60.0;

/// Writes `sheets` into a new workbook at `path`, replacing any existing file.
pub fn write_xlsx(path: &Path, sheets: &[Sheet]) -> Result<(), String> {
    validate(sheets)?;
    let mut workbook = Workbook::new();
    let created = ExcelDateTime::from_ymd(2000, 1, 1).map_err(|e| e.to_string())?;
    workbook.set_properties(&DocProperties::new().set_creation_datetime(&created));
    let mut formats = Formats::new();
    for sheet in sheets {
        let worksheet = workbook.add_worksheet();
        write_sheet(worksheet, sheet, &mut formats)
            .map_err(|e| format!("sheet `{}`: {e}", sheet.name))?;
    }
    workbook
        .save(path)
        .map_err(|e| format!("cannot write `{}`: {e}", path.display()))
}

fn validate(sheets: &[Sheet]) -> Result<(), String> {
    if sheets.is_empty() {
        return Err("an Excel workbook needs at least one sheet".to_owned());
    }
    let mut seen: Vec<(String, &str)> = Vec::with_capacity(sheets.len());
    for sheet in sheets {
        let name = sheet.name.as_str();
        if name.trim().is_empty() {
            return Err("Excel sheet names cannot be empty".to_owned());
        }
        if name.chars().count() > MAX_SHEET_NAME_CHARS {
            return Err(format!(
                "Excel sheet name `{name}` is {} characters long; the limit is {MAX_SHEET_NAME_CHARS}",
                name.chars().count()
            ));
        }
        if let Some(c) = name
            .chars()
            .find(|c| matches!(c, '[' | ']' | ':' | '*' | '?' | '/' | '\\'))
        {
            return Err(format!(
                "Excel sheet name `{name}` contains `{c}`; sheet names cannot contain []:*?/\\"
            ));
        }
        if name.starts_with('\'') || name.ends_with('\'') {
            return Err(format!(
                "Excel sheet name `{name}` cannot start or end with an apostrophe"
            ));
        }
        if name.eq_ignore_ascii_case("history") {
            return Err(format!(
                "`{name}` is reserved by Excel and cannot be used as a sheet name"
            ));
        }
        let folded = name.to_lowercase();
        if let Some((_, first)) = seen.iter().find(|(f, _)| *f == folded) {
            return Err(format!(
                "Excel sheet names must be unique ignoring case: `{name}` clashes with `{first}`"
            ));
        }
        seen.push((folded, name));
        if sheet.columns.len() > MAX_COLUMNS {
            return Err(format!(
                "sheet `{name}` has {} columns; Excel allows at most {MAX_COLUMNS}",
                sheet.columns.len()
            ));
        }
        if sheet.rows.len() > MAX_DATA_ROWS {
            return Err(format!(
                "sheet `{name}` has {} rows; Excel allows at most {MAX_DATA_ROWS} below the header — split the output or export CSV/Parquet",
                sheet.rows.len()
            ));
        }
        if let Some(i) = sheet
            .rows
            .iter()
            .position(|r| r.len() != sheet.columns.len())
        {
            return Err(format!(
                "sheet `{name}`: row {} has {} cells but there are {} columns",
                i + 1,
                sheet.rows[i].len(),
                sheet.columns.len()
            ));
        }
    }
    Ok(())
}

/// Cell formats shared across sheets.
struct Formats {
    header: Format,
    date: Format,
    timestamp: Format,
    time: Format,
    /// Decimal formats by scale.
    decimals: Vec<Option<Format>>,
}

impl Formats {
    fn new() -> Self {
        Self {
            header: Format::new().set_bold(),
            date: Format::new().set_num_format("yyyy-mm-dd"),
            timestamp: Format::new().set_num_format("yyyy-mm-dd hh:mm:ss"),
            time: Format::new().set_num_format("hh:mm:ss"),
            decimals: Vec::new(),
        }
    }

    fn decimal(&mut self, scale: u8) -> &Format {
        let i = usize::from(scale);
        if self.decimals.len() <= i {
            self.decimals.resize(i + 1, None);
        }
        self.decimals[i].get_or_insert_with(|| {
            let pattern = if scale == 0 {
                "0".to_owned()
            } else {
                format!("0.{}", "0".repeat(i))
            };
            Format::new().set_num_format(pattern)
        })
    }
}

fn write_sheet(ws: &mut Worksheet, sheet: &Sheet, formats: &mut Formats) -> Result<(), XlsxError> {
    ws.set_name(&sheet.name)?;
    if sheet.columns.is_empty() {
        return Ok(());
    }
    let mut widths: Vec<usize> = sheet
        .columns
        .iter()
        .map(|c| c.chars().count() + 3)
        .collect();
    for (c, name) in sheet.columns.iter().enumerate() {
        ws.write_string_with_format(0, c as u16, name, &formats.header)?;
    }
    for (r, row) in sheet.rows.iter().enumerate() {
        let r = r as u32 + 1;
        for (c, cell) in row.iter().enumerate() {
            let col = c as u16;
            let width = write_cell(ws, r, col, cell, formats)?;
            widths[c] = widths[c].max(width);
        }
    }
    for (c, width) in widths.iter().enumerate() {
        ws.set_column_width(c as u16, (*width as f64 + 1.0).clamp(MIN_WIDTH, MAX_WIDTH))?;
    }
    let last_col = (sheet.columns.len() - 1) as u16;
    ws.set_freeze_panes(1, 0)?;
    ws.autofilter(0, 0, sheet.rows.len() as u32, last_col)?;
    Ok(())
}

/// Writes one cell and returns its approximate display width in characters.
fn write_cell(
    ws: &mut Worksheet,
    r: u32,
    c: u16,
    cell: &Cell,
    formats: &mut Formats,
) -> Result<usize, XlsxError> {
    let width = match cell {
        Cell::Null => 0,
        Cell::Bool(b) => {
            ws.write_boolean(r, c, *b)?;
            5
        }
        Cell::Int(i) => {
            let text = i.to_string();
            if (-MAX_EXACT_INT..=MAX_EXACT_INT).contains(i) {
                ws.write_number(r, c, *i as f64)?;
            } else {
                ws.write_string(r, c, &text)?;
            }
            text.len()
        }
        Cell::Float(f) if f.is_finite() => {
            ws.write_number(r, c, *f)?;
            f.to_string().len().min(12)
        }
        Cell::Float(f) => {
            let text = f.to_string();
            ws.write_string(r, c, &text)?;
            text.len()
        }
        Cell::Decimal { value, scale } => {
            let format = formats.decimal(*scale);
            ws.write_number_with_format(r, c, *value, format)?;
            format!("{:.*}", usize::from(*scale), value).len()
        }
        Cell::String(s) => {
            ws.write_string(r, c, s)?;
            s.chars().count()
        }
        &Cell::Date { year, month, day } => match excel_date(year, month, day) {
            Some(date) => {
                ws.write_datetime_with_format(r, c, date, &formats.date)?;
                10
            }
            None => {
                let text = format!("{year:04}-{month:02}-{day:02}");
                ws.write_string(r, c, &text)?;
                text.len()
            }
        },
        &Cell::Timestamp {
            year,
            month,
            day,
            hour,
            minute,
            second,
            micros,
        } => {
            let seconds = f64::from(second) + f64::from(micros) / 1e6;
            match excel_date(year, month, day).and_then(|d| hms(d, hour, minute, seconds)) {
                Some(ts) => {
                    ws.write_datetime_with_format(r, c, ts, &formats.timestamp)?;
                    19
                }
                None => {
                    let mut text =
                        format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}");
                    if micros != 0 {
                        text.push_str(&format!(".{micros:06}"));
                    }
                    ws.write_string(r, c, &text)?;
                    text.len()
                }
            }
        }
        &Cell::Time {
            hour,
            minute,
            second,
            micros,
        } => {
            let seconds = f64::from(second) + f64::from(micros) / 1e6;
            let time = u16::try_from(hour)
                .ok()
                .zip(u8::try_from(minute).ok())
                .and_then(|(h, m)| ExcelDateTime::from_hms(h, m, seconds).ok());
            match time {
                Some(t) => {
                    ws.write_datetime_with_format(r, c, t, &formats.time)?;
                    8
                }
                None => {
                    let mut text = format!("{hour:02}:{minute:02}:{second:02}");
                    if micros != 0 {
                        text.push_str(&format!(".{micros:06}"));
                    }
                    ws.write_string(r, c, &text)?;
                    text.len()
                }
            }
        }
    };
    Ok(width)
}

fn excel_date(year: i32, month: u32, day: u32) -> Option<ExcelDateTime> {
    ExcelDateTime::from_ymd(
        u16::try_from(year).ok()?,
        u8::try_from(month).ok()?,
        u8::try_from(day).ok()?,
    )
    .ok()
}

fn hms(date: ExcelDateTime, hour: u32, minute: u32, seconds: f64) -> Option<ExcelDateTime> {
    date.and_hms(
        u16::try_from(hour).ok()?,
        u8::try_from(minute).ok()?,
        seconds,
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use calamine::{Data, Reader, open_workbook_auto};

    fn sheet(name: &str) -> Sheet {
        Sheet {
            name: name.to_owned(),
            columns: vec!["a".to_owned()],
            rows: vec![vec![Cell::Int(1)]],
        }
    }

    fn every_variant() -> Vec<Sheet> {
        vec![
            Sheet {
                name: "Matches".to_owned(),
                columns: [
                    "null",
                    "bool",
                    "int",
                    "big_int",
                    "float",
                    "decimal",
                    "string",
                    "date",
                    "timestamp",
                    "time",
                ]
                .map(String::from)
                .to_vec(),
                rows: vec![
                    vec![
                        Cell::Null,
                        Cell::Bool(true),
                        Cell::Int(-42),
                        Cell::Int(9_007_199_254_740_993),
                        Cell::Float(12.3),
                        Cell::Decimal {
                            value: 1234.5,
                            scale: 3,
                        },
                        Cell::String("hello".to_owned()),
                        Cell::Date {
                            year: 2026,
                            month: 7,
                            day: 1,
                        },
                        Cell::Timestamp {
                            year: 2026,
                            month: 7,
                            day: 1,
                            hour: 13,
                            minute: 45,
                            second: 30,
                            micros: 0,
                        },
                        Cell::Time {
                            hour: 8,
                            minute: 5,
                            second: 9,
                            micros: 250_000,
                        },
                    ],
                    vec![
                        Cell::Null,
                        Cell::Bool(false),
                        Cell::Int(7),
                        Cell::Int(-5),
                        Cell::Float(f64::NAN),
                        Cell::Decimal {
                            value: -0.5,
                            scale: 0,
                        },
                        Cell::String(String::new()),
                        Cell::Date {
                            year: 1800,
                            month: 1,
                            day: 2,
                        },
                        Cell::Timestamp {
                            year: 2026,
                            month: 1,
                            day: 2,
                            hour: 0,
                            minute: 0,
                            second: 1,
                            micros: 500_000,
                        },
                        Cell::Null,
                    ],
                ],
            },
            Sheet {
                name: "Summary".to_owned(),
                columns: vec!["n".to_owned()],
                rows: vec![vec![Cell::Int(2)]],
            },
        ]
    }

    #[test]
    fn round_trip_every_variant() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.xlsx");
        write_xlsx(&path, &every_variant()).unwrap();

        let mut wb = open_workbook_auto(&path).unwrap();
        assert_eq!(wb.sheet_names(), ["Matches", "Summary"]);
        let range = wb.worksheet_range("Matches").unwrap();
        assert_eq!(range.get_size(), (3, 10));
        let header: Vec<String> = (0..10)
            .map(|c| range.get((0, c)).unwrap().to_string())
            .collect();
        assert_eq!(header[0], "null");
        assert_eq!(header[9], "time");

        let row = |r: usize| {
            (0..10)
                .map(|c| range.get((r, c)).cloned().unwrap_or(Data::Empty))
                .collect::<Vec<_>>()
        };
        let r1 = row(1);
        assert_eq!(r1[0], Data::Empty);
        assert_eq!(r1[1], Data::Bool(true));
        assert_eq!(r1[2], Data::Float(-42.0));
        assert_eq!(r1[3], Data::String("9007199254740993".to_owned()));
        assert_eq!(r1[4], Data::Float(12.3));
        assert_eq!(r1[5], Data::Float(1234.5));
        assert_eq!(r1[6], Data::String("hello".to_owned()));
        let dt = |d: &Data| match d {
            Data::DateTime(dt) if dt.is_datetime() => dt.to_ymd_hms_milli(),
            other => panic!("expected a date cell, got {other:?}"),
        };
        assert_eq!(dt(&r1[7]), (2026, 7, 1, 0, 0, 0, 0));
        assert_eq!(dt(&r1[8]), (2026, 7, 1, 13, 45, 30, 0));
        // the time keeps its milliseconds
        match &r1[9] {
            Data::DateTime(t) => {
                let seconds = 8.0 * 3600.0 + 5.0 * 60.0 + 9.25;
                assert!((t.as_f64() - seconds / 86_400.0).abs() < 1e-9)
            }
            other => panic!("expected a time cell, got {other:?}"),
        }

        let r2 = row(2);
        assert_eq!(r2[1], Data::Bool(false));
        assert_eq!(r2[3], Data::Float(-5.0));
        assert_eq!(r2[4], Data::String("NaN".to_owned()));
        assert_eq!(r2[5], Data::Float(-0.5));
        assert_eq!(r2[7], Data::String("1800-01-02".to_owned()));
        assert_eq!(dt(&r2[8]), (2026, 1, 2, 0, 0, 1, 500));
        assert_eq!(r2[9], Data::Empty);

        let summary = wb.worksheet_range("Summary").unwrap();
        assert_eq!(summary.get((1, 0)), Some(&Data::Float(2.0)));
    }

    #[test]
    fn output_is_reproducible() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.xlsx"), dir.path().join("b.xlsx"));
        write_xlsx(&a, &every_variant()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        write_xlsx(&b, &every_variant()).unwrap();
        assert_eq!(std::fs::read(a).unwrap(), std::fs::read(b).unwrap());
    }

    #[test]
    fn rejects_bad_sheet_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.xlsx");
        let err = |sheets: &[Sheet]| write_xlsx(&path, sheets).unwrap_err();
        assert!(err(&[sheet("")]).contains("cannot be empty"));
        assert!(err(&[sheet(&"x".repeat(32))]).contains("limit is 31"));
        assert!(err(&[sheet("a/b")]).contains("contains `/`"));
        assert!(err(&[sheet("q?")]).contains("contains `?`"));
        assert!(err(&[sheet("Data"), sheet("DATA")]).contains("unique ignoring case"));
        assert!(err(&[]).contains("at least one sheet"));
        assert!(!path.exists());
        write_xlsx(&path, &[sheet(&"x".repeat(31))]).unwrap();
    }

    #[test]
    fn rejects_ragged_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = sheet("Data");
        s.rows.push(vec![Cell::Int(1), Cell::Int(2)]);
        let err = write_xlsx(&dir.path().join("x.xlsx"), &[s]).unwrap_err();
        assert!(
            err.contains("row 2 has 2 cells but there are 1 columns"),
            "{err}"
        );
    }

    #[test]
    fn rejects_too_many_rows() {
        let dir = tempfile::tempdir().unwrap();
        let s = Sheet {
            name: "Big".to_owned(),
            columns: Vec::new(),
            rows: vec![Vec::new(); MAX_DATA_ROWS + 1],
        };
        let err = write_xlsx(&dir.path().join("x.xlsx"), &[s]).unwrap_err();
        assert!(
            err.contains("1048576 rows") && err.contains("at most 1048575"),
            "{err}"
        );
    }
}
