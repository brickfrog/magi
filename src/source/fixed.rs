//! Fixed-width (`fixed_width`) source reader.
//!
//! Mainframe and legacy exports: one record per line, each field at a fixed byte position. The
//! positions come from a layout file, a CSV with the columns `field`, `start` (1-based byte),
//! `length` (bytes) and optionally `type`:
//! - `A` (the default): text; the padding blanks around it are removed, a blank field is empty.
//! - `N`: a whole number, usually zero-padded (`000123`); read as `int`.
//! - `D`: a date written `YYYYMMDD`; all zeros (`00000000`) or blanks mean no date.
//! - `M`: an amount in cents with the decimal point implied (`01500` is 15.00); read as a decimal
//!   with 2 places.
//!
//! A value that does not fit its type (letters in an `N` field, `20261399` in a `D` field) is
//! handed over as written, so staging lists it in the source's rejects.
//!
//! Records are the file's lines (LF or CRLF). Fields are cut from each record's bytes and then
//! decoded, so a single-byte encoding (`encoding: "cp1252"`) never shifts a position. `record:`
//! keeps only the records that start with the given code (e.g. `"D"` for detail records, leaving
//! out the header and trailer); every kept record must be as long as the others and reach the
//! layout's last byte, so a misaligned file stops the run instead of reading shifted fields.
//! Messages name lines and fields, never values.

use std::path::{Path, PathBuf};

use encoding_rs::Encoding;

use crate::semantic::types::Type;
use crate::source::SourceError;
use crate::source::staged::{StagedColumn, StagedTable};

/// Where the fields are and how the file is encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixedWidthOptions {
    /// The layout CSV (`field,start,length[,type]`).
    pub layout: PathBuf,
    /// A WHATWG encoding label (`utf-8`, `cp1252`, `latin1`, ...); `None` is UTF-8.
    pub encoding: Option<String>,
    /// Keep only records starting with this code.
    pub record: Option<String>,
    /// Fields whose blank values take the nearest non-blank value above them.
    pub fill_down: Vec<String>,
    /// Name of the column holding each record's 1-based data row number (added at staging).
    pub row_number: Option<String>,
}

/// How a field's bytes are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Number,
    Date,
    Cents,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    name: String,
    /// 0-based byte offset.
    start: usize,
    len: usize,
    kind: Kind,
}

/// Type codes a layout's `type` column may use.
const KINDS: &str = "A (text), N (number), D (date YYYYMMDD) or M (cents)";

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |f| f.to_string_lossy().into_owned(),
    )
}

/// The encoding `label` names; UTF-8 without one.
pub fn encoding(label: Option<&str>) -> Result<&'static Encoding, String> {
    let Some(label) = label else {
        return Ok(encoding_rs::UTF_8);
    };
    let enc = Encoding::for_label(label.trim().as_bytes())
        .ok_or_else(|| format!("unknown encoding `{label}`"))?;
    // positions count bytes: only encodings where ASCII (spaces, digits, record codes) is one byte
    if enc != encoding_rs::UTF_8 && !enc.is_single_byte() {
        return Err(format!(
            "encoding `{label}` is not supported for fixed-width records (use UTF-8 or a single-byte encoding such as cp1252 or latin1)"
        ));
    }
    Ok(enc)
}

/// Split one CSV line of the layout file (fields may be wrapped in double quotes).
fn split_csv_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter().map(|s| s.trim().to_string()).collect()
}

