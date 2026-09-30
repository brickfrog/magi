//! Query extraction over ODBC.
//!
//! [`extract`] runs a query through the system ODBC driver manager and hands the result set over
//! as a [`StagedTable`] of canonical text (see [`crate::source::staged`]); [`describe`] prepares
//! the query and reports its columns without executing it or fetching rows.
//!
//! Values are fetched as text in column-wise batches. Column types come from the driver's
//! metadata (`SQLDescribeCol`). Drivers with weak typing (SQLite) may report a type that some
//! values do not fit; such a column is re-typed from its values and a warning is attached, so
//! every canonical value casts cleanly to the column's inferred type.
//!
//! Canonical forms beyond the staged.rs list: binary values are `\xAB\xCD` escapes (DuckDB's
//! text form for `BLOB`); SQL NULL is `None` while an empty string stays `Some("")`, because SQL
//! sources distinguish the two.
//!
//! Diagnostics never contain cell values, the connection string or secrets from it.

use std::sync::LazyLock;

use odbc_api::buffers::TextRowSet;
use odbc_api::{Connection, ConnectionOptions, Cursor, DataType, Environment, ResultSetMetadata};

use crate::semantic::types::Type;
use crate::source::staged::{StagedColumn, StagedTable};

/// Upper bound on rows per fetched batch.
const MAX_BATCH_ROWS: usize = 1000;
/// Memory budget for one batch of text buffers; wide rows get smaller batches.
const BATCH_BYTES: usize = 16 << 20;
/// Largest value (in bytes) a batch buffer holds; longer values switch to row-by-row fetching.
const MAX_BUFFERED_VALUE: usize = 4096;
/// Largest DuckDB decimal precision.
const MAX_DECIMAL_PRECISION: usize = 38;
/// Sub-second digits DuckDB keeps for `TIME` / `TIMESTAMP`.
const MAX_FRACTION_DIGITS: usize = 6;

/// How to reach an ODBC data source.
pub enum Target {
    /// `SQLConnect` to a DSN: the credentials are its own arguments, never part of a string a
    /// driver parses.
    Dsn {
        dsn: String,
        user: Option<String>,
        password: Option<String>,
    },
    /// `SQLDriverConnect` with a complete connection string.
    ConnectionString(String),
}

impl Target {
    /// Removes the secrets this target carries from a driver or driver-manager message.
    fn redact(&self, message: &str) -> String {
        match self {
            Target::ConnectionString(cs) => redact(message, cs),
            Target::Dsn { password, .. } => {
                let mut out = message.to_owned();
                if let Some(p) = password.as_deref().filter(|p| !p.is_empty()) {
                    out = out.replace(p, "***");
                }
                redact_secret_assignments(&out)
            }
        }
    }
}

/// Runs `query` and returns its complete result set as canonical text.
///
/// `source_rows` numbers rows 1..n in fetch order; that order is only stable if the query
/// orders its result.
pub fn extract(target: &Target, query: &str) -> Result<StagedTable, String> {
    let connection = connect(target)?;
    let fetched = match fetch_batched(&connection, query) {
        Ok(Some(fetched)) => Ok(fetched),
        // A value did not fit its batch buffer: run the query again, reading each value in full.
        Ok(None) => fetch_row_by_row(&connection, query),
        Err(failure) => Err(failure),
    }
    .map_err(|failure| failure.message(target))?;
    Ok(build_table(fetched))
}

/// Prepares `query` and describes its result columns without executing it or fetching rows.
pub fn describe(target: &Target, query: &str) -> Result<Vec<StagedColumn>, String> {
    let connection = connect(target)?;
    let describe = || {
        let mut prepared = connection
            .prepare(query)
            .map_err(|e| Failure::Odbc("preparing the query", e))?;
        read_columns(&mut prepared)
    };
    let columns = describe().map_err(|failure| failure.message(target))?;
    Ok(columns
        .into_iter()
        .enumerate()
        .map(|(index, column)| StagedColumn {
            name: column_name(&column.name, index).into_owned(),
            inferred: map_type(column.data_type),
            exact: None,
        })
        .collect())
}

/// Removes secrets from a driver or driver-manager message: every occurrence of the connection
/// string itself, the values of secret attributes it contains (`PWD`, `Password`, tokens, ...)
/// and any `PWD=...`-style assignment the driver echoes back.
pub fn redact(message: &str, connection_string: &str) -> String {
    let mut out = message.to_owned();
    let full = connection_string.trim();
    for form in [full, full.trim_end_matches(';')] {
        if !form.is_empty() {
            out = out.replace(form, "<connection string>");
        }
    }
    for (key, value) in connection_attributes(connection_string) {
        if is_secret_key(key) && !value.is_empty() {
            out = out.replace(value, "***");
        }
    }
    redact_secret_assignments(&out)
}

// ---------------------------------------------------------------------------------------------
// Connection and fetching

/// The process-wide ODBC environment, created on first use.
static ENVIRONMENT: LazyLock<Result<Environment, String>> = LazyLock::new(|| {
    Environment::new().map_err(|e| {
        format!(
            "cannot initialise the ODBC driver manager: {}",
            one_line(&e)
        )
    })
});

fn environment() -> Result<&'static Environment, String> {
    ENVIRONMENT.as_ref().map_err(Clone::clone)
}

