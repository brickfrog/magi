//! Loading a program: the entry file plus every file it imports (filesystem-based modules).
//! Each file is parsed once; import cycles are reported.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::ast::{self, Statement};
use crate::diagnostic::Diagnostic;
use crate::syntax::parser;
use crate::syntax::span::SourceMap;

pub struct Loaded {
    pub sources: SourceMap,
    /// One parsed program per file; imported files come before the files that import them.
    pub files: Vec<ast::Program>,
    pub diagnostics: Vec<Diagnostic>,
    /// Names declared by statements that failed to parse (already reported); later stages must
    /// not report them again as unknown.
    pub failed_names: Vec<String>,
}

impl Loaded {
    pub fn statements(&self) -> impl Iterator<Item = &Statement> {
        self.files.iter().flat_map(|f| f.statements.iter())
    }

    /// The program without its `export`, `model` and `test` statements, each source declaration
    /// replaced by `source(declaration)` (same source map, so spans stay valid): it writes no
    /// file, and without exports every relation is computed (`plan::optimize::needed`).
    pub fn without_outputs(
        &self,
        mut source: impl FnMut(&ast::SourceDecl) -> ast::SourceDecl,
    ) -> Loaded {
        let files = self
            .files
            .iter()
            .map(|f| ast::Program {
                statements: f
                    .statements
                    .iter()
                    .filter_map(|s| match s {
                        Statement::Export(_) | Statement::Model(_) | Statement::Test(_) => None,
                        Statement::Source(d) => Some(Statement::Source(source(d))),
                        s => Some(s.clone()),
                    })
                    .collect(),
            })
            .collect();
        Loaded {
            sources: self.sources.clone(),
            files,
            diagnostics: self.diagnostics.clone(),
            failed_names: self.failed_names.clone(),
        }
    }
}

pub fn load(path: &Path) -> Result<Loaded, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Ok(load_text(path, text))
}

/// Load from in-memory text (imports are still read from disk relative to `path`).
pub fn load_text(path: &Path, text: String) -> Loaded {
    load_text_with(path, text, &|p| std::fs::read_to_string(p))
}

/// Like [`load_text`], reading imported files through `read` (the language server passes the
/// editor's unsaved buffers, falling back to disk).
pub fn load_text_with(
    path: &Path,
    text: String,
    read: &dyn Fn(&Path) -> std::io::Result<String>,
) -> Loaded {
    let mut loader = Loader {
        loaded: Loaded {
            sources: SourceMap::default(),
            files: Vec::new(),
            diagnostics: Vec::new(),
            failed_names: Vec::new(),
        },
        seen: HashSet::new(),
        stack: Vec::new(),
        read,
    };
    let name = path.display().to_string();
    loader.visit(path, name, text);
    loader.loaded
}

struct Loader<'r> {
    loaded: Loaded,
    seen: HashSet<PathBuf>,
    stack: Vec<PathBuf>,
    read: &'r dyn Fn(&Path) -> std::io::Result<String>,
}

impl Loader<'_> {
    fn visit(&mut self, path: &Path, name: String, text: String) {
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let file = self.loaded.sources.add(path, name, text);
        self.seen.insert(key.clone());
        self.stack.push(key);
        let src = self.loaded.sources.file(file).text.clone();
        let parsed = parser::parse(file, &src);
        self.loaded.diagnostics.extend(parsed.diagnostics);
        self.loaded.failed_names.extend(parsed.failed_names);
        let base = self.loaded.sources.base_dir(file);
        for stmt in &parsed.program.statements {
            let Statement::Import(imp) = stmt else {
                continue;
            };
            let target = base.join(&imp.path.value);
            let canon = std::fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
            if self.stack.contains(&canon) {
                self.loaded
                    .diagnostics
                    .push(Diagnostic::error("M004", "import cycle").label(
                        imp.path.span,
                        format!("`{}` is already being imported", imp.path.value),
                    ));
                continue;
            }
            if self.seen.contains(&canon) {
                continue;
            }
            match (self.read)(&target) {
                Ok(text) => {
                    let display = target.display().to_string();
                    self.visit(&target, display, text);
                }
                Err(e) => self.loaded.diagnostics.push(
                    Diagnostic::error("M004", format!("cannot read imported file: {e}"))
                        .label(imp.path.span, format!("{}", target.display())),
                ),
            }
        }
        self.stack.pop();
        self.loaded.files.push(parsed.program);
    }
}
