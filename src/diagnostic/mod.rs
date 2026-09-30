//! Diagnostics: compile-time errors/warnings with source spans, and run-time findings.
//!
//! Rendering goes through `miette` so every diagnostic shows the offending source text. Codes are
//! stable identifiers (`M0xx` syntax/resolution, `M1xx` types, `M2xx` sources, `M3xx`
//! reconciliation, `M4xx` validation/run time, `M5xx` exports, `M6xx` BI models, `M7xx` tests);
//! see `docs/diagnostics.md`.

use std::fmt;

use miette::{GraphicalReportHandler, GraphicalTheme, LabeledSpan, NamedSource, SourceSpan};

use crate::syntax::span::{SourceMap, Span};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Note,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Label {
    pub span: Span,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic {
    pub code: &'static str,
    pub severity: Severity,
    pub message: String,
    pub labels: Vec<Label>,
    pub help: Option<String>,
}

impl Diagnostic {
    fn new(code: &'static str, severity: Severity, message: impl Into<String>) -> Self {
        Self {
            code,
            severity,
            message: message.into(),
            labels: Vec::new(),
            help: None,
        }
    }
    pub fn error(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(code, Severity::Error, message)
    }
    pub fn warning(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(code, Severity::Warning, message)
    }
    pub fn note(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(code, Severity::Note, message)
    }
    pub fn label(mut self, span: Span, text: impl Into<String>) -> Self {
        self.labels.push(Label {
            span,
            text: text.into(),
        });
        self
    }
    pub fn help(mut self, text: impl Into<String>) -> Self {
        self.help = Some(text.into());
        self
    }
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}[{}]: {}", self.severity, self.code, self.message)
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        })
    }
}

/// Accumulates diagnostics so one `magi check` reports every problem it can find.
#[derive(Debug, Default)]
pub struct Diagnostics {
    pub list: Vec<Diagnostic>,
}

impl Diagnostics {
    pub fn push(&mut self, d: Diagnostic) {
        if !self.list.contains(&d) {
            self.list.push(d);
        }
    }
    pub fn extend(&mut self, ds: impl IntoIterator<Item = Diagnostic>) {
        for d in ds {
            self.push(d);
        }
    }
    pub fn has_errors(&self) -> bool {
        self.list.iter().any(Diagnostic::is_error)
    }
    pub fn count(&self, severity: Severity) -> usize {
        self.list.iter().filter(|d| d.severity == severity).count()
    }
}

// ---------------------------------------------------------------------------------------------
// rendering

struct Report<'a> {
    diag: &'a Diagnostic,
    source: Option<NamedSource<String>>,
    labels: Vec<LabeledSpan>,
    help: Option<String>,
}

impl fmt::Debug for Report<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.diag.fmt(f)
    }
}
impl fmt::Display for Report<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.diag.message)
    }
}
impl std::error::Error for Report<'_> {}

impl miette::Diagnostic for Report<'_> {
    fn code<'b>(&'b self) -> Option<Box<dyn fmt::Display + 'b>> {
        Some(Box::new(format!(
            "{}[{}]",
            self.diag.severity, self.diag.code
        )))
    }
    fn severity(&self) -> Option<miette::Severity> {
        Some(match self.diag.severity {
            Severity::Error => miette::Severity::Error,
            Severity::Warning => miette::Severity::Warning,
            Severity::Note => miette::Severity::Advice,
        })
    }
    fn help<'b>(&'b self) -> Option<Box<dyn fmt::Display + 'b>> {
        self.help
            .as_ref()
            .map(|h| Box::new(h.clone()) as Box<dyn fmt::Display>)
    }
    fn source_code(&self) -> Option<&dyn miette::SourceCode> {
        self.source.as_ref().map(|s| s as &dyn miette::SourceCode)
    }
    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        if self.labels.is_empty() {
            None
        } else {
            Some(Box::new(self.labels.iter().cloned()))
        }
    }
}

/// Render one diagnostic with source context. `color` enables ANSI styling.
pub fn render(diag: &Diagnostic, sources: &SourceMap, color: bool) -> String {
    let primary = diag.labels.first().map(|l| l.span.file);
    let mut labels = Vec::new();
    let mut extra = Vec::new();
    for (i, l) in diag.labels.iter().enumerate() {
        if Some(l.span.file) == primary && (l.span.file as usize) < sources.files.len() {
            let text = if l.text.is_empty() {
                None
            } else {
                Some(l.text.clone())
            };
            let span = SourceSpan::new((l.span.start as usize).into(), l.span.len());
            labels.push(if i == 0 {
                LabeledSpan::new_primary_with_span(text, span)
            } else {
                LabeledSpan::new_with_span(text, span)
            });
        } else if (l.span.file as usize) < sources.files.len() {
            let file = sources.file(l.span.file);
            let (line, col) = file.line_col(l.span.start);
            extra.push(format!("{}:{line}:{col}: {}", file.name, l.text));
        }
    }
    let source = primary
        .filter(|&f| (f as usize) < sources.files.len())
        .map(|f| {
            let file = sources.file(f);
            NamedSource::new(file.name.clone(), file.text.to_string())
        });
    let help = match (&diag.help, extra.is_empty()) {
        (h, true) => h.clone(),
        (None, false) => Some(extra.join("\n")),
        (Some(h), false) => Some(format!("{h}\n{}", extra.join("\n"))),
    };
    let report = Report {
        diag,
        source,
        labels,
        help,
    };
    let theme = if color {
        GraphicalTheme::unicode()
    } else {
        GraphicalTheme::unicode_nocolor()
    };
    let handler = GraphicalReportHandler::new_themed(theme).with_width(100);
    let mut out = String::new();
    if handler.render_report(&mut out, &report).is_err() {
        out = format!("{diag}\n");
    }
    out
}

/// Suggest the closest candidate for a misspelt name (`did you mean ...?`). Equal scores go to
/// the lexicographically smallest name, so the suggestion does not depend on candidate order
/// (callers often pass hash-map keys).
pub fn did_you_mean<'a>(
    name: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    candidates
        .into_iter()
        .map(|c| (strsim::jaro_winkler(&lower, &c.to_ascii_lowercase()), c))
        .filter(|(score, _)| *score >= 0.80)
        .max_by(|a, b| a.0.total_cmp(&b.0).then_with(|| b.1.cmp(a.1)))
        .map(|(_, c)| c.to_string())
}

#[cfg(test)]
mod tests {
    use super::did_you_mean;

    #[test]
    fn did_you_mean_breaks_ties_by_name_not_order() {
        let forward = did_you_mean("orders_2023", ["orders_2021", "orders_2022"]);
        let backward = did_you_mean("orders_2023", ["orders_2022", "orders_2021"]);
        assert_eq!(forward.as_deref(), Some("orders_2021"));
        assert_eq!(backward.as_deref(), Some("orders_2021"));
        // A strictly better score still wins over a smaller name.
        assert_eq!(
            did_you_mean("fliter", ["alter", "filter"]).as_deref(),
            Some("filter")
        );
    }
}