fn connect(target: &Target) -> Result<Connection<'static>, String> {
    let env = environment()?;
    let options = ConnectionOptions::default();
    match target {
        Target::Dsn {
            dsn,
            user,
            password,
        } => env.connect(
            dsn,
            user.as_deref().unwrap_or_default(),
            password.as_deref().unwrap_or_default(),
            options,
        ),
        Target::ConnectionString(cs) => env.connect_with_connection_string(cs, options),
    }
    .map_err(|e| Failure::Odbc("connecting to the data source", e).message(target))
}

enum Failure {
    Odbc(&'static str, odbc_api::Error),
    NoResultSet,
}

impl Failure {
    fn message(&self, target: &Target) -> String {
        match self {
            Failure::Odbc(context, error) => {
                format!(
                    "ODBC error while {context}: {}",
                    target.redact(&one_line(error))
                )
            }
            Failure::NoResultSet => "the query does not return a result set".to_owned(),
        }
    }
}

fn one_line(error: &odbc_api::Error) -> String {
    let text = error.to_string();
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

struct ColumnMeta {
    name: String,
    data_type: DataType,
}

fn read_columns(meta: &mut impl ResultSetMetadata) -> Result<Vec<ColumnMeta>, Failure> {
    let context = |e| Failure::Odbc("reading result columns", e);
    let count = meta.num_result_cols().map_err(context)?;
    let count = u16::try_from(count).unwrap_or(0);
    if count == 0 {
        return Err(Failure::NoResultSet);
    }
    (1..=count)
        .map(|number| {
            Ok(ColumnMeta {
                name: meta.col_name(number).map_err(context)?,
                data_type: meta.col_data_type(number).map_err(context)?,
            })
        })
        .collect()
}

fn column_name(reported: &str, index: usize) -> std::borrow::Cow<'_, str> {
    if reported.trim().is_empty() {
        format!("column{}", index + 1).into()
    } else {
        reported.into()
    }
}

/// Result set as fetched: text exactly as the driver returned it (lossily decoded).
struct Fetched {
    columns: Vec<ColumnMeta>,
    rows: Vec<Vec<Option<String>>>,
    /// Per column: number of values that were not valid UTF-8.
    invalid_utf8: Vec<usize>,
}

impl Fetched {
    fn new(columns: Vec<ColumnMeta>) -> Self {
        let invalid_utf8 = vec![0; columns.len()];
        Self {
            columns,
            rows: Vec::new(),
            invalid_utf8,
        }
    }

    fn text(&mut self, column: usize, bytes: &[u8]) -> String {
        match std::str::from_utf8(bytes) {
            Ok(text) => text.to_owned(),
            Err(_) => {
                self.invalid_utf8[column] += 1;
                String::from_utf8_lossy(bytes).into_owned()
            }
        }
    }
}

fn execute<'c>(
    connection: &'c Connection<'static>,
    query: &str,
) -> Result<impl Cursor + 'c, Failure> {
    connection
        .execute(query, (), None)
        .map_err(|e| Failure::Odbc("executing the query", e))?
        .ok_or(Failure::NoResultSet)
}

/// Fetches in column-wise text batches. `Ok(None)` if a value was too long for its buffer.
fn fetch_batched(
    connection: &Connection<'static>,
    query: &str,
) -> Result<Option<Fetched>, Failure> {
    let mut cursor = execute(connection, query)?;
    let columns = read_columns(&mut cursor)?;
    let lengths: Vec<usize> = columns.iter().map(|c| buffer_length(c.data_type)).collect();
    let row_bytes: usize = lengths.iter().map(|len| len + 1).sum();
    let batch_rows = (BATCH_BYTES / row_bytes.max(1)).clamp(1, MAX_BATCH_ROWS);
    let buffer = TextRowSet::from_max_str_lens(batch_rows, lengths)
        .map_err(|e| Failure::Odbc("allocating fetch buffers", e))?;
    let mut block = cursor
        .bind_buffer(buffer)
        .map_err(|e| Failure::Odbc("binding fetch buffers", e))?;
    let mut fetched = Fetched::new(columns);
    let width = fetched.columns.len();
    loop {
        let batch = match block.fetch_with_truncation_check(true) {
            Ok(Some(batch)) => batch,
            Ok(None) => break,
            Err(odbc_api::Error::TooLargeValueForBuffer { .. }) => return Ok(None),
            Err(e) => return Err(Failure::Odbc("fetching rows", e)),
        };
        for row in 0..batch.num_rows() {
            let cells = (0..width)
                .map(|column| {
                    batch
                        .at(column, row)
                        .map(|bytes| fetched.text(column, bytes))
                })
                .collect();
            fetched.rows.push(cells);
        }
    }
    Ok(Some(fetched))
}

/// Fetches one row at a time, reading every value in full regardless of its length.
fn fetch_row_by_row(connection: &Connection<'static>, query: &str) -> Result<Fetched, Failure> {
    let mut cursor = execute(connection, query)?;
    let mut fetched = Fetched::new(read_columns(&mut cursor)?);
    let width = u16::try_from(fetched.columns.len()).unwrap_or(u16::MAX);
    let context = |e| Failure::Odbc("fetching rows", e);
    let mut bytes = Vec::new();
    while let Some(mut row) = cursor.next_row().map_err(context)? {
        let mut cells = Vec::with_capacity(usize::from(width));
        for number in 1..=width {
            let present = row.get_text(number, &mut bytes).map_err(context)?;
            cells.push(present.then(|| fetched.text(usize::from(number - 1), &bytes)));
        }
        fetched.rows.push(cells);
    }
    Ok(fetched)
}

