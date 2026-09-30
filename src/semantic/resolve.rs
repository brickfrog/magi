//! Name resolution and type checking: AST -> HIR.
//!
//! Declarations may appear in any order; relations are resolved on demand with cycle detection.
//! Every problem found is reported (one `magi check` shows everything it can), and a relation
//! that failed to resolve is "poisoned" so it does not cascade into follow-up errors.

mod expr;
mod model;
mod pipeline;
mod reconcile;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::ast::{self, CheckKind as AstCheckKind, ExprKind, Literal, Statement};
use crate::diagnostic::{Diagnostic, Diagnostics, did_you_mean};
use crate::semantic::hir::*;
use crate::semantic::load::Loaded;
use crate::semantic::schema::SchemaProvider;
use crate::semantic::types::{ColType, Type};
use crate::source::excel::ExcelOptions;
use crate::source::fixed::FixedWidthOptions;
use crate::source::staged::NoteLevel;
use crate::syntax::span::{SourceMap, Span};

use expr::Scope;

pub struct Options {
    /// Value of `today()` (ISO date).
    pub today: String,
}

/// Check a value for [`Options::today`] (`--today`, a test's `today:`): a calendar date
/// written `YYYY-MM-DD`.
pub fn parse_date(s: &str) -> Result<String, String> {
    let field = |r: std::ops::Range<usize>| -> Option<u32> {
        let part = s.get(r)?;
        if part.bytes().all(|c| c.is_ascii_digit()) {
            part.parse().ok()
        } else {
            None
        }
    };
    let shape = s.len() == 10 && s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-';
    let (true, Some(y), Some(m), Some(d)) = (shape, field(0..4), field(5..7), field(8..10)) else {
        return Err("expected a date written YYYY-MM-DD, e.g. 2024-01-31".into());
    };
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let days = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return Err(format!("month {m} does not exist (expected YYYY-MM-DD)")),
    };
    if d == 0 || d > days {
        return Err(format!("{y:04}-{m:02} has no day {d}"));
    }
    Ok(s.to_string())
}