/// The fields of a layout file, in file order.
fn read_layout(path: &Path) -> Result<Vec<Field>, SourceError> {
    let name = file_name(path);
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("cannot read layout `{name}`: {e}"))?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut lines = text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty());
    let Some((_, header)) = lines.next() else {
        return Err(format!("layout `{name}` is empty").into());
    };
    let header: Vec<String> = split_csv_line(header)
        .into_iter()
        .map(|h| h.to_lowercase())
        .collect();
    let at = |col: &str| header.iter().position(|h| h == col);
    let (Some(fi), Some(si), Some(li)) = (at("field"), at("start"), at("length")) else {
        return Err(SourceError {
            code: "M200",
            message: format!("layout `{name}` needs the columns `field`, `start` and `length`"),
            help: Some(
                "the first line names the columns: `field,start,length,type` (`type` is optional)"
                    .into(),
            ),
        });
    };
    let ti = at("type");
    let mut fields: Vec<Field> = Vec::new();
    for (i, line) in lines {
        let n = i + 1;
        let cells = split_csv_line(line);
        let cell = |k: usize| cells.get(k).map(String::as_str).unwrap_or("");
        let field = cell(fi).split_whitespace().collect::<Vec<_>>().join(" ");
        if field.is_empty() {
            return Err(format!("layout `{name}` line {n}: the field has no name").into());
        }
        let number = |k: usize, what: &str| -> Result<usize, SourceError> {
            match cell(k).parse::<usize>() {
                Ok(v) if v >= 1 => Ok(v),
                _ => Err(format!(
                    "layout `{name}` line {n}: `{what}` of `{field}` is not a whole number of at least 1"
                )
                .into()),
            }
        };
        let start = number(si, "start")?;
        let len = number(li, "length")?;
        let kind = match ti.map(cell).unwrap_or("").to_uppercase().as_str() {
            "" | "A" => Kind::Text,
            "N" => Kind::Number,
            "D" => Kind::Date,
            "M" => Kind::Cents,
            _ => {
                return Err(SourceError {
                    code: "M200",
                    message: format!("layout `{name}` line {n}: unknown type of `{field}`"),
                    help: Some(format!("a field's type is {KINDS}")),
                });
            }
        };
        if let Some(prev) = fields
            .iter()
            .find(|f| f.name.to_lowercase() == field.to_lowercase())
        {
            return Err(format!("layout `{name}` line {n}: `{}` is named twice", prev.name).into());
        }
        let f = Field {
            name: field,
            start: start - 1,
            len,
            kind,
        };
        if let Some(prev) = fields
            .iter()
            .find(|p| f.start < p.start + p.len && p.start < f.start + f.len)
        {
            return Err(format!(
                "layout `{name}` line {n}: `{}` overlaps `{}`",
                f.name, prev.name
            )
            .into());
        }
        fields.push(f);
    }
    if fields.is_empty() {
        return Err(format!("layout `{name}` lists no fields").into());
    }
    Ok(fields)
}