/// Text buffer size (bytes, excluding the terminator) for one value of a column.
fn buffer_length(data_type: DataType) -> usize {
    data_type
        .utf8_len()
        .or_else(|| data_type.display_size())
        .map_or(MAX_BUFFERED_VALUE, |len| len.get().min(MAX_BUFFERED_VALUE))
        .max(1)
}

// ---------------------------------------------------------------------------------------------
// Types and canonical text

/// Maps driver metadata to a MAGI type.
fn map_type(data_type: DataType) -> Type {
    match data_type {
        DataType::TinyInt | DataType::SmallInt | DataType::Integer | DataType::BigInt => Type::Int,
        DataType::Numeric { precision, scale } | DataType::Decimal { precision, scale } => {
            decimal_type(precision, scale)
        }
        DataType::Float { .. } | DataType::Real | DataType::Double => Type::Float,
        DataType::Bit => Type::Bool,
        DataType::Date => Type::Date,
        DataType::Time { .. } => Type::Time,
        DataType::Timestamp { .. } => Type::Timestamp,
        DataType::Binary { .. } | DataType::Varbinary { .. } | DataType::LongVarbinary { .. } => {
            Type::Binary
        }
        // ODBC 2 date/time codes, still reported by some drivers.
        DataType::Other { data_type, .. } => match data_type.0 {
            9 => Type::Date,
            // SQL_C_SBIGINT / SQL_C_UBIGINT: C type codes some drivers (sqliteodbc) report for
            // BIGINT columns instead of SQL_BIGINT.
            -25 | -27 => Type::Int,
            10 => Type::Time,
            11 => Type::Timestamp,
            _ => Type::String,
        },
        DataType::Char { .. }
        | DataType::WChar { .. }
        | DataType::Varchar { .. }
        | DataType::WVarchar { .. }
        | DataType::LongVarchar { .. }
        | DataType::WLongVarchar { .. }
        | DataType::Unknown => Type::String,
    }
}

/// `DECIMAL(precision, scale)` as reported by a driver, clamped to what DuckDB supports. An
/// unknown precision (0) becomes the widest decimal; a negative scale counts as integer digits.
fn decimal_type(precision: usize, scale: i16) -> Type {
    let frac = usize::try_from(scale.max(0))
        .unwrap_or(0)
        .min(MAX_DECIMAL_PRECISION);
    if precision == 0 {
        return decimal(MAX_DECIMAL_PRECISION, frac);
    }
    let int = if scale < 0 {
        precision + usize::from(scale.unsigned_abs())
    } else {
        precision.saturating_sub(frac)
    };
    decimal((int + frac).clamp(1, MAX_DECIMAL_PRECISION), frac)
}

fn decimal(precision: usize, scale: usize) -> Type {
    // Callers keep both within 0..=38, so the conversions cannot fail.
    let precision = u8::try_from(precision.min(MAX_DECIMAL_PRECISION)).unwrap_or(38);
    let scale = u8::try_from(scale).unwrap_or(0).min(precision);
    Type::Decimal(precision, scale)
}

fn build_table(fetched: Fetched) -> StagedTable {
    let Fetched {
        columns,
        mut rows,
        invalid_utf8,
    } = fetched;
    let mut table = StagedTable::default();
    for (index, column) in columns.iter().enumerate() {
        let name = column_name(&column.name, index).into_owned();
        if name != column.name {
            table.note(format!(
                "result column {} has no name; it is called `{name}`",
                index + 1
            ));
        }
        if invalid_utf8[index] > 0 {
            table.warn(format!(
                "column `{name}`: {} value(s) are not valid UTF-8; invalid bytes were replaced",
                invalid_utf8[index]
            ));
        }
        let inferred = settle_column(
            &mut rows,
            index,
            &name,
            map_type(column.data_type),
            &mut table,
        );
        table.columns.push(StagedColumn {
            name,
            inferred,
            exact: None,
        });
    }
    table.source_rows = (1..=rows.len() as u64).collect();
    table.rows = rows;
    table
}

/// Chooses the column's type (the reported one unless some values do not fit it) and rewrites
/// every value of the column into canonical text for that type.
fn settle_column(
    rows: &mut [Vec<Option<String>>],
    column: usize,
    name: &str,
    reported: Type,
    table: &mut StagedTable,
) -> Type {
    let values = || rows.iter().filter_map(|row| row[column].as_deref());
    let settled = if let Type::Decimal(precision, scale) = reported {
        match fit_decimal(values()) {
            Ok((int, frac)) => {
                let int = int.max(usize::from(precision - scale));
                let frac = frac.max(usize::from(scale));
                if int + frac <= MAX_DECIMAL_PRECISION {
                    let widened = decimal(int + frac, frac);
                    if widened != reported {
                        table.note(format!(
                            "column `{name}`: the driver reported {reported} but the values need \
                             {widened}; widened"
                        ));
                    }
                    widened
                } else {
                    table.warn(format!(
                        "column `{name}`: values need more than {MAX_DECIMAL_PRECISION} digits; \
                         inferred as float, which may round them"
                    ));
                    Type::Float
                }
            }
            Err(bad) => retype(values(), bad, name, reported, "decimal numbers", table),
        }
    } else {
        let bad = values()
            .filter(|value| canonical(reported, value).is_none())
            .count();
        if bad == 0 {
            reported
        } else {
            retype(
                values(),
                bad,
                name,
                reported,
                &format!("{reported} values"),
                table,
            )
        }
    };

    let mut truncated = 0usize;
    for cell in rows.iter_mut().filter_map(|row| row[column].as_mut()) {
        if let Some(value) = canonical(settled, cell) {
            truncated += usize::from(value.truncated);
            *cell = value.text;
        }
    }
    if truncated > 0 {
        table.warn(format!(
            "column `{name}`: {truncated} value(s) have more than {MAX_FRACTION_DIGITS} sub-second \
             digits; truncated to microseconds"
        ));
    }
    settled
}

