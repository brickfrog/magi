//! Source locations. Every AST node keeps the span of the text it was parsed from so that
//! semantic diagnostics can point at the exact code the analyst wrote.

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Index into [`SourceMap::files`].
pub type FileId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Span {
    pub file: FileId,
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(file: FileId, start: usize, end: usize) -> Self {
        Self {
            file,
            start: start as u32,
            end: end as u32,
        }
    }
    /// Smallest span covering both (must be in the same file).
    pub fn to(self, other: Span) -> Span {
        Span {
            file: self.file,
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
    pub fn len(self) -> usize {
        (self.end - self.start) as usize
    }
    pub fn range(self) -> std::ops::Range<usize> {
        self.start as usize..self.end as usize
    }
}

#[derive(Debug)]
pub struct SourceFile {
    pub path: PathBuf,
    /// Display name used in diagnostics.
    pub name: String,
    pub text: Arc<str>,
}

impl SourceFile {
    /// 1-based line and column of a byte offset.
    pub fn line_col(&self, offset: u32) -> (usize, usize) {
        let offset = (offset as usize).min(self.text.len());
        let before = &self.text[..offset];
        let line = before.bytes().filter(|&b| b == b'\n').count() + 1;
        let col = before.rfind('\n').map_or(offset, |nl| offset - nl - 1) + 1;
        (line, col)
    }
}

/// All `.magi` files that make up one program (the entry file plus imports).
#[derive(Debug, Default)]
pub struct SourceMap {
    pub files: Vec<SourceFile>,
}

impl SourceMap {
    pub fn add(&mut self, path: &Path, name: String, text: String) -> FileId {
        self.files.push(SourceFile {
            path: path.to_path_buf(),
            name,
            text: text.into(),
        });
        (self.files.len() - 1) as FileId
    }
    pub fn file(&self, id: FileId) -> &SourceFile {
        &self.files[id as usize]
    }
    pub fn text(&self, span: Span) -> &str {
        &self.file(span.file).text[span.range()]
    }
    /// Directory relative paths in the given file resolve against.
    pub fn base_dir(&self, id: FileId) -> PathBuf {
        self.file(id)
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default()
    }
}