/// Reads the records of the fixed-width file at `path`.
pub fn read_fixed(path: &Path, opts: &FixedWidthOptions) -> Result<StagedTable, SourceError> {
    let fields = read_layout(&opts.layout)?;
    let enc = encoding(opts.encoding.as_deref())?;
    let name = file_name(path);
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read `{name}`: {e}"))?;
    let bytes = match bytes.strip_prefix(b"\xEF\xBB\xBF") {
        Some(rest) if enc == encoding_rs::UTF_8 => rest,
        _ => &bytes[..],
    };
    let needed = fields.iter().map(|f| f.start + f.len).max().unwrap_or(0);
    let code = opts.record.as_deref().map(str::as_bytes);

    let mut table = StagedTable::default();
    let mut lines = bytes.split(|&b| b == b'\n').enumerate().peekable();
    // the length of the first kept record, and its line
    let mut width: Option<(usize, usize)> = None;
    while let Some((i, line)) = lines.next() {
        // the empty piece after the last line break is no record
        if line.is_empty() && lines.peek().is_none() {
            break;
        }
        let n = i + 1;
        let record = line.strip_suffix(b"\r").unwrap_or(line);
        if code.is_some_and(|c| !record.starts_with(c)) {
            continue;
        }
        match width {
            None => width = Some((record.len(), n)),
            Some((w, first)) if w != record.len() => {
                return Err(SourceError {
                    code: "M200",
                    message: format!(
                        "`{name}` line {n}: the record is {} bytes, the one on line {first} is {w}",
                        record.len()
                    ),
                    help: Some(
                        "every record of a fixed-width file has the same length; a shorter or longer one would read its fields from the wrong positions"
                            .into(),
                    ),
                });
            }
            Some(_) => {}
        }
        if record.len() < needed {
            return Err(SourceError {
                code: "M200",
                message: format!(
                    "`{name}` line {n}: the record is {} bytes, the layout reads up to byte {needed}",
                    record.len()
                ),
                help: Some(match &opts.record {
                    None => "is this a header or trailer record? `record: \"D\"` keeps only the records that start with a code".into(),
                    Some(_) => "check the layout against the file".into(),
                }),
            });
        }
        let mut row = Vec::with_capacity(fields.len());
        for f in &fields {
            let raw = &record[f.start..f.start + f.len];
            let text = enc
                .decode_without_bom_handling_and_without_replacement(raw)
                .ok_or_else(|| {
                    format!(
                        "`{name}` line {n}: field `{}` is not valid {} text",
                        f.name,
                        enc.name()
                    )
                })?;
            row.push(value(&text, f.kind));
        }
        table.rows.push(row);
        table.source_rows.push(n as u64);
    }
    // `fill_down` fields (a name the layout does not have is reported by the resolver, M215)
    for (i, f) in fields.iter().enumerate() {
        if !opts
            .fill_down
            .iter()
            .any(|c| c.to_lowercase() == f.name.to_lowercase())
        {
            continue;
        }
        let mut last: Option<String> = None;
        for row in &mut table.rows {
            match &row[i] {
                Some(v) => last = Some(v.clone()),
                None => row[i] = last.clone(),
            }
        }
    }
    let clean = |i: usize, ty: Type| {
        table.rows.iter().filter_map(|r| r[i].as_deref()).all(|v| {
            let d = v.strip_prefix('-').unwrap_or(v);
            match ty {
                Type::Int => {
                    !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()) && v.len() <= 18
                }
                _ => d.split_once('.').is_some_and(|(w, c)| {
                    !w.is_empty() && (w.bytes().chain(c.bytes())).all(|b| b.is_ascii_digit())
                }),
            }
        })
    };
    let columns = fields
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let ty = match f.kind {
                Kind::Text => Type::String,
                Kind::Number => Type::Int,
                Kind::Date => Type::Date,
                // 2 decimal places, and at least one digit before them
                Kind::Cents => Type::Decimal((f.len.max(3) as u8).min(38), 2),
            };
            StagedColumn {
                name: f.name.clone(),
                inferred: ty,
                // what a column declared numeric is checked against: only when every value is one
                exact: (matches!(f.kind, Kind::Number | Kind::Cents) && clean(i, ty)).then_some(ty),
            }
        })
        .collect();
    table.columns = columns;
    Ok(table)
}