fn retype<'a>(
    values: impl Iterator<Item = &'a str> + Clone,
    bad: usize,
    name: &str,
    reported: Type,
    expected: &str,
    table: &mut StagedTable,
) -> Type {
    let inferred = infer_from_values(values);
    table.warn(format!(
        "column `{name}`: the driver reported {reported} but {bad} value(s) are not {expected}; \
         inferred as {inferred}"
    ));
    inferred
}

/// The narrowest type all values fit, for columns whose reported type does not fit.
fn infer_from_values<'a>(values: impl Iterator<Item = &'a str> + Clone) -> Type {
    let all = |ty: Type| values.clone().all(|value| canonical(ty, value).is_some());
    if all(Type::Int) {
        return Type::Int;
    }
    if let Ok((int, frac)) = fit_decimal(values.clone())
        && int + frac <= MAX_DECIMAL_PRECISION
    {
        return decimal(int + frac, frac);
    }
    [Type::Float, Type::Date, Type::Timestamp]
        .into_iter()
        .find(|&ty| all(ty))
        .unwrap_or(Type::String)
}

/// Largest integer and fraction digit counts over all values, or the number of values that are
/// not decimal numbers.
fn fit_decimal<'a>(values: impl Iterator<Item = &'a str>) -> Result<(usize, usize), usize> {
    let (mut int, mut frac, mut bad) = (0, 0, 0);
    for value in values {
        match ExactDecimal::parse(value) {
            Some(d) => {
                int = int.max(d.int.len());
                frac = frac.max(d.frac.len());
            }
            None => bad += 1,
        }
    }
    if bad == 0 { Ok((int, frac)) } else { Err(bad) }
}

struct Canonical {
    text: String,
    /// Sub-microsecond digits were dropped.
    truncated: bool,
}

impl From<String> for Canonical {
    fn from(text: String) -> Self {
        Self {
            text,
            truncated: false,
        }
    }
}

/// Canonical text of `raw` for `ty`, or `None` if `raw` is not a value of that type.
fn canonical(ty: Type, raw: &str) -> Option<Canonical> {
    match ty {
        Type::Int => canonical_int(raw).map(Canonical::from),
        Type::Decimal(..) => ExactDecimal::parse(raw).map(|d| d.to_string().into()),
        Type::Float => canonical_float(raw).map(Canonical::from),
        Type::Bool => canonical_bool(raw).map(|b| b.to_owned().into()),
        Type::Date => canonical_date(raw).map(Canonical::from),
        Type::Time => canonical_time(raw.trim()),
        Type::Timestamp => canonical_timestamp(raw),
        Type::Binary => canonical_binary(raw).map(Canonical::from),
        Type::String | Type::TimestampTz | Type::Json | Type::Null | Type::Unknown => {
            Some(raw.to_owned().into())
        }
    }
}

/// An exactly represented decimal number: no leading zeros in `int`, no trailing zeros in `frac`.
struct ExactDecimal {
    negative: bool,
    int: String,
    frac: String,
}

impl ExactDecimal {
    /// Parses plain (`-12.50`, `.5`, `+3`) and scientific (`1.5E+3`) decimal text exactly.
    fn parse(raw: &str) -> Option<Self> {
        let text = raw.trim();
        let (negative, text) = match text.as_bytes().first()? {
            b'-' => (true, &text[1..]),
            b'+' => (false, &text[1..]),
            _ => (false, text),
        };
        let (mantissa, exponent) = match text.split_once(['e', 'E']) {
            Some((mantissa, exponent)) => (mantissa, exponent.parse::<i32>().ok()?),
            None => (text, 0),
        };
        // Anything further out cannot be a DuckDB decimal anyway.
        if exponent.unsigned_abs() > 100 {
            return None;
        }
        let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
        if (int.is_empty() && frac.is_empty()) || !digits(int) || !digits(frac) {
            return None;
        }
        let mut all = format!("{int}{frac}");
        let mut point = int.len() as i64 + i64::from(exponent);
        if point < 0 {
            all.insert_str(0, &"0".repeat(point.unsigned_abs() as usize));
            point = 0;
        }
        let point = point as usize;
        if point > all.len() {
            all.push_str(&"0".repeat(point - all.len()));
        }
        let (int, frac) = all.split_at(point);
        let int = int.trim_start_matches('0');
        let frac = frac.trim_end_matches('0');
        Some(Self {
            negative: negative && !(int.is_empty() && frac.is_empty()),
            int: int.to_owned(),
            frac: frac.to_owned(),
        })
    }
}

impl std::fmt::Display for ExactDecimal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sign = if self.negative { "-" } else { "" };
        let int = if self.int.is_empty() { "0" } else { &self.int };
        if self.frac.is_empty() {
            write!(f, "{sign}{int}")
        } else {
            write!(f, "{sign}{int}.{}", self.frac)
        }
    }
}

fn canonical_int(raw: &str) -> Option<String> {
    let d = ExactDecimal::parse(raw)?;
    if !d.frac.is_empty() {
        return None;
    }
    d.to_string().parse::<i64>().ok().map(|v| v.to_string())
}

