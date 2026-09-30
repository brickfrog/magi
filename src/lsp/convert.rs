//! Positions and paths. MAGI spans are byte offsets into a file's text; LSP positions are a line
//! and a column counted in UTF-16 code units (the protocol's default encoding), so a column past
//! `é` or `€` differs from the byte offset.

use std::path::{Path, PathBuf};

use lsp_types::{Position, Range, Url};

/// Line starts of one text, for converting between byte offsets and LSP positions. Lines end at
/// `\n`; a `\r` before it stays part of the line, which no position inside the line can reach.
pub struct LineIndex<'t> {
    text: &'t str,
    starts: Vec<usize>,
}

impl<'t> LineIndex<'t> {
    pub fn new(text: &'t str) -> Self {
        let starts = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(i, _)| i + 1))
            .collect();
        LineIndex { text, starts }
    }

    /// The position of byte `offset` (clamped to the text, and back to a character boundary).
    pub fn position(&self, offset: usize) -> Position {
        let mut offset = offset.min(self.text.len());
        while !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        let line = self.starts.partition_point(|&s| s <= offset) - 1;
        let character = self.text[self.starts[line]..offset]
            .chars()
            .map(char::len_utf16)
            .sum::<usize>();
        Position::new(line as u32, character as u32)
    }

    pub fn range(&self, start: usize, end: usize) -> Range {
        Range::new(self.position(start), self.position(end))
    }

    /// The byte offset of `pos`. A line past the end maps to the end of the text, a column past
    /// the end of its line to the line's end, and a column inside a surrogate pair to the
    /// character's start.
    pub fn offset(&self, pos: Position) -> usize {
        let Some(&start) = self.starts.get(pos.line as usize) else {
            return self.text.len();
        };
        let line = &self.text[start..];
        let line = &line[..line.find('\n').unwrap_or(line.len())];
        let mut units = 0usize;
        for (i, c) in line.char_indices() {
            units += c.len_utf16();
            if units > pos.character as usize {
                return start + i;
            }
        }
        start + line.len()
    }
}

/// The file an LSP document URI names (`file://` only).
pub fn path_of(uri: &Url) -> Option<PathBuf> {
    uri.to_file_path().ok()
}

pub fn uri_of(path: &Path) -> Option<Url> {
    Url::from_file_path(path).ok()
}

/// A path's identity: canonical when it exists, else its canonical directory joined with its
/// name (a buffer the editor has not saved yet), else the path as given.
pub fn key(path: &Path) -> PathBuf {
    if let Ok(p) = std::fs::canonicalize(path) {
        return p;
    }
    match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) => std::fs::canonicalize(dir)
            .map(|d| d.join(name))
            .unwrap_or_else(|_| path.to_path_buf()),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_count_utf16_units() {
        let text = "ab\nxé€𝄞y\n";
        let ix = LineIndex::new(text);
        let y = text.find('y').unwrap();
        // x(1) é(1) €(1) 𝄞(2)
        assert_eq!(ix.position(y), Position::new(1, 5));
        assert_eq!(ix.offset(Position::new(1, 5)), y);
        assert_eq!(ix.position(text.len()), Position::new(2, 0));
        // inside the surrogate pair: the character's start
        assert_eq!(ix.offset(Position::new(1, 4)), text.find('𝄞').unwrap());
        // past the line's end: the line's end
        assert_eq!(ix.offset(Position::new(0, 99)), 2);
        assert_eq!(ix.offset(Position::new(9, 0)), text.len());
    }
}
