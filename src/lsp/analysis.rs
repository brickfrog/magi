//! Analysing a program for the editor: which file is the program's entry, the analysis itself
//! (no SQL source is contacted), and MAGI diagnostics as LSP diagnostics.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lsp_types::{DiagnosticRelatedInformation, DiagnosticSeverity, Location, NumberOrString, Url};

use super::convert::{LineIndex, key};
use crate::diagnostic::{Diagnostic, Severity};
use crate::semantic::hir::Hir;
use crate::semantic::load::{self, Loaded};
use crate::semantic::resolve::{self, Options};
use crate::semantic::schema::SchemaProvider;
use crate::source::Reader;

/// Reads a file the program imports: the editor's buffer when it is open, else the disk.
pub type Read<'a> = &'a dyn Fn(&Path) -> std::io::Result<String>;

/// One analysed program (an entry file and its imports).
pub struct Analysis {
    pub loaded: Loaded,
    pub hir: Hir,
    pub diags: Vec<Diagnostic>,
    /// Identity ([`key`]) of each file, indexed by `FileId`.
    pub keys: Vec<PathBuf>,
}

impl Analysis {
    /// The `FileId` of a file in this program.
    pub fn file_id(&self, file: &Path) -> Option<u32> {
        self.keys.iter().position(|k| k == file).map(|i| i as u32)
    }
}

/// Analyse the program whose entry file is `root`; imports are read through `read`. SQL sources
/// are not contacted (as in `magi check`); local files are read to learn their columns.
pub fn analyse(root: &Path, read: Read<'_>) -> Result<Analysis, String> {
    let text = read(root).map_err(|e| format!("cannot read {}: {e}", root.display()))?;
    let loaded = load::load_text_with(root, text, read);
    let mut reader = Reader::new(false)?;
    let options = Options {
        today: crate::cli::utc_today(),
    };
    let (hir, diags) = resolve::analyze(&loaded, &mut reader as &mut dyn SchemaProvider, &options);
    let keys = loaded.sources.files.iter().map(|f| key(&f.path)).collect();
    Ok(Analysis {
        loaded,
        hir,
        diags: diags.list,
        keys,
    })
}

/// The entry file of the program `file` belongs to. A program is often split into files that
/// use each other's names (`reconcile.magi` imports `prepare.magi`, which reads sources declared
/// in `reconcile.magi`), so analysing an imported file on its own reports names as unknown. The
/// entry is the `.magi` file in the same directory whose imports reach `file` and take in the
/// most files (the first by name on a tie); `file` itself when no file imports it.
pub fn entry(file: &Path, read: Read<'_>) -> PathBuf {
    let Some(dir) = file.parent() else {
        return file.to_path_buf();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return file.to_path_buf();
    };
    let mut siblings: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "magi") && key(p) != file)
        .collect();
    siblings.sort();
    let mut best: Option<(usize, PathBuf)> = None;
    for sibling in siblings {
        let Ok(text) = read(&sibling) else {
            continue;
        };
        let loaded = load::load_text_with(&sibling, text, read);
        let files = &loaded.sources.files;
        if files.iter().any(|f| key(&f.path) == file)
            && best.as_ref().is_none_or(|(n, _)| files.len() > *n)
        {
            best = Some((files.len(), key(&sibling)));
        }
    }
    best.map_or_else(|| file.to_path_buf(), |(_, p)| p)
}

/// The analysis's diagnostics as LSP diagnostics, by the file their primary label is in (`uri`
/// names a file of the program). A diagnostic without a label goes to the entry file's first
/// line.
pub fn diagnostics(
    a: &Analysis,
    uri: &dyn Fn(u32) -> Option<Url>,
) -> BTreeMap<Url, Vec<lsp_types::Diagnostic>> {
    let indexes: Vec<LineIndex<'_>> = a
        .loaded
        .sources
        .files
        .iter()
        .map(|f| LineIndex::new(&f.text))
        .collect();
    let location = |span: crate::syntax::span::Span| {
        let file = span.file as usize;
        let index = indexes.get(file)?;
        Some((
            uri(span.file)?,
            index.range(span.start as usize, span.end as usize),
        ))
    };
    let mut out: BTreeMap<Url, Vec<lsp_types::Diagnostic>> = BTreeMap::new();
    for d in &a.diags {
        let primary = match d.labels.first() {
            Some(l) => location(l.span),
            None => uri(0).map(|u| (u, lsp_types::Range::default())),
        };
        let Some((uri, range)) = primary else {
            continue;
        };
        let related: Vec<DiagnosticRelatedInformation> = d
            .labels
            .iter()
            .skip(1)
            .filter_map(|l| {
                let (uri, range) = location(l.span)?;
                Some(DiagnosticRelatedInformation {
                    location: Location::new(uri, range),
                    message: l.text.clone(),
                })
            })
            .collect();
        let message = match &d.help {
            Some(help) => format!("{}\nhelp: {help}", d.message),
            None => d.message.clone(),
        };
        let diag = lsp_types::Diagnostic {
            range,
            severity: Some(match d.severity {
                Severity::Error => DiagnosticSeverity::ERROR,
                Severity::Warning => DiagnosticSeverity::WARNING,
                Severity::Note => DiagnosticSeverity::INFORMATION,
            }),
            code: Some(NumberOrString::String(d.code.to_string())),
            source: Some("magi".into()),
            message,
            related_information: (!related.is_empty()).then_some(related),
            ..Default::default()
        };
        let list = out.entry(uri).or_default();
        if !list.contains(&diag) {
            list.push(diag);
        }
    }
    out
}