/// Shortest text that round-trips to the same double.
fn canonical_float(raw: &str) -> Option<String> {
    let value: f64 = raw.trim().parse().ok()?;
    Some(if value.is_nan() {
        "nan".to_owned()
    } else if value.is_infinite() {
        if value > 0.0 { "inf" } else { "-inf" }.to_owned()
    } else if value != 0.0 && !(1e-5..1e16).contains(&value.abs()) {
        format!("{value:e}")
    } else {
        format!("{value}")
    })
}

fn canonical_bool(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "t" => Some("true"),
        "0" | "false" | "f" => Some("false"),
        _ => None,
    }
}

/// `YYYY-MM-DD`; a midnight time part (`2024-01-31 00:00:00`) is accepted and dropped.
fn canonical_date(raw: &str) -> Option<String> {
    let text = raw.trim();
    let (date, time) = split_date_time(text);
    if let Some(time) = time {
        let midnight = canonical_time(time)?;
        if midnight.text != "00:00:00" {
            return None;
        }
    }
    parse_date(date)
}

fn split_date_time(text: &str) -> (&str, Option<&str>) {
    match text.split_once([' ', 'T']) {
        Some((date, time)) => (date, Some(time.trim_start())),
        None => (text, None),
    }
}

fn parse_date(text: &str) -> Option<String> {
    let mut parts = text.split('-');
    let (year, month, day) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || year.len() != 4 || month.len() != 2 || day.len() != 2 {
        return None;
    }
    let number = |s: &str| {
        s.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| s.parse::<u32>().ok())?
    };
    let (year, month, day) = (number(year)?, number(month)?, number(day)?);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    (1..=days)
        .contains(&day)
        .then(|| format!("{year:04}-{month:02}-{day:02}"))
}

/// `HH:MM:SS[.ffffff]` with trailing zero fractions trimmed.
fn canonical_time(text: &str) -> Option<Canonical> {
    let (clock, fraction) = text.split_once('.').unwrap_or((text, ""));
    let mut parts = clock.split(':');
    let (hour, minute, second) = (parts.next()?, parts.next()?, parts.next()?);
    let two_digits = |s: &str, max: u32| {
        (s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<u32>().ok())?
            .filter(|&v| v <= max)
    };
    if parts.next().is_some()
        || two_digits(hour, 23).is_none()
        || two_digits(minute, 59).is_none()
        || two_digits(second, 59).is_none()
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let fraction = fraction.trim_end_matches('0');
    let truncated = fraction.len() > MAX_FRACTION_DIGITS;
    let fraction = &fraction[..fraction.len().min(MAX_FRACTION_DIGITS)];
    let fraction = fraction.trim_end_matches('0');
    let text = if fraction.is_empty() {
        format!("{hour}:{minute}:{second}")
    } else {
        format!("{hour}:{minute}:{second}.{fraction}")
    };
    Some(Canonical { text, truncated })
}

/// `YYYY-MM-DD HH:MM:SS[.ffffff]`; a bare date means midnight.
fn canonical_timestamp(raw: &str) -> Option<Canonical> {
    let (date, time) = split_date_time(raw.trim());
    let date = parse_date(date)?;
    let time = match time {
        Some(time) => canonical_time(time)?,
        None => "00:00:00".to_owned().into(),
    };
    Some(Canonical {
        text: format!("{date} {}", time.text),
        truncated: time.truncated,
    })
}

