//! Source schemas: how the analyser learns what columns a source has, and how a declared
//! schema (source contract) is merged with what the source itself reports.

use crate::semantic::hir::{Connection, Source};
use crate::semantic::types::Type;
use crate::source::SourceError;
use crate::source::staged::SourceNote;

/// Columns as reported by the source itself.
#[derive(Debug, Clone, Default)]
pub struct InferredSchema {
    pub columns: Vec<(String, Type)>,
    pub notes: Vec<SourceNote>,
}

pub trait SchemaProvider {
    /// `Ok(None)` means the schema cannot be known without contacting an external system and
    /// the provider was asked not to (`magi check` without `--sources`).
    fn infer(
        &mut self,
        source: &Source,
        connections: &[Connection],
    ) -> Result<Option<InferredSchema>, SourceError>;

    /// Whether `format` is a date/time format staging can parse with (`%Q` is not); the error
    /// names the bad specifier.
    fn check_format(&mut self, format: &str) -> Result<(), String>;
}