/// A field's canonical text (see [`crate::source::staged`]); `None` when blank. Text that does not
/// fit the field's type is kept as written, for staging to reject.
fn value(text: &str, kind: Kind) -> Option<String> {
    let t = text.trim_matches(' ');
    if t.is_empty() {
        return None;
    }
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    Some(match kind {
        Kind::Text | Kind::Number => t.to_string(),
        Kind::Date if t.bytes().all(|b| b == b'0') => return None,
        Kind::Date if t.len() == 8 && digits(t) => format!("{}-{}-{}", &t[..4], &t[4..6], &t[6..]),
        Kind::Date => t.to_string(),
        Kind::Cents => {
            let (sign, d) = match t.strip_prefix('-') {
                Some(rest) => ("-", rest),
                None => ("", t.strip_prefix('+').unwrap_or(t)),
            };
            if !digits(d) {
                return Some(t.to_string());
            }
            let d = format!("{d:0>3}");
            let (whole, cents) = d.split_at(d.len() - 2);
            let whole = whole.trim_start_matches('0');
            format!(
                "{sign}{}.{cents}",
                if whole.is_empty() { "0" } else { whole }
            )
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.path().join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn opts(layout: PathBuf) -> FixedWidthOptions {
        FixedWidthOptions {
            layout,
            encoding: None,
            record: None,
            fill_down: Vec::new(),
            row_number: None,
        }
    }

    #[test]
    fn values_follow_the_field_type() {
        assert_eq!(value("  AB C  ", Kind::Text).as_deref(), Some("AB C"));
        assert_eq!(value("    ", Kind::Text), None);
        assert_eq!(value("000123", Kind::Number).as_deref(), Some("000123"));
        assert_eq!(value("20260131", Kind::Date).as_deref(), Some("2026-01-31"));
        assert_eq!(value("00000000", Kind::Date), None);
        assert_eq!(value("2026013X", Kind::Date).as_deref(), Some("2026013X"));
        assert_eq!(value("01500", Kind::Cents).as_deref(), Some("15.00"));
        assert_eq!(value("00005", Kind::Cents).as_deref(), Some("0.05"));
        assert_eq!(value("-7", Kind::Cents).as_deref(), Some("-0.07"));
        assert_eq!(value("1O", Kind::Cents).as_deref(), Some("1O"));
    }

    #[test]
    fn cp1252_fields_are_cut_by_byte() {
        let dir = tempfile::tempdir().unwrap();
        let layout = write(
            &dir,
            "l.csv",
            b"field,start,length,type\nkind,1,1\nname,2,5,A\nday,7,8,D\nfee,15,4,M\n",
        );
        // `MU\xD1OZ` is MUÑOZ in cp1252: 5 bytes, 5 characters
        let file = write(
            &dir,
            "f.txt",
            b"H header\r\nDMU\xD1OZ202601310150\r\nDAB   000000000000\r\nT0000002\r\n",
        );
        let mut o = opts(layout);
        o.encoding = Some("cp1252".into());
        o.record = Some("D".into());
        let t = read_fixed(&file, &o).unwrap();
        assert_eq!(t.source_rows, vec![2, 3]);
        assert_eq!(
            t.rows[0],
            vec![
                Some("D".into()),
                Some("MUÑOZ".into()),
                Some("2026-01-31".into()),
                Some("1.50".into())
            ]
        );
        assert_eq!(t.rows[1][1].as_deref(), Some("AB"));
        assert_eq!(t.rows[1][2], None);
        assert_eq!(t.columns[3].inferred, Type::Decimal(4, 2));
        // as UTF-8 the same bytes are not text
        o.encoding = None;
        let err = read_fixed(&file, &o).unwrap_err().message;
        assert!(err.contains("line 2") && err.contains("`name`"), "{err}");
    }

    #[test]
    fn misaligned_records_stop_the_read() {
        let dir = tempfile::tempdir().unwrap();
        let layout = write(&dir, "l.csv", b"field,start,length\na,1,2\nb,3,2\n");
        let short = write(&dir, "s.txt", b"aabb\naab\n");
        let err = read_fixed(&short, &opts(layout.clone()))
            .unwrap_err()
            .message;
        assert!(err.contains("line 2") && err.contains("line 1"), "{err}");
        let header = write(&dir, "h.txt", b"H\naabb\n");
        let err = read_fixed(&header, &opts(layout)).unwrap_err();
        assert!(err.message.contains("up to byte 4"), "{}", err.message);
        assert!(err.help.unwrap().contains("record:"));
    }

    #[test]
    fn layout_errors_name_the_line() {
        let dir = tempfile::tempdir().unwrap();
        let file = write(&dir, "f.txt", b"aabb\n");
        let bad = |layout: &[u8]| {
            let l = write(&dir, "l.csv", layout);
            read_fixed(&file, &opts(l)).unwrap_err().message
        };
        assert!(bad(b"name,from,to\na,1,2\n").contains("`field`, `start` and `length`"));
        assert!(bad(b"field,start,length\na,1,2\nb,2,2\n").contains("`b` overlaps `a`"));
        assert!(bad(b"field,start,length\na,1,2\nA,3,2\n").contains("named twice"));
        assert!(bad(b"field,start,length\na,0,2\n").contains("line 2"));
        assert!(bad(b"field,start,length,type\na,1,2,X\n").contains("unknown type"));
        assert!(
            encoding(Some("utf-16"))
                .unwrap_err()
                .contains("not supported")
        );
        assert!(encoding(Some("klingon")).unwrap_err().contains("unknown"));
    }
}
