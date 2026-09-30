//! Rows extracted by a MAGI-side reader (Excel, ODBC) before they are staged into DuckDB.
//!
//! Readers hand over every cell as canonical text so staging is a plain VARCHAR append; the
//! declared or inferred [`Type`] is applied afterwards by DuckDB casts. Canonical text forms:
//! numbers use the shortest round-trip form (`12.3`, `5`, `-0.25`), dates `YYYY-MM-DD`,
//! timestamps `YYYY-MM-DD HH:MM:SS[.ffffff]`, times `HH:MM:SS`, booleans `true`/`false`.
//! Empty cells are `None`.

use crate::semantic::types::Type;

#[derive(Debug, Clone, PartialEq)]
pub struct StagedColumn {
    pub name: String,
    /// Type inferred from the source (cell types, driver metadata).
    pub inferred: Type,
    /// When every value is a number: the narrowest type holding each exactly (a decimal with the
    /// digits its values need). What a column declared numeric is checked against (M203).
    pub exact: Option<Type>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteLevel {
    Warning,
    Note,
}

/// Something a reader noticed that the analyst should know about. Messages must not contain
/// cell values (source data may be sensitive); cell references and counts are fine.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceNote {
    pub level: NoteLevel,
    pub message: String,
    /// The column whose inferred type the note is about; the note is left out when the program
    /// declares that column's type (the declaration then decides how its values are read).
    pub type_hint_for: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct StagedTable {
    pub columns: Vec<StagedColumn>,
    /// Row-major; every row has exactly `columns.len()` cells.
    pub rows: Vec<Vec<Option<String>>>,
    /// 1-based position of each row in its source (e.g. Excel worksheet row number); same length
    /// as `rows`. Used for row identity and diagnostics.
    pub source_rows: Vec<u64>,
    pub notes: Vec<SourceNote>,
}

impl StagedTable {
    pub fn warn(&mut self, message: impl Into<String>) {
        self.push(NoteLevel::Warning, message.into(), None);
    }
    pub fn note(&mut self, message: impl Into<String>) {
        self.push(NoteLevel::Note, message.into(), None);
    }
    /// A note suggesting to declare `column`'s type (see [`SourceNote::type_hint_for`]).
    pub fn type_hint(&mut self, level: NoteLevel, column: &str, message: impl Into<String>) {
        self.push(level, message.into(), Some(column.to_string()));
    }

    fn push(&mut self, level: NoteLevel, message: String, type_hint_for: Option<String>) {
        self.notes.push(SourceNote {
            level,
            message,
            type_hint_for,
        });
    }
}