/// Drivers render binary values as hex; DuckDB's text form for `BLOB` is `\xAB` escapes.
fn canonical_binary(raw: &str) -> Option<String> {
    let hex = raw.trim().as_bytes();
    if !hex.len().is_multiple_of(2) || !hex.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let mut out = String::with_capacity(hex.len() * 2);
    for pair in hex.chunks(2) {
        out.push_str("\\x");
        out.extend(pair.iter().map(|b| char::from(b.to_ascii_uppercase())));
    }
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// Redaction

const SECRET_KEYS: &[&str] = &[
    "pwd",
    "password",
    "passwd",
    "secret",
    "token",
    "accesstoken",
    "apikey",
    "api_key",
];

fn is_secret_key(key: &str) -> bool {
    let key = key.trim().to_ascii_lowercase();
    SECRET_KEYS.contains(&key.as_str())
        || ["password", "secret", "token"]
            .iter()
            .any(|word| key.contains(word))
}

/// `key=value` pairs of an ODBC connection string; `{...}` values may contain `;` and `}}`.
fn connection_attributes(connection_string: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut rest = connection_string;
    while let Some((key, after)) = rest.split_once('=') {
        let key = key.trim_start_matches(';');
        let (value, next) = if let Some(braced) = after.trim_start().strip_prefix('{') {
            let end = closing_brace(braced).unwrap_or(braced.len());
            let tail = braced.get(end + 1..).unwrap_or("");
            (
                &braced[..end],
                tail.split_once(';').map_or("", |(_, next)| next),
            )
        } else {
            after.split_once(';').unwrap_or((after, ""))
        };
        out.push((key.trim(), value));
        rest = next;
    }
    out
}

/// Index of the `}` closing a braced value (`}}` is an escaped brace).
fn closing_brace(braced: &str) -> Option<usize> {
    let bytes = braced.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'}' {
            if bytes.get(i + 1) == Some(&b'}') {
                i += 2;
                continue;
            }
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Replaces the value of any `pwd=...`-style assignment in free text with `***`.
fn redact_secret_assignments(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for key in SECRET_KEYS {
        for (start, _) in lower.match_indices(key) {
            let before = start.checked_sub(1).map(|i| bytes[i]);
            if before.is_some_and(|b| b.is_ascii_alphanumeric()) {
                continue;
            }
            let mut i = start + key.len();
            while bytes.get(i).is_some_and(|b| *b == b' ') {
                i += 1;
            }
            if bytes.get(i) != Some(&b'=') {
                continue;
            }
            i += 1;
            while bytes.get(i).is_some_and(|b| *b == b' ') {
                i += 1;
            }
            let mut value_start = i;
            let value_end = match bytes.get(i) {
                Some(b'{') => {
                    closing_brace(&text[i + 1..]).map_or(text.len(), |end| i + 1 + end + 1)
                }
                Some(&quote @ (b'\'' | b'"')) => {
                    value_start = i + 1;
                    text[value_start..]
                        .find(char::from(quote))
                        .map_or(text.len(), |end| value_start + end)
                }
                _ => text[i..]
                    .find(|c: char| c == ';' || c.is_whitespace() || "'\",)]".contains(c))
                    .map_or(text.len(), |end| i + end),
            };
            if value_end > value_start {
                spans.push((value_start, value_end));
            }
        }
    }
    spans.sort_unstable();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for (start, end) in spans {
        if start < copied {
            continue;
        }
        out.push_str(&text[copied..start]);
        out.push_str("***");
        copied = end;
    }
    out.push_str(&text[copied..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::staged::NoteLevel;

    #[test]
    fn redact_removes_connection_string_and_secrets() {
        let cs = "Driver=/opt/x.so;Server=db;UID=ana;PWD=hunter2;";
        let message = format!("login failed for [{cs}] (PWD=hunter2)");
        let redacted = redact(&message, cs);
        assert!(!redacted.contains(cs), "{redacted}");
        assert!(!redacted.contains("hunter2"), "{redacted}");
        assert!(redacted.contains("<connection string>"), "{redacted}");

        // Secret values echoed on their own, or in a differently formatted assignment.
        let cs = "DSN=prod;Password={p;a}}ss};Token=abc123";
        let redacted = redact("bad password p;a}}ss, token abc123, password = 'x9'", cs);
        assert_eq!(redacted, "bad password ***, token ***, password = '***'");
        let redacted = redact("echo: Pwd={se;cret} end", "");
        assert_eq!(redacted, "echo: Pwd=*** end");
        // Keys are matched as words only.
        assert_eq!(redact("mypwd=1", ""), "mypwd=1");
    }

    #[test]
    fn connection_attributes_handle_braces() {
        let attrs = connection_attributes("Driver={My Driver};PWD={a;b}}c};Database=/x.db;");
        assert_eq!(
            attrs,
            vec![
                ("Driver", "My Driver"),
                ("PWD", "a;b}}c"),
                ("Database", "/x.db")
            ]
        );
    }

    #[test]
    fn maps_driver_types() {
        use std::num::NonZeroUsize;
        assert_eq!(map_type(DataType::BigInt), Type::Int);
        assert_eq!(
            map_type(DataType::Decimal {
                precision: 12,
                scale: 2
            }),
            Type::Decimal(12, 2)
        );
        assert_eq!(
            map_type(DataType::Numeric {
                precision: 60,
                scale: 4
            }),
            Type::Decimal(38, 4)
        );
        assert_eq!(
            map_type(DataType::Numeric {
                precision: 0,
                scale: 3
            }),
            Type::Decimal(38, 3)
        );
        assert_eq!(
            map_type(DataType::Numeric {
                precision: 5,
                scale: -2
            }),
            Type::Decimal(7, 0)
        );
        assert_eq!(map_type(DataType::Real), Type::Float);
        assert_eq!(map_type(DataType::Bit), Type::Bool);
        assert_eq!(
            map_type(DataType::Timestamp { precision: 3 }),
            Type::Timestamp
        );
        assert_eq!(
            map_type(DataType::LongVarbinary { length: None }),
            Type::Binary
        );
        assert_eq!(
            map_type(DataType::WVarchar {
                length: NonZeroUsize::new(10)
            }),
            Type::String
        );
        assert_eq!(map_type(DataType::Unknown), Type::String);
        let other = |code| DataType::Other {
            data_type: odbc_api::sys::SqlDataType(code),
            column_size: None,
            decimal_digits: 0,
        };
        assert_eq!(map_type(other(-25)), Type::Int);
        assert_eq!(map_type(other(11)), Type::Timestamp);
        assert_eq!(map_type(other(-11)), Type::String);
    }

    fn text(ty: Type, raw: &str) -> Option<String> {
        canonical(ty, raw).map(|c| c.text)
    }

    #[test]
    fn canonical_numbers_keep_values() {
        assert_eq!(text(Type::Int, " +007 ").as_deref(), Some("7"));
        assert_eq!(text(Type::Int, "-0").as_deref(), Some("0"));
        assert_eq!(text(Type::Int, "5.000").as_deref(), Some("5"));
        assert_eq!(text(Type::Int, "5.5"), None);
        assert_eq!(text(Type::Int, "9223372036854775808"), None);
        let dec = Type::Decimal(10, 2);
        assert_eq!(text(dec, "0012.30").as_deref(), Some("12.3"));
        assert_eq!(text(dec, "-.50").as_deref(), Some("-0.5"));
        assert_eq!(text(dec, "-0.00").as_deref(), Some("0"));
        assert_eq!(text(dec, "1.25E+3").as_deref(), Some("1250"));
        assert_eq!(text(dec, "125e-4").as_deref(), Some("0.0125"));
        assert_eq!(text(dec, "1,5"), None);
        assert_eq!(text(Type::Float, "12.300").as_deref(), Some("12.3"));
        assert_eq!(text(Type::Float, "1E+20").as_deref(), Some("1e20"));
        assert_eq!(text(Type::Float, "0.000001").as_deref(), Some("1e-6"));
        assert_eq!(text(Type::Float, "-Inf").as_deref(), Some("-inf"));
        assert_eq!(text(Type::Float, "abc"), None);
    }

    #[test]
    fn canonical_temporal_and_other() {
        assert_eq!(
            text(Type::Date, "2024-02-29").as_deref(),
            Some("2024-02-29")
        );
        assert_eq!(text(Type::Date, "2023-02-29"), None);
        assert_eq!(
            text(Type::Date, "2024-01-31 00:00:00.000").as_deref(),
            Some("2024-01-31")
        );
        assert_eq!(text(Type::Date, "2024-01-31 10:00:00"), None);
        assert_eq!(
            text(Type::Timestamp, "2024-01-31 10:20:30.120000").as_deref(),
            Some("2024-01-31 10:20:30.12")
        );
        assert_eq!(
            text(Type::Timestamp, "2024-01-31T10:20:30.000").as_deref(),
            Some("2024-01-31 10:20:30")
        );
        assert_eq!(
            text(Type::Timestamp, "2024-01-31").as_deref(),
            Some("2024-01-31 00:00:00")
        );
        let nanos = canonical(Type::Timestamp, "2024-01-31 10:20:30.123456789").unwrap();
        assert_eq!(
            (nanos.text.as_str(), nanos.truncated),
            ("2024-01-31 10:20:30.123456", true)
        );
        assert_eq!(
            text(Type::Time, "07:08:09.500").as_deref(),
            Some("07:08:09.5")
        );
        assert_eq!(text(Type::Time, "24:00:00"), None);
        assert_eq!(text(Type::Bool, "1").as_deref(), Some("true"));
        assert_eq!(text(Type::Bool, "FALSE").as_deref(), Some("false"));
        assert_eq!(text(Type::Bool, "2"), None);
        assert_eq!(text(Type::Binary, "0aFF").as_deref(), Some("\\x0A\\xFF"));
        assert_eq!(text(Type::Binary, "0aF"), None);
        assert_eq!(text(Type::String, " kept ").as_deref(), Some(" kept "));
    }

    fn column(rows: &[Option<&str>], reported: Type) -> (Type, Vec<Option<String>>, StagedTable) {
        let mut rows: Vec<Vec<Option<String>>> =
            rows.iter().map(|v| vec![v.map(str::to_owned)]).collect();
        let mut table = StagedTable::default();
        let ty = settle_column(&mut rows, 0, "c", reported, &mut table);
        (
            ty,
            rows.into_iter().map(|mut r| r.remove(0)).collect(),
            table,
        )
    }

    #[test]
    fn weakly_typed_columns_are_retyped_with_a_warning() {
        let (ty, values, table) = column(&[Some("1"), None, Some("2.5")], Type::Int);
        assert_eq!(ty, Type::Decimal(2, 1));
        assert_eq!(values, vec![Some("1".into()), None, Some("2.5".into())]);
        assert_eq!(table.notes.len(), 1);
        assert_eq!(table.notes[0].level, NoteLevel::Warning);

        let (ty, _, table) = column(&[Some("1.5"), Some("n/a")], Type::Float);
        assert_eq!(ty, Type::String);
        assert!(
            !table.notes[0].message.contains("n/a"),
            "{}",
            table.notes[0].message
        );

        // Decimal columns widen to hold every value exactly.
        let (ty, values, table) = column(&[Some("123.456"), Some("7")], Type::Decimal(5, 2));
        assert_eq!(ty, Type::Decimal(6, 3));
        assert_eq!(values, vec![Some("123.456".into()), Some("7".into())]);
        assert_eq!(table.notes[0].level, NoteLevel::Note);

        // Values fitting the reported type leave it alone and produce no notes.
        let (ty, _, table) = column(&[Some("2024-01-02"), None], Type::Date);
        assert_eq!((ty, table.notes.len()), (Type::Date, 0));
    }

    /// Driver-backed tests are `#[ignore]`d: run them with `cargo test -- --ignored` and
    /// `MAGI_TEST_SQLITE_ODBC_DRIVER` naming the SQLite ODBC driver built by
    /// `scripts/build-sqlite-odbc.sh`.
    fn sqlite_driver() -> String {
        std::env::var("MAGI_TEST_SQLITE_ODBC_DRIVER")
            .ok()
            .filter(|d| !d.is_empty())
            .expect("MAGI_TEST_SQLITE_ODBC_DRIVER must name the SQLite ODBC driver (scripts/build-sqlite-odbc.sh)")
    }

    fn sqlite_database(driver: &str, statements: &[&str]) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let cs = format!(
            "Driver={driver};Database={};",
            dir.path().join("t.db").display()
        );
        let connection = connect(&target(&cs)).unwrap();
        for statement in statements {
            connection.execute(statement, (), None).unwrap();
        }
        (dir, cs)
    }

    fn target(connection_string: &str) -> Target {
        Target::ConnectionString(connection_string.to_owned())
    }

    // sqliteodbc reports NUMERIC columns as DOUBLE and BIGINT with a C type code (-25).
    const SETUP: &[&str] = &[
        "CREATE TABLE t (id INTEGER, amount REAL, name TEXT, price NUMERIC(10,2), \
         day DATE, at TIMESTAMP, flag BIT, big BIGINT)",
        "INSERT INTO t VALUES (1, 12.5, 'alpha', 10.25, '2024-01-31', \
         '2024-01-31 10:20:30.500', 1, 9007199254740993)",
        "INSERT INTO t VALUES (2, 0.30000000000000004, '', 3, '2023-12-01', \
         '2023-12-01 00:00:00', 0, -1)",
        "INSERT INTO t VALUES (3, NULL, NULL, NULL, NULL, NULL, NULL, NULL)",
    ];

    #[test]
    #[ignore = "needs MAGI_TEST_SQLITE_ODBC_DRIVER"]
    fn extracts_sqlite_table_as_canonical_text() {
        let driver = sqlite_driver();
        let (_dir, cs) = sqlite_database(&driver, SETUP);
        let table = extract(&target(&cs), "SELECT * FROM t ORDER BY id").unwrap();
        let columns: Vec<(&str, Type)> = table
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.inferred))
            .collect();
        assert_eq!(
            columns,
            vec![
                ("id", Type::Int),
                ("amount", Type::Float),
                ("name", Type::String),
                ("price", Type::Float),
                ("day", Type::Date),
                ("at", Type::Timestamp),
                ("flag", Type::Bool),
                ("big", Type::Int),
            ]
        );
        let row = |values: &[Option<&str>]| -> Vec<Option<String>> {
            values.iter().map(|v| v.map(str::to_owned)).collect()
        };
        assert_eq!(
            table.rows,
            vec![
                row(&[
                    Some("1"),
                    Some("12.5"),
                    Some("alpha"),
                    Some("10.25"),
                    Some("2024-01-31"),
                    Some("2024-01-31 10:20:30.5"),
                    Some("true"),
                    Some("9007199254740993"),
                ]),
                row(&[
                    Some("2"),
                    Some("0.30000000000000004"),
                    Some(""),
                    Some("3"),
                    Some("2023-12-01"),
                    Some("2023-12-01 00:00:00"),
                    Some("false"),
                    Some("-1"),
                ]),
                row(&[Some("3"), None, None, None, None, None, None, None]),
            ]
        );
        assert_eq!(table.source_rows, vec![1, 2, 3]);
        assert!(table.notes.is_empty(), "{:?}", table.notes);
    }

    #[test]
    #[ignore = "needs MAGI_TEST_SQLITE_ODBC_DRIVER"]
    fn extracts_values_longer_than_the_batch_buffer() {
        let driver = sqlite_driver();
        let long = "x".repeat(3 * MAX_BUFFERED_VALUE);
        let insert = format!("INSERT INTO t VALUES (2, '{long}')");
        let (_dir, cs) = sqlite_database(
            &driver,
            &[
                "CREATE TABLE t (id INTEGER, body VARCHAR(10))",
                "INSERT INTO t VALUES (1, 'short')",
                &insert,
            ],
        );
        let table = extract(&target(&cs), "SELECT id, body FROM t ORDER BY id").unwrap();
        assert_eq!(table.rows[0][1].as_deref(), Some("short"));
        assert_eq!(table.rows[1][1].as_deref(), Some(long.as_str()));
    }

    #[test]
    #[ignore = "needs MAGI_TEST_SQLITE_ODBC_DRIVER"]
    fn weak_sqlite_types_are_retyped() {
        let driver = sqlite_driver();
        let (_dir, cs) = sqlite_database(
            &driver,
            &[
                "CREATE TABLE t (n INTEGER, secret TEXT)",
                "INSERT INTO t VALUES (1, 'a'), ('two-and-a-bit', 'b')",
            ],
        );
        let table = extract(&target(&cs), "SELECT n FROM t ORDER BY rowid").unwrap();
        assert_eq!(table.columns[0].inferred, Type::String);
        assert_eq!(table.notes.len(), 1);
        assert!(!table.notes[0].message.contains("two-and-a-bit"));
    }

    #[test]
    #[ignore = "needs MAGI_TEST_SQLITE_ODBC_DRIVER"]
    fn describe_does_not_execute() {
        let driver = sqlite_driver();
        let (_dir, cs) = sqlite_database(&driver, SETUP);
        // Overflows only when a row is actually computed.
        let query = "SELECT id, name, abs(-9223372036854775807 - id) AS boom FROM t";
        let columns = describe(&target(&cs), query).unwrap();
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "name", "boom"]);
        assert_eq!(columns[0].inferred, Type::Int);
        assert_eq!(columns[1].inferred, Type::String);
        assert!(extract(&target(&cs), query).is_err());
    }

    #[test]
    #[ignore = "needs MAGI_TEST_SQLITE_ODBC_DRIVER"]
    fn query_errors_are_redacted() {
        let driver = sqlite_driver();
        let (_dir, cs) = sqlite_database(&driver, SETUP);
        let cs = format!("{cs}PWD=hunter2;");
        for error in [
            extract(&target(&cs), "SELEC nonsense FROM t").unwrap_err(),
            describe(&target(&cs), "SELECT missing FROM t").unwrap_err(),
            extract(&target(&cs), "DELETE FROM t").unwrap_err(),
        ] {
            assert!(!error.contains(&cs), "{error}");
            assert!(!error.contains("hunter2"), "{error}");
        }
    }

    #[test]
    fn connection_errors_are_redacted() {
        let cs = "Driver=/nonexistent/libnothing.so;Database=/tmp/x.db;PWD=hunter2;";
        let error = extract(&target(cs), "SELECT 1").unwrap_err();
        assert!(error.starts_with("ODBC error while connecting"), "{error}");
        assert!(!error.contains(cs) && !error.contains("hunter2"), "{error}");
    }
}