enum Decl<'a> {
    Source(&'a ast::SourceDecl),
    Dataset(&'a ast::DatasetDecl),
    Reconcile(&'a ast::ReconcileDecl),
}

impl Decl<'_> {
    fn span(&self) -> Span {
        match self {
            Decl::Source(d) => d.name.span,
            Decl::Dataset(d) => d.name.span,
            Decl::Reconcile(d) => d.name.span,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    InProgress,
    Done,
    Failed,
}

pub struct Analyzer<'a> {
    sm: &'a SourceMap,
    diags: Diagnostics,
    hir: Hir,
    decls: HashMap<String, Decl<'a>>,
    /// Lower-case name -> declared spelling of every source, dataset and reconcile (and of
    /// declarations that failed to parse): references are case-insensitive and resolve to the
    /// declared spelling, which is the only spelling the HIR uses.
    canon: HashMap<String, String>,
    validates: HashMap<String, Vec<&'a ast::ValidateDecl>>,
    mappings: HashMap<String, &'a ast::MappingDecl>,
    used_mappings: HashSet<String>,
    /// Number of `duckdb.*` calls resolved so far (marks DuckDB-specific datasets), and the
    /// functions already reported as DuckDB-specific.
    natives_used: usize,
    natives_noted: HashSet<String>,
    connections: HashMap<String, usize>,
    /// Datasets whose columns stay unknown even at run time (`native_sql` without `schema`).
    native_open: HashSet<String>,
    state: HashMap<String, State>,
    provider: &'a mut dyn SchemaProvider,
}

/// Analyse a loaded program. The returned HIR is only meaningful when the diagnostics contain
/// no errors.
pub fn analyze(
    loaded: &Loaded,
    provider: &mut dyn SchemaProvider,
    options: &Options,
) -> (Hir, Diagnostics) {
    let mut a = Analyzer {
        sm: &loaded.sources,
        diags: Diagnostics::default(),
        hir: Hir {
            today: options.today.clone(),
            ..Hir::default()
        },
        decls: HashMap::new(),
        canon: HashMap::new(),
        validates: HashMap::new(),
        mappings: HashMap::new(),
        used_mappings: HashSet::new(),
        natives_used: 0,
        natives_noted: HashSet::new(),
        connections: HashMap::new(),
        native_open: HashSet::new(),
        state: HashMap::new(),
        provider,
    };
    a.diags.extend(loaded.diagnostics.iter().cloned());
    a.collect(loaded);
    // A declaration that failed to parse is already reported; references to it stay silent.
    for name in &loaded.failed_names {
        a.canon
            .entry(name.to_ascii_lowercase())
            .or_insert_with(|| name.clone());
        if !a.decls.contains_key(name) {
            a.state.insert(name.clone(), State::Failed);
        }
    }
    let names: Vec<String> = loaded
        .statements()
        .filter_map(|s| match s {
            Statement::Source(d) => Some(d.name.name.clone()),
            Statement::Dataset(d) => Some(d.name.name.clone()),
            Statement::Reconcile(d) => Some(d.name.name.clone()),
            _ => None,
        })
        .collect();
    for name in names {
        a.ensure(&name, None);
    }
    for s in loaded.statements() {
        if let Statement::Validate(v) = s {
            // validations of relations that are never referenced still run
            a.ensure(&v.target.display(), Some(v.target.span));
        }
    }
    for s in loaded.statements() {
        if let Statement::Export(e) = s {
            a.export(e);
        }
    }
    for s in loaded.statements() {
        if let Statement::Model(m) = s {
            a.model(m);
        }
    }
    a.warn_unused_mappings();
    (a.hir, a.diags)
}

/// `name` starts with MAGI's reserved prefix `__magi`, in any case.
fn is_reserved(name: &str) -> bool {
    name.get(..6)
        .is_some_and(|p| p.eq_ignore_ascii_case("__magi"))
}

impl<'a> Analyzer<'a> {
    fn err(&mut self, d: Diagnostic) {
        self.diags.push(d);
    }

    fn text(&self, span: Span) -> &'a str {
        self.sm.text(span)
    }

    /// Names starting with `__magi` (in any case: DuckDB names are case-insensitive) belong to
    /// MAGI's internal tables and columns.
    fn not_reserved(&mut self, name: &ast::Ident) -> bool {
        self.not_reserved_at(
            &name.name,
            name.span,
            "reserved for MAGI's internal tables and columns",
        )
    }

    /// [`Self::not_reserved`] for a name without its own identifier, e.g. a column a source
    /// file provides (`span` is where the name comes from, `label` says so).
    fn not_reserved_at(&mut self, name: &str, span: Span, label: &str) -> bool {
        if is_reserved(name) {
            self.err(
                Diagnostic::error(
                    "M013",
                    format!("`{name}` uses the reserved prefix `__magi`"),
                )
                .label(span, label)
                .help("choose another name"),
            );
            return false;
        }
        true
    }

    fn base_dir(&self, span: Span) -> PathBuf {
        self.sm.base_dir(span.file)
    }

    // ---- declarations -------------------------------------------------------------------------

    fn collect(&mut self, loaded: &'a Loaded) {
        // DuckDB names are case-insensitive, so `Sales` and `sales` would be one table.
        let mut seen: HashMap<String, (String, Span)> = HashMap::new();
        let mut claim = |this: &mut Self, name: &ast::Ident, what: &str| -> bool {
            if !this.not_reserved(name) {
                return false;
            }
            if name.name.contains('.') {
                this.err(
                    Diagnostic::error("M014", format!("`{}` contains `.`", name.name))
                        .label(name.span, format!("{what} name"))
                        .help("`x.part` names an output of relation `x` (`x.rejects`, `r.matches`); choose a name without `.`"),
                );
                return false;
            }
            if let Some((prev_name, prev)) = seen.get(&name.name.to_ascii_lowercase()) {
                let mut d =
                    Diagnostic::error("M003", format!("`{}` is defined more than once", name.name))
                        .label(name.span, format!("{what} redefines it"))
                        .label(*prev, "first defined here");
                if *prev_name != name.name {
                    d = d.help(format!(
                        "names are case-insensitive: `{prev_name}` and `{}` are the same name",
                        name.name
                    ));
                    // references to the second spelling stay silent instead of being unknown
                    this.state.insert(name.name.clone(), State::Failed);
                }
                this.err(d);
                return false;
            }
            seen.insert(
                name.name.to_ascii_lowercase(),
                (name.name.clone(), name.span),
            );
            true
        };
        for stmt in loaded.statements() {
            match stmt {
                Statement::Source(d) => {
                    if claim(self, &d.name, "source") {
                        self.canon
                            .insert(d.name.name.to_ascii_lowercase(), d.name.name.clone());
                        self.decls.insert(d.name.name.clone(), Decl::Source(d));
                    }
                }
                Statement::Dataset(d) => {
                    if claim(self, &d.name, "dataset") {
                        self.canon
                            .insert(d.name.name.to_ascii_lowercase(), d.name.name.clone());
                        self.decls.insert(d.name.name.clone(), Decl::Dataset(d));
                    }
                }
                Statement::Reconcile(d) => {
                    if claim(self, &d.name, "reconcile") {
                        self.canon
                            .insert(d.name.name.to_ascii_lowercase(), d.name.name.clone());
                        self.decls.insert(d.name.name.clone(), Decl::Reconcile(d));
                    }
                }
                Statement::Mapping(d) => {
                    if claim(self, &d.name, "mapping") {
                        self.mappings.insert(d.name.name.clone(), d);
                    }
                }
                Statement::Connection(d) => {
                    if claim(self, &d.name, "connection") {
                        self.connection(d);
                    }
                }
                Statement::Runtime(r) => self.runtime(r),
                Statement::Validate(_)
                | Statement::Import(_)
                | Statement::Export(_)
                | Statement::Model(_)
                | Statement::Test(_) => {}
            }
        }
        // after every declaration is known, so a target written in another case finds its block
        for stmt in loaded.statements() {
            if let Statement::Validate(v) = stmt {
                let target = self.canonical(&v.target.display());
                let list = self.validates.entry(target.clone()).or_default();
                if let Some(prev) = list.first() {
                    let prev_span = prev.target.span;
                    self.diags.push(
                        Diagnostic::error(
                            "M003",
                            format!("`{target}` has more than one validate block"),
                        )
                        .label(v.target.span, "second validate block")
                        .label(prev_span, "first one here")
                        .help("merge the checks into one block"),
                    );
                } else {
                    list.push(v);
                }
            }
        }
    }

    /// The declared spelling of a relation reference (`Sales`, `REC.Matches` -> `sales`,
    /// `rec.matches`): the base name as declared, the output part (always lower case in MAGI)
    /// in lower case. Unknown names are returned unchanged.
    fn canonical(&self, name: &str) -> String {
        let (base, part) = match name.split_once('.') {
            Some((b, p)) => (b, Some(p)),
            None => (name, None),
        };
        let base = self
            .canon
            .get(&base.to_ascii_lowercase())
            .map_or(base, String::as_str);
        match part {
            Some(p) => format!("{base}.{}", p.to_ascii_lowercase()),
            None => base.to_string(),
        }
    }

    /// M005 for mappings nothing uses. Skipped after an error: a statement that failed to resolve
    /// (e.g. over a source that cannot be read) may be the one using the mapping.
    fn warn_unused_mappings(&mut self) {
        if self.diags.has_errors() {
            return;
        }
        let mut unused: Vec<(&String, Span)> = self
            .mappings
            .iter()
            .filter(|(n, _)| !self.used_mappings.contains(*n))
            .map(|(n, d)| (n, d.name.span))
            .collect();
        unused.sort_by_key(|(_, s)| (s.file, s.start));
        let diags: Vec<Diagnostic> = unused
            .into_iter()
            .map(|(name, span)| {
                Diagnostic::warning("M005", format!("mapping `{name}` is never used"))
                    .label(span, "defined here")
            })
            .collect();
        self.diags.extend(diags);
    }

    /// Look up a mapping by name, recording the use.
    fn mapping(&mut self, name: &ast::Ident) -> Option<&'a ast::MappingDecl> {
        let found = self
            .mappings
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(&name.name))
            .map(|(k, d)| (k.clone(), *d));
        match found {
            Some((declared, d)) => {
                self.used_mappings.insert(declared);
                Some(d)
            }
            None if self.state.get(&name.name) == Some(&State::Failed) => None,
            None => {
                let names: Vec<&str> = self.mappings.keys().map(String::as_str).collect();
                let mut d = Diagnostic::error("M109", format!("unknown mapping `{}`", name.name))
                    .label(name.span, "not defined");
                if let Some(s) = did_you_mean(&name.name, names) {
                    d = d.help(format!("did you mean `{s}`?"));
                }
                self.err(d);
                None
            }
        }
    }

    /// Resolve a relation by name (`x` or `x.part`), producing it if needed. Returns the index in
    /// `hir.relations`, or `None` if it is unknown or failed (the error is already reported).
    fn ensure(&mut self, name: &str, use_span: Option<Span>) -> Option<usize> {
        let name = &self.canonical(name);
        if let Some(&i) = self.hir.relation_index.get(name) {
            return Some(i);
        }
        let (base, part) = match name.split_once('.') {
            Some((b, p)) => (b.to_string(), Some(p.to_string())),
            None => (name.to_string(), None),
        };
        match self.state.get(&base).copied() {
            Some(State::InProgress) => {
                if let Some(span) = use_span {
                    let decl_span = self.decls.get(&base).map(Decl::span);
                    let mut d = Diagnostic::error("M006", format!("`{base}` depends on itself"))
                        .label(span, "this reference closes a cycle");
                    if let Some(ds) = decl_span {
                        d = d.label(ds, "defined here");
                    }
                    self.err(d);
                }
                self.state.insert(base.clone(), State::Failed);
                return None;
            }
            Some(State::Failed) => return None,
            Some(State::Done) => {}
            None => {
                let Some(decl) = self.decls.get(&base) else {
                    if let Some(span) = use_span {
                        self.unknown_relation(&base, span);
                    }
                    return None;
                };
                let decl: Decl<'a> = match decl {
                    Decl::Source(d) => Decl::Source(d),
                    Decl::Dataset(d) => Decl::Dataset(d),
                    Decl::Reconcile(d) => Decl::Reconcile(d),
                };
                self.state.insert(base.clone(), State::InProgress);
                let ok = match decl {
                    Decl::Source(d) => self.source(d),
                    Decl::Dataset(d) => self.dataset(d),
                    Decl::Reconcile(d) => self.reconcile(d),
                };
                if self.state.get(&base) == Some(&State::Failed) || !ok {
                    self.state.insert(base.clone(), State::Failed);
                    return None;
                }
                self.state.insert(base.clone(), State::Done);
                self.validate_outputs(&base);
            }
        }
        if let Some(&i) = self.hir.relation_index.get(name) {
            return Some(i);
        }
        if let (Some(span), Some(part)) = (use_span, part) {
            let parts: Vec<String> = self
                .hir
                .relations
                .iter()
                .filter_map(|r| r.name.strip_prefix(&format!("{base}.")).map(str::to_string))
                .collect();
            let mut d = Diagnostic::error("M002", format!("`{base}` has no output `{part}`"))
                .label(span, "unknown output");
            if let Some(s) = did_you_mean(&part, parts.iter().map(String::as_str)) {
                d = d.help(format!("did you mean `{base}.{s}`?"));
            } else if !parts.is_empty() {
                d = d.help(format!(
                    "available: {}",
                    parts
                        .iter()
                        .map(|p| format!("`{base}.{p}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            } else if !self.validates.contains_key(&base)
                && (part == "failures" || part == "checks")
            {
                d = d.help(format!(
                    "`{base}.{part}` exists only when `{base}` has a validate block"
                ));
            }
            self.err(d);
        }
        None
    }

    /// Run the validate blocks of a declaration that was just resolved: of `base` itself, then
    /// of its outputs (`rec.matches`, `src.rejects`), then of their `.failures` / `.checks`
    /// (fewer parts first: validating `rec.matches` makes `rec.matches.failures`). A block
    /// whose target does not exist is skipped here and reported where [`analyze`] resolves it.
    fn validate_outputs(&mut self, base: &str) {
        let prefix = format!("{base}.");
        let mut targets: Vec<String> = self
            .validates
            .keys()
            .filter(|k| *k == base || k.starts_with(&prefix))
            .cloned()
            .collect();
        targets.sort_by_key(|k| (k.matches('.').count(), k.clone()));
        for target in targets {
            if !self.hir.relation_index.contains_key(&target) {
                continue;
            }
            for v in self.validates[&target].clone() {
                self.validation(v);
            }
        }
    }

    fn unknown_relation(&mut self, name: &str, span: Span) {
        let mut d = Diagnostic::error("M002", format!("unknown relation `{name}`"))
            .label(span, "not defined");
        let names: Vec<&str> = self.decls.keys().map(String::as_str).collect();
        if let Some(s) = did_you_mean(name, names) {
            d = d.help(format!("did you mean `{s}`?"));
        } else if self.mappings.keys().any(|m| m.eq_ignore_ascii_case(name)) {
            d = d.help(format!("`{name}` is a mapping; use it with `normalize col with {name}` or `map(col, {name})`"));
        }
        self.err(d);
    }

    fn rel_ref(&mut self, r: &ast::RelRef) -> Option<usize> {
        self.ensure(&r.display(), Some(r.span))
    }

    fn relation(&self, i: usize) -> &Relation {
        &self.hir.relations[i]
    }

    // ---- options ------------------------------------------------------------------------------

    fn opt_str(&mut self, o: &ast::Opt) -> Option<String> {
        match &o.value.kind {
            ExprKind::Literal(Literal::Str(s)) => Some(s.clone()),
            _ => {
                self.err(
                    Diagnostic::error("M007", format!("`{}` must be a string", o.key.name))
                        .label(o.value.span, "expected a string"),
                );
                None
            }
        }
    }
    fn opt_bool(&mut self, o: &ast::Opt) -> Option<bool> {
        match &o.value.kind {
            ExprKind::Literal(Literal::Bool(b)) => Some(*b),
            _ => {
                self.err(
                    Diagnostic::error(
                        "M007",
                        format!("`{}` must be `true` or `false`", o.key.name),
                    )
                    .label(o.value.span, "expected a bool"),
                );
                None
            }
        }
    }
    fn opt_int(&mut self, o: &ast::Opt) -> Option<i64> {
        match &o.value.kind {
            ExprKind::Literal(Literal::Int(v)) => Some(*v),
            _ => {
                self.err(
                    Diagnostic::error("M007", format!("`{}` must be a whole number", o.key.name))
                        .label(o.value.span, "expected a number"),
                );
                None
            }
        }
    }
    fn opt_secret(&mut self, o: &ast::Opt) -> Option<Secret> {
        match &o.value.kind {
            ExprKind::Literal(Literal::Str(s)) => Some(Secret::Literal(s.clone())),
            ExprKind::Call {
                namespace: None,
                name,
                args,
            } if name.name == "env" => match args.as_slice() {
                [
                    ast::Expr {
                        kind: ExprKind::Literal(Literal::Str(var)),
                        ..
                    },
                ] => Some(Secret::Env(var.clone())),
                _ => {
                    self.err(
                        Diagnostic::error("M007", "`env` takes one string: the variable name")
                            .label(o.value.span, "here"),
                    );
                    None
                }
            },
            _ => {
                self.err(
                    Diagnostic::error(
                        "M007",
                        format!("`{}` must be a string or `env(\"VAR\")`", o.key.name),
                    )
                    .label(o.value.span, "here"),
                );
                None
            }
        }
    }
    fn unknown_option(&mut self, o: &ast::Opt, allowed: &[&str], what: &str) {
        let mut d = Diagnostic::error(
            "M007",
            format!("unknown option `{}` for {what}", o.key.name),
        )
        .label(o.key.span, "not recognised");
        if let Some(s) = did_you_mean(&o.key.name, allowed.iter().copied()) {
            d = d.help(format!("did you mean `{s}`?"));
        } else {
            d = d.help(format!(
                "allowed: {}",
                allowed
                    .iter()
                    .map(|a| format!("`{a}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        self.err(d);
    }
    /// `section: N`, counted from 1.
    fn opt_section(&mut self, o: &ast::Opt) -> Option<u32> {
        let v = self.opt_int(o)?;
        match u32::try_from(v) {
            Ok(n) if n >= 1 => Some(n),
            _ => {
                self.err(
                    Diagnostic::error("M007", "`section` counts from 1")
                        .label(o.value.span, "expected 1 for the file's first section")
                        .help("a section is a run of non-blank lines; blank lines separate them"),
                );
                None
            }
        }
    }
    /// A column name: `row_number: row_no`.
    fn opt_column(&mut self, o: &ast::Opt) -> Option<(String, Span)> {
        match &o.value.kind {
            ExprKind::Column(c) if c.qualifier.is_none() => Some((c.name.name.clone(), c.span)),
            _ => {
                self.err(
                    Diagnostic::error("M007", format!("`{}` takes a column name", o.key.name))
                        .label(
                            o.value.span,
                            "expected a name, e.g. `row_no` or `` `Row No` ``",
                        ),
                );
                None
            }
        }
    }
    /// A column name or a list of them: `fill_down: Account`, `fill_down: [a, b]`.
    fn opt_columns(&mut self, o: &ast::Opt) -> Vec<(String, Span)> {
        let items = match &o.value.kind {
            ExprKind::List(items) => items.as_slice(),
            _ => std::slice::from_ref(&o.value),
        };
        let mut out = Vec::new();
        for e in items {
            match &e.kind {
                ExprKind::Column(c) if c.qualifier.is_none() => {
                    out.push((c.name.name.clone(), c.span))
                }
                _ => self.err(
                    Diagnostic::error(
                        "M007",
                        format!(
                            "`{}` takes a column name or a list of column names",
                            o.key.name
                        ),
                    )
                    .label(e.span, "expected a column name"),
                ),
            }
        }
        out
    }
    /// `fill_down` or `row_number` on a source whose rows have no file order (M214); whether `o`
    /// is one of them.
    fn row_option_unsupported(&mut self, o: &ast::Opt, what: &str) -> bool {
        if !matches!(o.key.name.as_str(), "fill_down" | "row_number") {
            return false;
        }
        self.err(
            Diagnostic::error(
                "M214",
                format!("`{}` is not available for {what}", o.key.name),
            )
            .label(o.key.span, "needs the rows in file order")
            .help("only csv, excel and fixed_width sources are read in the order of their lines or rows"),
        );
        true
    }

    fn connection(&mut self, d: &'a ast::ConnectionDecl) {
        let kind = match d.kind.name.as_str() {
            "odbc" => {
                let (mut dsn, mut cs, mut user, mut password) = (None, None, None, None);
                for o in &d.options {
                    match o.key.name.as_str() {
                        "dsn" => dsn = self.opt_str(o),
                        "connection_string" => cs = self.opt_secret(o),
                        "user" => user = self.opt_secret(o),
                        "password" => password = self.opt_secret(o),
                        // `integrated` (the only mode) adds nothing to the connection string:
                        // the DSN / driver configuration decides how Windows or Kerberos
                        // credentials are used
                        "auth" => {
                            let mode = match &o.value.kind {
                                ExprKind::Column(c) if c.qualifier.is_none() => {
                                    Some(c.name.name.clone())
                                }
                                ExprKind::Literal(Literal::Str(s)) => Some(s.clone()),
                                _ => None,
                            };
                            if mode.as_deref() != Some("integrated") {
                                let mut diag = Diagnostic::error(
                                    "M007",
                                    "unsupported `auth` for an odbc connection",
                                )
                                .label(o.value.span, "expected `integrated`");
                                diag = match mode
                                    .as_deref()
                                    .and_then(|m| did_you_mean(m, ["integrated"]))
                                {
                                    Some(s) => diag.help(format!("did you mean `{s}`?")),
                                    None => diag.help(
                                        "`auth: integrated` uses the credentials configured for the DSN or driver; otherwise give `user:` and `password:`",
                                    ),
                                };
                                self.err(diag);
                            }
                        }
                        _ => self.unknown_option(
                            o,
                            &["dsn", "connection_string", "user", "password", "auth"],
                            "an odbc connection",
                        ),
                    }
                    let secret_literal = match (&o.value.kind, o.key.name.as_str()) {
                        (ExprKind::Literal(Literal::Str(_)), "password") => true,
                        (ExprKind::Literal(Literal::Str(s)), "connection_string") => {
                            let l = s.to_ascii_lowercase().replace(' ', "");
                            l.contains("pwd=") || l.contains("password=")
                        }
                        _ => false,
                    };
                    if secret_literal {
                        self.err(
                            Diagnostic::warning(
                                "M008",
                                format!("`{}` is written in the program text", o.key.name),
                            )
                            .label(o.value.span, "credentials in source code")
                            .help("read secrets from the environment: `env(\"MAGI_WAREHOUSE\")`"),
                        );
                    }
                }
                if dsn.is_none() && cs.is_none() {
                    self.err(
                        Diagnostic::error(
                            "M007",
                            "odbc connection needs `dsn` or `connection_string`",
                        )
                        .label(d.name.span, "here"),
                    );
                }
                ConnectionKind::Odbc {
                    dsn,
                    connection_string: cs,
                    user,
                    password,
                }
            }
            "duckdb" => {
                let mut path = None;
                for o in &d.options {
                    match o.key.name.as_str() {
                        "path" => path = self.opt_str(o),
                        _ => self.unknown_option(o, &["path"], "a duckdb connection"),
                    }
                }
                let Some(path) = path else {
                    self.err(
                        Diagnostic::error("M007", "duckdb connection needs `path`")
                            .label(d.name.span, "here"),
                    );
                    return;
                };
                ConnectionKind::DuckDb {
                    path: self.base_dir(d.span).join(path),
                }
            }
            other => {
                let mut diag =
                    Diagnostic::error("M007", format!("unknown connection kind `{other}`"))
                        .label(d.kind.span, "expected `odbc` or `duckdb`");
                if let Some(s) = did_you_mean(other, ["odbc", "duckdb"]) {
                    diag = diag.help(format!("did you mean `{s}`?"));
                }
                self.err(diag);
                return;
            }
        };
        self.connections
            .insert(d.name.name.clone(), self.hir.connections.len());
        self.hir.connections.push(Connection {
            name: d.name.name.clone(),
            kind,
        });
    }

    fn runtime(&mut self, r: &ast::RuntimeDecl) {
        for o in &r.options {
            match o.key.name.as_str() {
                "threads" => self.hir.runtime.threads = self.opt_int(o).map(|v| v.max(1) as u32),
                "memory_limit" => self.hir.runtime.memory_limit = self.opt_str(o),
                "temp_storage" => {
                    let v = match &o.value.kind {
                        ExprKind::Column(c) if c.qualifier.is_none() && c.name.name == "memory" => {
                            Some("memory".to_string())
                        }
                        _ => self.opt_str(o),
                    };
                    if let Some(v) = v {
                        self.hir.runtime.temp_directory = if v == "memory" {
                            None
                        } else {
                            Some(self.base_dir(r.span).join(v))
                        };
                    }
                }
                "persist_intermediate" => {
                    if self.opt_bool(o) == Some(true) {
                        self.err(
                            Diagnostic::error(
                                "M007",
                                "`persist_intermediate: true` is not supported",
                            )
                            .label(
                                o.value.span,
                                "intermediate relations are always discarded after the run",
                            )
                            .help("export the relations you need to keep"),
                        );
                    }
                }
                _ => self.unknown_option(
                    o,
                    &[
                        "threads",
                        "memory_limit",
                        "temp_storage",
                        "persist_intermediate",
                    ],
                    "runtime",
                ),
            }
        }
    }

    // ---- sources ------------------------------------------------------------------------------

    fn source(&mut self, d: &'a ast::SourceDecl) -> bool {
        // `layout:` and other options resolve against the declaring file; the path against the
        // file of its literal, which is a test's file for `given x = "path"`
        let base = self.base_dir(d.span);
        let path_arg = |this: &mut Self| -> Option<PathBuf> {
            match d.args.as_slice() {
                [
                    ast::Expr {
                        kind: ExprKind::Literal(Literal::Str(p)),
                        span,
                    },
                ] => Some(this.base_dir(*span).join(p)),
                _ => {
                    this.err(
                        Diagnostic::error(
                            "M007",
                            format!("`{}(...)` takes one file path string", d.kind.name),
                        )
                        .label(d.kind.span, "expected e.g. `csv(\"data/a.csv\")`"),
                    );
                    None
                }
            }
        };
        // CSV and Excel rows keep their file order: `fill_down` and `row_number` name columns
        // (checked once the source's columns are known)
        let mut fill_down: Vec<(String, Span)> = Vec::new();
        let mut row_number: Option<(String, Span)> = None;
        let kind = match d.kind.name.as_str() {
            "csv" => {
                let Some(path) = path_arg(self) else {
                    return false;
                };
                let mut options = CsvOptions::default();
                for o in &d.options {
                    match o.key.name.as_str() {
                        "delimiter" => options.delimiter = self.opt_str(o),
                        "header" => options.header = self.opt_bool(o).unwrap_or(true),
                        "all_text" => options.all_text = self.opt_bool(o).unwrap_or(false),
                        "section" => options.section = self.opt_section(o),
                        "ragged" => options.ragged = self.opt_bool(o).unwrap_or(false),
                        "fill_down" => fill_down = self.opt_columns(o),
                        "row_number" => row_number = self.opt_column(o),
                        _ => self.unknown_option(
                            o,
                            &[
                                "delimiter",
                                "header",
                                "all_text",
                                "section",
                                "ragged",
                                "fill_down",
                                "row_number",
                            ],
                            "a csv source",
                        ),
                    }
                }
                options.fill_down = fill_down.iter().map(|(n, _)| n.clone()).collect();
                options.row_number = row_number.as_ref().map(|(n, _)| n.clone());
                SourceKind::Csv { path, options }
            }
            "parquet" => {
                let Some(path) = path_arg(self) else {
                    return false;
                };
                for o in &d.options {
                    if !self.row_option_unsupported(o, "a parquet source") {
                        self.unknown_option(o, &[], "a parquet source");
                    }
                }
                SourceKind::Parquet { path }
            }
            "excel" | "xlsx" => {
                let Some(path) = path_arg(self) else {
                    return false;
                };
                let mut options = ExcelOptions::default();
                let mut section_at = None;
                for o in &d.options {
                    match o.key.name.as_str() {
                        "sheet" => options.sheet = self.opt_str(o),
                        "range" => options.range = self.opt_str(o),
                        "header_row" => {
                            options.header_row = self.opt_int(o).map(|v| v.max(0) as u32)
                        }
                        "section" => {
                            options.section = self.opt_section(o);
                            section_at = Some(o.key.span);
                        }
                        "header" => options.header = self.opt_bool(o).unwrap_or(true),
                        "all_text" => options.all_text = self.opt_bool(o).unwrap_or(false),
                        "fill_down" => fill_down = self.opt_columns(o),
                        "row_number" => row_number = self.opt_column(o),
                        _ => self.unknown_option(
                            o,
                            &[
                                "sheet",
                                "range",
                                "header_row",
                                "section",
                                "header",
                                "all_text",
                                "fill_down",
                                "row_number",
                            ],
                            "an excel source",
                        ),
                    }
                }
                // a section says where the table is; so do `range` and `header_row`
                if let Some(span) = section_at {
                    let other = d
                        .options
                        .iter()
                        .find(|o| matches!(o.key.name.as_str(), "range" | "header_row"));
                    if let Some(o) = other {
                        self.err(
                            Diagnostic::error(
                                "M007",
                                format!("`section` conflicts with `{}`", o.key.name),
                            )
                            .label(span, "reads a run of non-blank rows")
                            .label(o.key.span, "also says where the table is")
                            .help("keep one of them: `section: N` follows the table as the sheet grows"),
                        );
                        return false;
                    }
                }
                options.fill_down = fill_down.iter().map(|(n, _)| n.clone()).collect();
                options.row_number = row_number.as_ref().map(|(n, _)| n.clone());
                SourceKind::Excel { path, options }
            }
            "fixed_width" => {
                let Some(path) = path_arg(self) else {
                    return false;
                };
                let (mut layout, mut encoding, mut record) = (None, None, None);
                for o in &d.options {
                    match o.key.name.as_str() {
                        "layout" => layout = self.opt_str(o).map(|p| base.join(p)),
                        "encoding" => {
                            encoding = self.opt_str(o);
                            if let Some(e) = &encoding
                                && let Err(message) = crate::source::fixed::encoding(Some(e))
                            {
                                self.err(
                                    Diagnostic::error("M007", message)
                                        .label(o.value.span, "the file's text encoding")
                                        .help("e.g. `encoding: \"cp1252\"`, `\"latin1\"` or `\"utf-8\"`"),
                                );
                                return false;
                            }
                        }
                        "record" => record = self.opt_str(o),
                        "fill_down" => fill_down = self.opt_columns(o),
                        "row_number" => row_number = self.opt_column(o),
                        _ => self.unknown_option(
                            o,
                            &["layout", "encoding", "record", "fill_down", "row_number"],
                            "a fixed_width source",
                        ),
                    }
                }
                let Some(layout) = layout else {
                    self.err(
                        Diagnostic::error("M007", "a fixed_width source needs `layout`")
                            .label(d.name.span, "here")
                            .help(
                                "`layout: \"layout.csv\"` names a CSV of `field,start,length,type`",
                            ),
                    );
                    return false;
                };
                SourceKind::FixedWidth {
                    path,
                    options: FixedWidthOptions {
                        layout,
                        encoding,
                        record,
                        fill_down: fill_down.iter().map(|(n, _)| n.clone()).collect(),
                        row_number: row_number.as_ref().map(|(n, _)| n.clone()),
                    },
                }
            }
            "duckdb" => {
                let Some(path) = path_arg(self) else {
                    return false;
                };
                let (mut table, mut query) = (None, None);
                for o in &d.options {
                    match o.key.name.as_str() {
                        "table" => table = self.opt_str(o),
                        "query" => query = self.opt_str(o).map(statement_body),
                        _ if self.row_option_unsupported(o, "a duckdb source") => {}
                        _ => self.unknown_option(o, &["table", "query"], "a duckdb source"),
                    }
                }
                if table.is_some() == query.is_some() {
                    self.err(
                        Diagnostic::error(
                            "M007",
                            "a duckdb source needs exactly one of `table` or `query`",
                        )
                        .label(d.name.span, "here"),
                    );
                    return false;
                }
                SourceKind::DuckDb { path, table, query }
            }
            "sql" => {
                let conn = match d.args.as_slice() {
                    [
                        ast::Expr {
                            kind: ExprKind::Column(c),
                            ..
                        },
                    ] if c.qualifier.is_none() => Some((c.name.name.clone(), c.span)),
                    [
                        ast::Expr {
                            kind: ExprKind::Literal(Literal::Str(s)),
                            span,
                        },
                    ] => Some((s.clone(), *span)),
                    _ => None,
                };
                let Some((mut conn, conn_span)) = conn else {
                    self.err(
                        Diagnostic::error("M007", "`sql(...)` takes a connection name")
                            .label(d.kind.span, "expected e.g. `sql(warehouse)`"),
                    );
                    return false;
                };
                if let Some(declared) = self
                    .connections
                    .keys()
                    .find(|k| k.eq_ignore_ascii_case(&conn))
                {
                    conn = declared.clone();
                }
                if !self.connections.contains_key(&conn) {
                    // a connection whose declaration did not parse was already reported
                    if self.state.get(&conn) == Some(&State::Failed) {
                        return false;
                    }
                    let mut diag =
                        Diagnostic::error("M009", format!("unknown connection `{conn}`"))
                            .label(conn_span, "not defined");
                    let names: Vec<&str> = self.connections.keys().map(String::as_str).collect();
                    diag = match did_you_mean(&conn, names) {
                        Some(s) => diag.help(format!("did you mean `{s}`?")),
                        None => diag.help(format!(
                            "declare it: `connection {conn} = odbc {{ dsn: \"...\" }}`"
                        )),
                    };
                    self.err(diag);
                    return false;
                }
                let mut query = None;
                for o in &d.options {
                    match o.key.name.as_str() {
                        "query" => query = self.opt_str(o).map(statement_body),
                        _ if self.row_option_unsupported(o, "a sql source") => {}
                        _ => self.unknown_option(o, &["query"], "a sql source"),
                    }
                }
                let Some(query) = query else {
                    self.err(
                        Diagnostic::error("M007", "a sql source needs `query`")
                            .label(d.name.span, "here"),
                    );
                    return false;
                };
                SourceKind::Sql {
                    connection: conn,
                    query,
                }
            }
            other => {
                let mut diag = Diagnostic::error("M007", format!("unknown source kind `{other}`"))
                    .label(
                        d.kind.span,
                        "expected csv, parquet, excel, fixed_width, duckdb or sql",
                    );
                if let Some(s) = did_you_mean(
                    other,
                    ["csv", "parquet", "excel", "fixed_width", "duckdb", "sql"],
                ) {
                    diag = diag.help(format!("did you mean `{s}`?"));
                }
                self.err(diag);
                return false;
            }
        };
        let declared = d.schema.as_ref().map(|s| self.declared_schema(s));
        let identity = d
            .identity
            .as_ref()
            .map(|ids| ids.iter().map(|i| i.name.clone()).collect::<Vec<_>>());
        let head = match d.args.last() {
            // a test's `given x = "path"`: the path is in the test's file, so point at it there
            Some(a) if a.span.file != d.name.span.file => a.span,
            last => d.name.span.to(last.map_or(d.kind.span, |a| a.span)),
        };
        let mut src = Source {
            name: d.name.name.clone(),
            kind,
            declared,
            identity,
            span: head,
        };
        // CSV, Excel and ODBC values arrive as text and are parsed by MAGI: a declared format
        // applies whatever type the text looks like
        let text_staged = match &src.kind {
            SourceKind::Csv { .. } | SourceKind::Excel { .. } | SourceKind::FixedWidth { .. } => {
                true
            }
            SourceKind::Sql { connection, .. } => {
                let conn = self.hir.connections.iter().find(|c| &c.name == connection);
                conn.is_some_and(|c| matches!(c.kind, ConnectionKind::Odbc { .. }))
            }
            SourceKind::Parquet { .. } | SourceKind::DuckDb { .. } => false,
        };

        // columns: what the source reports, merged with the declaration
        let inferred = match self.provider.infer(&src, &self.hir.connections) {
            Ok(i) => i,
            Err(e) => {
                let message = if e.code == "M200" {
                    format!("cannot read source `{}`: {}", src.name, e.message)
                } else {
                    format!("source `{}`: {}", src.name, e.message)
                };
                let mut diag =
                    Diagnostic::error(e.code, message).label(src.span, src.kind.describe());
                if let Some(help) = e.help {
                    diag = diag.help(help);
                }
                self.err(diag);
                return false;
            }
        };
        let mut columns: Vec<(String, ColType)> = Vec::new();
        let mut open = false;
        match (&inferred, &src.declared) {
            (Some(inf), declared) => {
                for note in &inf.notes {
                    let diag = match note.level {
                        NoteLevel::Warning => Diagnostic::warning(
                            "M201",
                            format!("source `{}`: {}", src.name, note.message),
                        ),
                        NoteLevel::Note => Diagnostic::note(
                            "M201",
                            format!("source `{}`: {}", src.name, note.message),
                        ),
                    };
                    self.err(diag.label(d.name.span, src.kind.describe()));
                }
                for (name, ty) in &inf.columns {
                    let decl = declared
                        .as_ref()
                        .and_then(|cols| cols.iter().find(|c| c.name.eq_ignore_ascii_case(name)));
                    match decl {
                        Some(dc) => {
                            if !compatible_inference(*ty, dc.ty.ty, &dc.formats, text_staged) {
                                self.err(
                                    Diagnostic::warning("M203", format!("column `{name}` is declared `{}` but the source reports `{ty}`", dc.ty))
                                        .label(dc.span, "declared here")
                                        .help(conflict_help(name, *ty, dc.ty.ty, &src.name)),
                                );
                            }
                            columns.push((name.clone(), dc.ty));
                        }
                        None => columns.push((name.clone(), ColType::nullable(*ty))),
                    }
                }
                if let Some(cols) = declared {
                    for dc in cols {
                        if !inf
                            .columns
                            .iter()
                            .any(|(n, _)| n.eq_ignore_ascii_case(&dc.name))
                        {
                            let mut diag = Diagnostic::error(
                                "M202",
                                format!(
                                    "declared column `{}` is not in source `{}`",
                                    dc.name, src.name
                                ),
                            )
                            .label(dc.span, "not found in the source");
                            if let Some(s) =
                                did_you_mean(&dc.name, inf.columns.iter().map(|(n, _)| n.as_str()))
                            {
                                diag = diag.help(format!("did you mean `{s}`?"));
                            } else {
                                diag = diag.help(format!(
                                    "source columns: {}",
                                    inf.columns
                                        .iter()
                                        .map(|(n, _)| format!("`{n}`"))
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                ));
                            }
                            self.err(diag);
                        }
                    }
                }
            }
            (None, Some(cols)) => {
                columns = cols.iter().map(|c| (c.name.clone(), c.ty)).collect();
            }
            (None, None) => {
                open = true;
                self.err(
                    Diagnostic::note("M204", format!("columns of `{}` are not checked: the schema is not declared and the source was not contacted", src.name))
                        .label(d.name.span, src.kind.describe())
                        .help("declare a `schema { ... }` or run `magi check --sources`"),
                );
            }
        }
        // `__magi` names are MAGI's own, and DuckDB column names are case-insensitive
        let mut usable = true;
        for (i, (name, _)) in columns.iter().enumerate() {
            let declared_at = src
                .declared
                .as_ref()
                .and_then(|cols| cols.iter().find(|c| c.name.eq_ignore_ascii_case(name)))
                .map(|c| c.span);
            let (span, label) = match declared_at {
                Some(span) => (span, "reserved for MAGI's internal tables and columns"),
                None => (d.name.span, "the source has a column with this name"),
            };
            usable &= self.not_reserved_at(name, span, label);
            if let Some((other, _)) = columns[..i]
                .iter()
                .find(|(o, _)| o.eq_ignore_ascii_case(name))
            {
                self.err(
                    Diagnostic::error(
                        "M003",
                        format!(
                            "source `{}` has columns `{other}` and `{name}`, which differ only in case",
                            src.name
                        ),
                    )
                    .label(span, "column names are case-insensitive")
                    .help("rename one of them in the source (or alias it in the source's query)"),
                );
                usable = false;
            }
        }
        // `fill_down` names columns of the file; `row_number` adds one
        for (name, span) in fill_down.iter().filter(|_| !open) {
            if columns.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
                continue;
            }
            let mut diag = Diagnostic::error(
                "M215",
                format!(
                    "`fill_down` column `{name}` is not in source `{}`",
                    src.name
                ),
            )
            .label(*span, "not found in the source");
            diag = match did_you_mean(name, columns.iter().map(|(n, _)| n.as_str())) {
                Some(s) => diag.help(format!("did you mean `{s}`?")),
                None => diag.help(format!(
                    "source columns: {}",
                    columns
                        .iter()
                        .map(|(n, _)| format!("`{n}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            };
            self.err(diag);
        }
        if let Some((name, span)) = &row_number {
            if !self.not_reserved_at(
                name,
                *span,
                "reserved for MAGI's internal tables and columns",
            ) {
                usable = false;
            } else if name.contains('.') {
                self.err(
                    Diagnostic::error("M014", format!("`{name}` contains `.`"))
                        .label(*span, "row number column name")
                        .help("`x.part` names an output of relation `x` (`x.rejects`, `r.matches`); choose a name without `.`"),
                );
                usable = false;
            } else if let Some((other, _)) =
                columns.iter().find(|(o, _)| o.eq_ignore_ascii_case(name))
            {
                let mut diag = Diagnostic::error(
                    "M003",
                    format!("source `{}` already has a column `{other}`", src.name),
                )
                .label(*span, "the row number column needs its own name");
                if other != name {
                    diag = diag.help(format!(
                        "names are case-insensitive: `{other}` and `{name}` are the same name"
                    ));
                }
                self.err(diag);
                usable = false;
            } else {
                columns.push((name.clone(), ColType::required(Type::Int)));
            }
        }
        if !usable {
            return false;
        }
        // identity columns as the source spells them (names are case-insensitive)
        let mut identity_cols: Vec<String> = Vec::new();
        if let Some(ids) = &d.identity {
            for id in ids {
                match columns
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case(&id.name))
                {
                    None if !open => {
                        let mut diag = Diagnostic::error(
                            "M205",
                            format!(
                                "identity column `{}` is not a column of `{}`",
                                id.name, src.name
                            ),
                        )
                        .label(id.span, "unknown column");
                        if let Some(s) =
                            did_you_mean(&id.name, columns.iter().map(|(n, _)| n.as_str()))
                        {
                            diag = diag.help(format!("did you mean `{s}`?"));
                        }
                        self.err(diag);
                    }
                    None => identity_cols.push(id.name.clone()),
                    Some((n, ty)) => {
                        if ty.nullable
                            && src.declared.as_ref().is_some_and(|cols| {
                                cols.iter().any(|c| c.name.eq_ignore_ascii_case(n))
                            })
                        {
                            self.err(
                                Diagnostic::warning("M206", format!("identity column `{}` is declared nullable", id.name))
                                    .label(id.span, "a row without identity cannot be traced; the run stops if one occurs")
                                    .help("declare it non-null (e.g. `string`, not `string?`)"),
                            );
                        }
                        identity_cols.push(n.clone());
                    }
                }
            }
            src.identity = Some(identity_cols.clone());
        } else if !src.kind.stable_row_order() {
            self.err(
                Diagnostic::note("M207", format!("source `{}` has no identity and database queries have no stable row order", src.name))
                    .label(d.name.span, "declare `identity <column>`")
                    .help("row identity is needed to trace reconciliation and validation results back to source rows"),
            );
        }
        // a null identity stops the run (M211) before anything reads the source, so identity
        // columns are never null downstream
        let mut cols = Vec::new();
        for (name, mut ty) in columns {
            if identity_cols.contains(&name) {
                ty.nullable = false;
            }
            let lineage = self.hir.lineage.add(
                format!("{}.{name}  ({})", src.name, src.kind.describe()),
                Vec::new(),
            );
            cols.push(Column { name, ty, lineage });
        }
        let rejects_cols = vec![
            self.plain_col("row", ColType::required(Type::Int)),
            self.plain_col("column", ColType::required(Type::String)),
            self.plain_col("expected", ColType::required(Type::String)),
            self.plain_col("value", ColType::nullable(Type::String)),
        ];
        let identity = src.identity.clone();
        let name = src.name.clone();
        let span = src.span;
        let idx = self.hir.sources.len();
        self.hir.sources.push(src);
        self.hir.order.push(Node::Source(idx));
        self.hir.add_relation(Relation {
            name: name.clone(),
            kind: RelKind::Source,
            columns: cols,
            open,
            identity,
            sort: Vec::new(),
            span,
        });
        self.hir.add_relation(Relation {
            name: format!("{name}.rejects"),
            kind: RelKind::SourceRejects,
            columns: rejects_cols,
            open: false,
            identity: None,
            sort: vec![("row".into(), false), ("column".into(), false)],
            span,
        });
        true
    }

    fn plain_col(&mut self, name: &str, ty: ColType) -> Column {
        let lineage = self.hir.lineage.add(name.to_string(), Vec::new());
        Column {
            name: name.to_string(),
            ty,
            lineage,
        }
    }

    fn declared_schema(&mut self, s: &ast::SchemaBlock) -> Vec<DeclaredColumn> {
        let mut out: Vec<DeclaredColumn> = Vec::new();
        for f in &s.fields {
            if out.iter().any(|c| c.name == f.name.name) {
                self.err(
                    Diagnostic::error(
                        "M003",
                        format!("column `{}` is declared twice", f.name.name),
                    )
                    .label(f.name.span, "duplicate"),
                );
                continue;
            }
            if let Some((ty, formats)) = self.type_expr(&f.ty) {
                out.push(DeclaredColumn {
                    name: f.name.name.clone(),
                    ty,
                    formats,
                    span: f.span,
                });
            }
        }
        out
    }

    fn type_expr(&mut self, t: &ast::TypeExpr) -> Option<(ColType, Vec<String>)> {
        use ast::TypeParam;
        let ints: Vec<u32> = t
            .params
            .iter()
            .filter_map(|p| {
                if let TypeParam::Int(v) = p {
                    Some(*v)
                } else {
                    None
                }
            })
            .collect();
        let strs: Vec<String> = t
            .params
            .iter()
            .filter_map(|p| {
                if let TypeParam::Str(s) = p {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .collect();
        let bad_params = |this: &mut Self, what: &str| {
            this.err(
                Diagnostic::error("M101", format!("invalid parameters for `{}`", t.name.name))
                    .label(t.span, what.to_string()),
            );
            None
        };
        let ty = match t.name.name.as_str() {
            "bool" | "boolean" => Type::Bool,
            "int" | "integer" => Type::Int,
            "decimal" | "numeric" => match (ints.as_slice(), strs.is_empty()) {
                ([p, s], true) if *p >= 1 && *p <= 38 && s <= p => {
                    Type::Decimal(*p as u8, *s as u8)
                }
                ([], true) => Type::Decimal(18, 3),
                _ => {
                    return bad_params(
                        self,
                        "expected `decimal(precision, scale)` with 1 <= precision <= 38 and scale <= precision",
                    );
                }
            },
            "float" | "double" => Type::Float,
            "string" | "text" => Type::String,
            "date" => Type::Date,
            "time" => Type::Time,
            "timestamp" => Type::Timestamp,
            "timestamp_tz" => Type::TimestampTz,
            "binary" => Type::Binary,
            "json" => Type::Json,
            other => {
                const TYPES: &[&str] = &[
                    "bool",
                    "int",
                    "decimal",
                    "float",
                    "string",
                    "date",
                    "time",
                    "timestamp",
                    "timestamp_tz",
                    "binary",
                    "json",
                ];
                let mut d = Diagnostic::error("M101", format!("unknown type `{other}`"))
                    .label(t.name.span, "not a MAGI type");
                if let Some(s) = did_you_mean(other, TYPES.iter().copied()) {
                    d = d.help(format!("did you mean `{s}`?"));
                }
                self.err(d);
                return None;
            }
        };
        if !matches!(ty, Type::Decimal(..)) && !ints.is_empty() {
            return bad_params(self, "this type takes no numeric parameters");
        }
        if !strs.is_empty()
            && !matches!(
                ty,
                Type::Date | Type::Timestamp | Type::TimestampTz | Type::Time
            )
        {
            return bad_params(self, "only date/time types take format strings");
        }
        for f in &strs {
            if let Err(e) = self.provider.check_format(f) {
                self.err(
                    Diagnostic::error(
                        "M101",
                        format!("invalid format `{f}` for `{}`: {e}", t.name.name),
                    )
                    .label(t.span, "not a date/time format")
                    .help("formats use strptime specifiers such as `%d/%m/%Y %H:%M`"),
                );
                return None;
            }
            // a zone in the text cannot be kept by a type without one: it would be converted or
            // dropped silently
            if ty != Type::TimestampTz && format_reads_zone(f) {
                self.err(
                    Diagnostic::error(
                        "M101",
                        format!("format `{f}` reads a time zone, which `{ty}` has no place for"),
                    )
                    .label(t.span, "`%z` / `%Z` in the format")
                    .help(format!(
                        "declare `timestamp_tz(\"{f}\")` to keep the instant, or remove the zone from the format"
                    )),
                );
                return None;
            }
        }
        Some((ColType::new(ty, t.nullable), strs))
    }

    // ---- datasets -----------------------------------------------------------------------------

    fn dataset(&mut self, d: &'a ast::DatasetDecl) -> bool {
        let Some(result) = self.pipeline(&d.pipeline, &d.name.name) else {
            return false;
        };
        let pipeline::PipelineResult {
            plan,
            columns,
            identity,
            sort,
            uses,
            backend_specific,
            open,
            native_open,
        } = result;
        if native_open {
            self.native_open.insert(d.name.name.clone());
        }
        let idx = self.hir.datasets.len();
        self.hir.datasets.push(Dataset {
            name: d.name.name.clone(),
            plan,
            uses,
            backend_specific,
            span: d.name.span,
        });
        self.hir.order.push(Node::Dataset(idx));
        self.hir.add_relation(Relation {
            name: d.name.name.clone(),
            kind: RelKind::Dataset,
            columns,
            open,
            identity,
            sort,
            span: d.span,
        });
        true
    }

    // ---- validation ---------------------------------------------------------------------------

    fn validation(&mut self, v: &'a ast::ValidateDecl) {
        let Some(rel) = self.rel_ref(&v.target) else {
            return;
        };
        let target = self.relation(rel).name.clone();
        let scope = Scope::single(self.relation(rel), &target);
        let mut checks = Vec::new();
        for c in &v.checks {
            let label = match &c.label {
                Some(l) => l.value.clone(),
                None => {
                    let text = self.text(c.span);
                    text.split_once(char::is_whitespace)
                        .map(|(_, rest)| rest.trim().to_string())
                        .unwrap_or_default()
                }
            };
            let kind = match &c.kind {
                AstCheckKind::Predicate(e) => {
                    let Some(t) = self.expr(e, &scope, &expr::AggMode::Whole) else {
                        continue;
                    };
                    if !self.expect_bool(&t, e.span, "a check") {
                        continue;
                    }
                    if t.contains_agg() {
                        if !self.agg_only(e, &scope) {
                            continue;
                        }
                        CheckKind::Aggregate(t)
                    } else {
                        CheckKind::Row(t)
                    }
                }
                AstCheckKind::NotNull(e) => {
                    let Some(t) =
                        self.expr(e, &scope, &expr::AggMode::Forbidden("a `not null` check"))
                    else {
                        continue;
                    };
                    if !t.ty.nullable && !t.ty.ty.is_unknown() {
                        self.err(
                            Diagnostic::note(
                                "M401",
                                "this check cannot fail: the expression is never null",
                            )
                            .label(e.span, format!("type is `{}`", t.ty)),
                        );
                    }
                    CheckKind::NotNull(t)
                }
                AstCheckKind::Unique(cols) => {
                    let mut names = Vec::new();
                    for col in cols {
                        if let Some(sc) = self.lookup(col, &scope) {
                            names.push(sc.phys.clone());
                        }
                    }
                    if names.len() != cols.len() {
                        continue;
                    }
                    CheckKind::Unique(names)
                }
            };
            checks.push(Check {
                label,
                severity: c.severity,
                kind,
                span: c.span,
            });
        }
        let target_cols = self.relation(rel).columns.clone();
        let is_source = self.relation(rel).kind == RelKind::Source;
        let mut failures = vec![
            self.plain_col("check", ColType::required(Type::String)),
            self.plain_col("severity", ColType::required(Type::String)),
        ];
        if is_source {
            // the failing row's position in the source file or sheet (as in `<source>.rejects`)
            failures.push(self.plain_col("source_row", ColType::required(Type::Int)));
        }
        for c in &target_cols {
            let name = if matches!(c.name.as_str(), "check" | "severity" | "source_row") {
                format!("row_{}", c.name)
            } else {
                c.name.clone()
            };
            failures.push(Column {
                name,
                ty: c.ty.with_nullable(true),
                lineage: c.lineage,
            });
        }
        let checks_cols = vec![
            self.plain_col("check", ColType::required(Type::String)),
            self.plain_col("severity", ColType::required(Type::String)),
            self.plain_col("status", ColType::required(Type::String)),
            self.plain_col("failing_rows", ColType::nullable(Type::Int)),
            self.plain_col("total_rows", ColType::required(Type::Int)),
            // the compared aggregate of an aggregate check, as text
            self.plain_col("measured", ColType::nullable(Type::String)),
        ];
        let idx = self.hir.validations.len();
        self.hir.validations.push(Validation {
            target: target.clone(),
            checks,
            span: v.target.span,
        });
        self.hir.order.push(Node::Validation(idx));
        self.hir.add_relation(Relation {
            name: format!("{target}.failures"),
            kind: RelKind::ValidationPart,
            columns: failures,
            open: false,
            identity: None,
            sort: Vec::new(),
            span: v.span,
        });
        self.hir.add_relation(Relation {
            name: format!("{target}.checks"),
            kind: RelKind::ValidationPart,
            columns: checks_cols,
            open: false,
            identity: None,
            sort: Vec::new(),
            span: v.span,
        });
    }

    fn expect_bool(&mut self, t: &TExpr, span: Span, what: &str) -> bool {
        if matches!(t.ty.ty, Type::Bool | Type::Unknown | Type::Null) {
            true
        } else {
            self.err(
                Diagnostic::error(
                    "M102",
                    format!("{what} must be a condition (bool), found `{}`", t.ty.ty),
                )
                .label(span, "not a condition"),
            );
            false
        }
    }

    /// Aggregate checks may only reference columns inside aggregate functions.
    fn agg_only(&mut self, e: &ast::Expr, _scope: &Scope) -> bool {
        fn bare_columns<'e>(e: &'e ast::Expr, in_agg: bool, out: &mut Vec<&'e ast::Expr>) {
            match &e.kind {
                ExprKind::Column(_) if !in_agg => out.push(e),
                ExprKind::Call {
                    namespace: None,
                    name,
                    args,
                } => {
                    let agg = in_agg || crate::semantic::functions::is_aggregate(&name.name);
                    args.iter().for_each(|a| bare_columns(a, agg, out));
                }
                ExprKind::Call { args, .. } | ExprKind::List(args) => {
                    args.iter().for_each(|a| bare_columns(a, in_agg, out))
                }
                ExprKind::Unary { expr, .. }
                | ExprKind::IsNull { expr, .. }
                | ExprKind::Paren(expr) => bare_columns(expr, in_agg, out),
                ExprKind::Binary { left, right, .. } => {
                    bare_columns(left, in_agg, out);
                    bare_columns(right, in_agg, out);
                }
                ExprKind::InList { expr, list, .. } => {
                    bare_columns(expr, in_agg, out);
                    list.iter().for_each(|a| bare_columns(a, in_agg, out));
                }
                ExprKind::Case { arms, otherwise } => {
                    for arm in arms {
                        bare_columns(&arm.when, in_agg, out);
                        bare_columns(&arm.then, in_agg, out);
                    }
                    if let Some(o) = otherwise {
                        bare_columns(o, in_agg, out);
                    }
                }
                _ => {}
            }
        }
        let mut bare = Vec::new();
        bare_columns(e, false, &mut bare);
        for b in &bare {
            self.err(
                Diagnostic::error("M103", "a check that uses aggregates cannot also refer to single rows")
                    .label(b.span, "column outside an aggregate")
                    .help("either check every row (no aggregates) or wrap this column in an aggregate such as `max(...)`"),
            );
        }
        bare.is_empty()
    }

    // ---- exports ------------------------------------------------------------------------------

    fn export(&mut self, e: &'a ast::ExportDecl) {
        let raw = &e.path.value;
        let path = self.base_dir(e.span).join(raw);
        let ext = path
            .extension()
            .and_then(|x| x.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        let format = match ext.as_str() {
            "csv" => ExportFormat::Csv,
            "parquet" => ExportFormat::Parquet,
            "xlsx" => ExportFormat::Xlsx,
            "duckdb" | "db" => ExportFormat::DuckDb,
            "json" | "jsonl" | "ndjson" => ExportFormat::Json,
            _ => {
                self.err(
                    Diagnostic::error("M501", format!("cannot tell the output format of `{raw}`"))
                        .label(e.path.span, "unknown file extension")
                        .help("use .csv, .parquet, .xlsx, .duckdb or .json"),
                );
                return;
            }
        };
        let mut parts = Vec::new();
        let mut name_opt: Option<String> = None;
        for o in &e.options {
            match (o.key.name.as_str(), format) {
                ("sheet", ExportFormat::Xlsx) | ("table", ExportFormat::DuckDb) => {
                    name_opt = self.opt_str(o)
                }
                _ => {
                    let allowed: &[&str] = match format {
                        ExportFormat::Xlsx => &["sheet"],
                        ExportFormat::DuckDb => &["table"],
                        _ => &[],
                    };
                    self.unknown_option(o, allowed, "this export");
                }
            }
        }
        match &e.target {
            ast::ExportTarget::Single(r) => {
                let Some(i) = self.rel_ref(r) else {
                    return;
                };
                let rel = self.relation(i).name.clone();
                let default = match format {
                    ExportFormat::Xlsx => rel.chars().take(31).collect(),
                    _ => rel.replace('.', "_"),
                };
                parts.push(ExportPart {
                    name: name_opt.unwrap_or(default),
                    relation: rel,
                });
            }
            ast::ExportTarget::Multi(ps) => {
                if !matches!(format, ExportFormat::Xlsx | ExportFormat::DuckDb) {
                    self.err(
                        Diagnostic::error(
                            "M502",
                            "only .xlsx and .duckdb exports can hold several relations",
                        )
                        .label(e.path.span, "single-relation format")
                        .help("write one `export <relation> to \"...\"` per file"),
                    );
                    return;
                }
                let kw = if format == ExportFormat::Xlsx {
                    "sheet"
                } else {
                    "table"
                };
                for p in ps {
                    let head = self.text(p.span).split_whitespace().next().unwrap_or("");
                    if head != kw {
                        self.err(
                            Diagnostic::error(
                                "M502",
                                format!("use `{kw}` entries for this format"),
                            )
                            .label(p.span, format!("expected `{kw} \"name\" = relation`")),
                        );
                        continue;
                    }
                    if parts
                        .iter()
                        .any(|x: &ExportPart| x.name.eq_ignore_ascii_case(&p.name.value))
                    {
                        self.err(
                            Diagnostic::error(
                                "M503",
                                format!("{kw} name `{}` is used twice", p.name.value),
                            )
                            .label(p.name.span, "duplicate"),
                        );
                        continue;
                    }
                    if let Some(i) = self.rel_ref(&p.rel) {
                        parts.push(ExportPart {
                            name: p.name.value.clone(),
                            relation: self.relation(i).name.clone(),
                        });
                    }
                }
                if parts.is_empty() {
                    return;
                }
            }
        }
        if format == ExportFormat::Xlsx {
            for p in &parts {
                if p.name.chars().count() > 31
                    || p.name.chars().any(|c| "[]:*?/\\".contains(c))
                    || p.name.is_empty()
                {
                    self.err(
                        Diagnostic::error(
                            "M503",
                            format!("`{}` is not a valid Excel sheet name", p.name),
                        )
                        .label(e.span, "sheet names are 1-31 characters without []:*?/\\")
                        .help("set one explicitly: `{ sheet: \"Matches\" }`"),
                    );
                    return;
                }
            }
        }
        // `out/a.csv`, `./out/a.csv` and `out/../out/a.csv` are one file
        let identity = crate::export::file_identity(&path);
        if self
            .hir
            .exports
            .iter()
            .any(|x| crate::export::file_identity(&x.path) == identity)
        {
            self.err(
                Diagnostic::error(
                    "M504",
                    format!("`{raw}` is written by more than one export"),
                )
                .label(e.path.span, "same output file"),
            );
            return;
        }
        let idx = self.hir.exports.len();
        self.hir.exports.push(Export {
            path,
            display_path: raw.clone(),
            format,
            parts,
            span: e.path.span,
        });
        self.hir.order.push(Node::Export(idx));
    }
}

/// Whether a source-reported type fits a declared one without values being rejected or changed.
/// `text`: the source's values arrive as text (CSV, Excel, ODBC) and are parsed by MAGI.
fn compatible_inference(inferred: Type, declared: Type, formats: &[String], text: bool) -> bool {
    let temporal = matches!(
        declared,
        Type::Date | Type::Timestamp | Type::TimestampTz | Type::Time
    );
    match (inferred, declared) {
        (a, b) if a == b => true,
        (Type::Unknown | Type::Null, _) | (_, Type::String) => true,
        // text is parsed with the declared formats; values that do not match become rejects
        _ if temporal && !formats.is_empty() && (text || inferred == Type::String) => true,
        (Type::String, Type::Json) => true,
        (Type::Int, Type::Decimal(..) | Type::Float) => true,
        // a smaller scale rounds (M209); fewer integer digits reject the values that need them
        (Type::Decimal(p1, s1), Type::Decimal(p2, s2)) => {
            s1 <= s2 && p1.saturating_sub(s1) <= p2.saturating_sub(s2)
        }
        (Type::Decimal(..), Type::Float) => true,
        (Type::Date, Type::Timestamp | Type::TimestampTz) => true,
        // int <- decimal/float rejects fractions, date <- timestamp rejects times, decimal <- float
        // rounds, and text is not parsed without a format
        _ => false,
    }
}

/// Whether a date/time format reads a time zone (`%z` offset or `%Z` name).
fn format_reads_zone(format: &str) -> bool {
    let mut chars = format.chars();
    // the character after each `%` is a specifier (`%%` is a literal percent sign)
    while let Some(c) = chars.next() {
        if c == '%' && matches!(chars.next(), Some('z' | 'Z')) {
            return true;
        }
    }
    false
}

/// A `query:` without the trailing `;` (and whitespace) SQL scripts end statements with: the query
/// is described and staged as a subquery, where a `;` is a syntax error.
fn statement_body(query: String) -> String {
    query
        .trim_end_matches(|c: char| c == ';' || c.is_whitespace())
        .to_string()
}

/// What happens to the values of a column whose declared type conflicts with the source's (M203).
fn conflict_help(column: &str, inferred: Type, declared: Type, source: &str) -> String {
    let rejects = format!("become null and are listed in `{source}.rejects`");
    match declared {
        Type::Decimal(_, s) => format!(
            "values that are not numbers or do not fit {rejects}; values with more than {s} decimal places are rounded (M209)"
        ),
        Type::Date | Type::Time | Type::Timestamp | Type::TimestampTz
            if inferred == Type::String =>
        {
            let example = match declared {
                Type::Date => "%d/%m/%Y",
                Type::Time => "%H.%M",
                Type::TimestampTz => "%d/%m/%Y %H:%M %z",
                _ => "%d/%m/%Y %H:%M",
            };
            format!(
                "values that do not convert exactly to {declared} {rejects}; declare the format of text that is not ISO, e.g. `{column}: {declared}(\"{example}\")`"
            )
        }
        _ => format!("values that do not convert exactly to {declared} {rejects}"),
    }
}
