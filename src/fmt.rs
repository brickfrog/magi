//! `magi fmt`: canonical source formatting.
//!
//! The formatter re-prints the parsed AST, so its output always re-parses to the same program.
//! Expressions are printed token for token: grouping the analyst wrote is an explicit `Paren`
//! node and is kept, and no other parentheses are introduced (the parser's precedence already
//! gave the tree its shape, so none are ever needed).
//!
//! Comments are not in the AST; they are re-attached by source position. A comment on its own
//! line stays before the statement / step / block item / multi-line `case` arm it precedes, at
//! that element's indentation; a comment after code stays at the end of that line; comments
//! before a closing `}` stay inside the block; comments after the last statement stay at the end
//! of the file. One blank line is kept between block items where the original had at least one.
//!
//! Statements are separated by one blank line, except that consecutive one-line statements of
//! the same kind that were adjacent in the original (a run of `export`s, say) stay adjacent.

use std::cell::RefCell;

use crate::ast::*;
use crate::diagnostic::Diagnostic;
use crate::syntax::parser::parse;
use crate::syntax::span::{FileId, Span};
use crate::syntax::token::Comment;

const INDENT: usize = 4;
/// Target line width; only pipelines and blocks break lines.
const MAX_WIDTH: usize = 100;
/// A `case` longer than this is written with one arm per line.
const CASE_INLINE_MAX: usize = 80;
/// Mapping arms align their `=>` only across patterns up to this many characters.
const ARM_ALIGN_MAX: usize = 40;

/// Format one `.magi` file. Files with parse errors are never formatted: the parse diagnostics
/// are returned instead.
pub fn format_source(file: FileId, src: &str) -> Result<String, Vec<Diagnostic>> {
    let parsed = parse(file, src);
    if parsed.diagnostics.iter().any(Diagnostic::is_error) {
        return Err(parsed.diagnostics);
    }
    let mut f = Formatter {
        src,
        used: vec![false; parsed.comments.len()],
        first_unused: 0,
        comments: parsed.comments,
        lines: Vec::new(),
        indent: 0,
        prev_end: None,
        no_blank: false,
        embedded: RefCell::new(Vec::new()),
    };
    f.program(&parsed.program);
    Ok(f.finish())
}

struct Line {
    text: String,
    /// The line already ends with (or is) a comment.
    has_comment: bool,
}

/// Text of an element together with the comments written inside it (in multi-line `case`
/// arms), which are consumed when the text is emitted.
struct Rendered {
    text: String,
    embedded: Vec<usize>,
}

impl From<String> for Rendered {
    fn from(text: String) -> Self {
        Rendered {
            text,
            embedded: Vec::new(),
        }
    }
}

impl From<&str> for Rendered {
    fn from(text: &str) -> Self {
        text.to_string().into()
    }
}

/// One line-oriented element inside a block (or a pipeline step).
#[derive(Clone, Copy)]
enum Item<'a> {
    Opt(&'a Opt),
    Schema(&'a SchemaBlock),
    Field(&'a SchemaField),
    Identity(&'a [Ident]),
    /// Mapping arm with the pattern column width used to align `=>`.
    Arm(&'a MapArm, usize),
    Assign(&'a Assign),
    Select(&'a SelectItem),
    Rename(&'a Rename),
    Check(&'a Check),
    Recon(&'a ReconcileItem),
    Tier(&'a TierItem),
    Expr(&'a Expr),
    Part(&'a ExportPart),
    Step(&'a Step),
    Model(&'a ModelItem),
}

impl Item<'_> {
    fn range(&self) -> (u32, u32) {
        let span = match self {
            Item::Opt(o) => o.span,
            Item::Schema(s) => s.span,
            Item::Field(f) => f.span,
            Item::Identity(ids) => match (ids.first(), ids.last()) {
                (Some(a), Some(b)) => a.span.to(b.span),
                _ => Span::default(),
            },
            Item::Arm(a, _) => a.span,
            Item::Assign(a) => a.span,
            Item::Select(SelectItem::Column(c)) => c.span,
            Item::Select(SelectItem::Assign(a)) => a.span,
            Item::Rename(r) => r.span,
            Item::Check(c) => c.span,
            Item::Recon(r) => match r {
                ReconcileItem::Block(_, s)
                | ReconcileItem::Cardinality(_, s)
                | ReconcileItem::Consume(_, s)
                | ReconcileItem::Ambiguity(_, s)
                | ReconcileItem::Duplicates(_, s)
                | ReconcileItem::Evidence(_, s)
                | ReconcileItem::Identity { span: s, .. } => *s,
                ReconcileItem::Tier(t) => t.span,
                ReconcileItem::Flag(f) => f.span,
            },
            Item::Tier(t) => match t {
                TierItem::Require(e) => e.span,
                TierItem::Rank(_, s)
                | TierItem::Block(_, s)
                | TierItem::Compare(_, s)
                | TierItem::Evidence(_, s)
                | TierItem::Shape { span: s, .. }
                | TierItem::Group { span: s, .. }
                | TierItem::Subset { span: s, .. } => *s,
                TierItem::Flag(f) => f.span,
            },
            Item::Expr(e) => e.span,
            Item::Part(p) => p.span,
            Item::Step(s) => s.span,
            Item::Model(m) => match m {
                ModelItem::Table { span, .. }
                | ModelItem::Relationship { span, .. }
                | ModelItem::Dimension { span, .. }
                | ModelItem::Metric { span, .. } => *span,
            },
        };
        (span.start, span.end)
    }

    fn is_tier(&self) -> bool {
        matches!(self, Item::Recon(ReconcileItem::Tier(_)))
    }
}

struct Formatter<'s> {
    src: &'s str,
    comments: Vec<Comment>,
    used: Vec<bool>,
    /// Every comment before this index is consumed.
    first_unused: usize,
    lines: Vec<Line>,
    indent: usize,
    /// End of the last source element emitted in the current block; `None` right after `{`.
    prev_end: Option<u32>,
    /// Suppress blank lines (between consecutive imports).
    no_blank: bool,
    /// Comments placed by the rendering in progress (see [`Formatter::render`]).
    embedded: RefCell<Vec<usize>>,
}

impl Formatter<'_> {
    fn finish(mut self) -> String {
        while self.lines.last().is_some_and(|l| l.text.is_empty()) {
            self.lines.pop();
        }
        let mut out = String::new();
        for line in &self.lines {
            out.push_str(&line.text);
            out.push('\n');
        }
        out
    }

    // ---- output + comment plumbing ------------------------------------------------------------

    fn push(&mut self, text: &str) {
        let mut line = " ".repeat(self.indent);
        line.push_str(text);
        self.lines.push(Line {
            text: line,
            has_comment: false,
        });
    }

    fn blank(&mut self) {
        if !self.no_blank && self.lines.last().is_some_and(|l| !l.text.is_empty()) {
            self.lines.push(Line {
                text: String::new(),
                has_comment: false,
            });
        }
    }

    fn set_prev_end(&mut self, end: u32) {
        self.prev_end = Some(self.prev_end.map_or(end, |p| p.max(end)));
    }

    /// Offset of the newline ending the source line that contains `pos`.
    fn line_end(&self, pos: u32) -> u32 {
        let p = (pos as usize).min(self.src.len());
        self.src.as_bytes()[p..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(self.src.len(), |i| p + i) as u32
    }

    /// Whether the source between `a` and `b` contains a blank line.
    fn blank_between(&self, a: u32, b: u32) -> bool {
        let mut newline = false;
        for &c in self
            .src
            .as_bytes()
            .get(a as usize..b as usize)
            .unwrap_or_default()
        {
            match c {
                b'\n' if newline => return true,
                b'\n' => newline = true,
                b' ' | b'\t' | b'\r' => {}
                _ => newline = false,
            }
        }
        false
    }

    /// Keep one blank line where the original had one between the previous element and `pos`.
    fn gap_blank(&mut self, pos: u32) {
        if self.prev_end.is_some_and(|pe| self.blank_between(pe, pos)) {
            self.blank();
        }
    }

    /// Unconsumed comments starting in `lo..hi`, in source order.
    fn pending_iter(&self, lo: u32, hi: u32) -> impl Iterator<Item = usize> + '_ {
        let from = self
            .first_unused
            .max(self.comments.partition_point(|c| c.span.start < lo));
        (from..self.comments.len())
            .take_while(move |&i| self.comments[i].span.start < hi)
            .filter(|&i| !self.used[i])
    }

    fn pending(&self, lo: u32, hi: u32) -> Vec<usize> {
        self.pending_iter(lo, hi).collect()
    }

    fn has_pending(&self, lo: u32, hi: u32) -> bool {
        self.pending_iter(lo, hi).next().is_some()
    }

    fn consume(&mut self, i: usize) {
        self.used[i] = true;
        while self.used.get(self.first_unused).is_some_and(|&u| u) {
            self.first_unused += 1;
        }
        self.set_prev_end(self.comments[i].span.end);
    }

    fn comment_line(&mut self, i: usize) {
        let text = self.comments[i].text.clone();
        self.push(&text);
        if let Some(l) = self.lines.last_mut() {
            l.has_comment = true;
        }
        self.consume(i);
    }

    /// Append comment `i` to the last output line, if that line can take a trailing comment.
    fn append_comment(&mut self, i: usize) -> bool {
        let Some(last) = self.lines.last_mut() else {
            return false;
        };
        if last.text.is_empty() || last.has_comment {
            return false;
        }
        last.text.push_str("  ");
        last.text.push_str(&self.comments[i].text);
        last.has_comment = true;
        self.consume(i);
        true
    }

    /// Comments before the next element. With `attach`, a comment that followed code in the
    /// original goes to the end of the previous output line.
    fn leading(&mut self, lo: u32, hi: u32, attach: bool) {
        for i in self.pending(lo, hi) {
            if attach && self.comments[i].trailing && self.append_comment(i) {
                continue;
            }
            self.gap_blank(self.comments[i].span.start);
            self.comment_line(i);
        }
    }

    /// Emit a one-line element spanning `start..end`. Comments inside it (which cannot stay
    /// where they were once the element is on one line) move above it, except a final trailing
    /// comment, which stays at the end of the line together with anything up to `limit`.
    fn atomic(&mut self, text: Rendered, start: u32, end: u32, limit: u32) {
        for &i in &text.embedded {
            self.consume(i);
        }
        let mut inside = self.pending(start, limit);
        let tail = match inside.last() {
            Some(&i) if self.comments[i].trailing => inside.pop(),
            _ => None,
        };
        for i in inside {
            self.comment_line(i);
        }
        self.push(&text.text);
        self.set_prev_end(end);
        if let Some(i) = tail {
            self.append_comment(i);
        }
    }

    /// Comments after a closing `}` on the same line.
    fn trailing(&mut self, limit: u32) {
        for i in self.pending(0, limit) {
            if !self.append_comment(i) {
                self.comment_line(i);
            }
        }
    }

    /// `header { items }`. With `optional`, the braces are omitted when there is nothing
    /// (not even a comment) to put in them.
    fn braced(
        &mut self,
        header: Rendered,
        items: &[Item],
        span: Span,
        limit: u32,
        tiers: bool,
        optional: bool,
    ) {
        if items.is_empty() && !self.has_pending(span.start, span.end) {
            let text = if optional {
                header
            } else {
                format!("{} {{}}", header.text).into()
            };
            self.atomic(text, span.start, span.end, limit);
            return;
        }
        for &i in &header.embedded {
            self.consume(i);
        }
        self.push(&format!("{} {{", header.text));
        self.indent += INDENT;
        self.items(items, span.end, tiers);
        self.leading(0, span.end, true);
        self.indent -= INDENT;
        self.push("}");
        self.prev_end = Some(span.end);
        self.trailing(limit);
    }

    /// One item per line. `close` bounds the comments the last item may claim. With `tiers`,
    /// every `tier` is set off by blank lines.
    fn items(&mut self, items: &[Item], close: u32, tiers: bool) {
        self.prev_end = None;
        for (i, it) in items.iter().enumerate() {
            let (start, end) = it.range();
            let next = items.get(i + 1).copied();
            let limit = self.line_end(end).min(next.map_or(close, |n| n.range().0));
            if tiers && i > 0 && (it.is_tier() || items[i - 1].is_tier()) {
                self.blank();
            }
            self.leading(0, start, true);
            self.gap_blank(start);
            self.item(*it, next, start, end, limit);
        }
    }

    fn item(&mut self, it: Item, next: Option<Item>, start: u32, end: u32, limit: u32) {
        match it {
            Item::Schema(s) => {
                let fields: Vec<Item> = s.fields.iter().map(Item::Field).collect();
                self.braced("schema".into(), &fields, s.span, limit, false, false);
            }
            Item::Recon(ReconcileItem::Tier(t)) => {
                let items: Vec<Item> = t.items.iter().map(Item::Tier).collect();
                self.braced(
                    format!("tier {}", ident(&t.name)).into(),
                    &items,
                    t.span,
                    limit,
                    false,
                    false,
                );
            }
            Item::Recon(ReconcileItem::Evidence(assigns, span))
            | Item::Tier(TierItem::Evidence(assigns, span)) => {
                if let [a] = assigns.as_slice()
                    && let Some(text) =
                        self.inline_block(*span, || format!("evidence {{ {} }}", self.assign(a)))
                {
                    return self.atomic(text, start, end, limit);
                }
                let items: Vec<Item> = assigns.iter().map(Item::Assign).collect();
                self.braced("evidence".into(), &items, *span, limit, false, false);
            }
            Item::Tier(TierItem::Compare(exprs, span)) => {
                if let [e] = exprs.as_slice()
                    && let Some(text) =
                        self.inline_block(*span, || format!("compare {{ {} }}", self.expr(e)))
                {
                    return self.atomic(text, start, end, limit);
                }
                let items: Vec<Item> = exprs.iter().map(Item::Expr).collect();
                self.braced("compare".into(), &items, *span, limit, false, false);
            }
            Item::Step(s) => self.step(s, limit),
            Item::Model(
                ModelItem::Table { options, span, .. }
                | ModelItem::Dimension { options, span, .. }
                | ModelItem::Metric { options, span, .. },
            ) => {
                let header = self.render(|| self.item_text(it));
                let items: Vec<Item> = options.iter().map(Item::Opt).collect();
                self.braced(header, &items, *span, limit, false, true);
            }
            _ => {
                let mut text = self.render(|| self.item_text(it));
                // Newline-separated items are not self-delimiting when the next one could
                // continue this one's trailing expression (`x` then `-1`, `(`, `and`, ...).
                if next.is_some_and(|n| continues_expr(&self.render(|| self.item_text(n)).text)) {
                    text.text.push(',');
                }
                self.atomic(text, start, end, limit);
            }
        }
    }

    /// Render `f` for emission: the result carries the comments it placed.
    fn render(&self, f: impl FnOnce() -> String) -> Rendered {
        self.embedded.borrow_mut().clear();
        let text = f();
        Rendered {
            text,
            embedded: std::mem::take(&mut *self.embedded.borrow_mut()),
        }
    }

    /// A one-item block written on one line (`evidence { x = e }`), when no comment needs the
    /// block's lines and the line fits.
    fn inline_block(&self, span: Span, f: impl FnOnce() -> String) -> Option<Rendered> {
        if self.has_pending(span.start, span.end) {
            return None;
        }
        let text = self.render(f);
        self.fits(&text.text).then_some(text)
    }

    /// Text of a one-line item (for block items: their leading keyword).
    fn item_text(&self, it: Item) -> String {
        match it {
            Item::Opt(o) => format!("{}: {}", ident(&o.key), self.expr(&o.value)),
            Item::Schema(_) => "schema".into(),
            Item::Field(f) => format!("{}: {}", ident(&f.name), type_expr(&f.ty)),
            Item::Identity(ids) => format!("identity {}", ident_or_list(ids)),
            Item::Arm(arm, width) => {
                let pat = self.pattern(&arm.pattern);
                let value = match &arm.value {
                    MapValue::Original => "original".to_string(),
                    MapValue::Expr(e) => self.expr(e),
                };
                if pat.contains('\n') || pat.chars().count() > width {
                    format!("{pat} => {value}")
                } else {
                    format!("{pat:<width$} => {value}")
                }
            }
            Item::Assign(a) => self.assign(a),
            Item::Select(s) => self.select_item(s),
            Item::Rename(r) => rename(r),
            Item::Check(c) => self.check(c),
            Item::Recon(r) => match r {
                ReconcileItem::Block(spec, _) => format!("block by {}", self.block_spec(spec)),
                ReconcileItem::Cardinality(v, _) => format!("cardinality {}", ident(v)),
                ReconcileItem::Consume(v, _) => format!("consume {}", ident(v)),
                ReconcileItem::Ambiguity(v, _) => format!("ambiguity {}", ident(v)),
                ReconcileItem::Duplicates(v, _) => format!("duplicates {}", ident(v)),
                ReconcileItem::Identity { side, cols, .. } => {
                    format!("identity {}: {}", ident(side), ident_or_list(cols))
                }
                ReconcileItem::Tier(t) => format!("tier {}", ident(&t.name)),
                ReconcileItem::Evidence(..) => "evidence".into(),
                ReconcileItem::Flag(f) => self.flag(f),
            },
            Item::Tier(t) => match t {
                TierItem::Require(e) => format!("require {}", self.expr(e)),
                TierItem::Rank(keys, _) => format!("rank by {}", self.sort_keys(keys)),
                TierItem::Block(spec, _) => format!("block by {}", self.block_spec(spec)),
                TierItem::Shape {
                    a_many,
                    b_many,
                    a_side,
                    b_side,
                    ..
                } => {
                    let card = |many: bool| if many { "many" } else { "one" };
                    format!(
                        "{} {} to {} {}",
                        card(*a_many),
                        ident(a_side),
                        card(*b_many),
                        ident(b_side)
                    )
                }
                TierItem::Group { side, keys, .. } => {
                    format!("group {} by {}", ident(side), ident_list(keys))
                }
                TierItem::Subset {
                    side,
                    max_items,
                    max_subsets,
                    ..
                } => {
                    let mut text = format!("subset {} max_items {max_items}", ident(side));
                    if let Some(m) = max_subsets {
                        text.push_str(&format!(" max_subsets {m}"));
                    }
                    text
                }
                TierItem::Compare(..) => "compare".into(),
                TierItem::Evidence(..) => "evidence".into(),
                TierItem::Flag(f) => self.flag(f),
            },
            Item::Expr(e) => self.expr(e),
            Item::Model(m) => match m {
                ModelItem::Table { rel, .. } => format!("table {}", ident(rel)),
                ModelItem::Relationship { from, to, .. } => {
                    format!("relationship {} -> {}", column_name(from), column_name(to))
                }
                ModelItem::Dimension { name, column, .. } => {
                    format!("dimension {} = {}", ident(name), column_name(column))
                }
                ModelItem::Metric { name, .. } => format!("metric {}", ident(name)),
            },
            Item::Part(p) => {
                // The keyword (`sheet` / `table`) is not in the AST; keep the one written.
                let kw = if self
                    .src
                    .get(p.span.start as usize..)
                    .is_some_and(|s| s.starts_with("table"))
                {
                    "table"
                } else {
                    "sheet"
                };
                format!("{kw} {} = {}", str_lit(&p.name), rel_ref(&p.rel))
            }
            Item::Step(_) => "|>".into(),
        }
    }

    /// Arms align `=>` across their patterns, except patterns longer than `ARM_ALIGN_MAX`: those
    /// keep one space, so a long list does not push every other arm's value far to the right.
    fn arm_items<'a>(&self, arms: &'a [MapArm]) -> Vec<Item<'a>> {
        let width = arms
            .iter()
            .map(|a| self.pattern(&a.pattern))
            .filter(|p| !p.contains('\n'))
            .map(|p| p.chars().count())
            .filter(|&n| n <= ARM_ALIGN_MAX)
            .max()
            .unwrap_or(0);
        arms.iter().map(|a| Item::Arm(a, width)).collect()
    }

    fn fits(&self, text: &str) -> bool {
        !text.contains('\n') && self.indent + text.chars().count() <= MAX_WIDTH
    }

    // ---- statements ---------------------------------------------------------------------------

    fn program(&mut self, program: &Program) {
        let stmts = &program.statements;
        let is_import = |s: &Statement| matches!(s, Statement::Import(_));
        // Imports first, each group in original order.
        let order: Vec<usize> = (0..stmts.len())
            .filter(|&i| is_import(&stmts[i]))
            .chain((0..stmts.len()).filter(|&i| !is_import(&stmts[i])))
            .collect();
        if order.first().is_some_and(|&i| i != 0) {
            // Imports move above the first statement, but not above a file header: the
            // comments at the top that a blank line separates from that statement.
            let first = stmts[0].span().start;
            let top = self.pending(0, first);
            let header_end = top
                .iter()
                .enumerate()
                .rev()
                .find(|&(k, &c)| {
                    let next = top
                        .get(k + 1)
                        .map_or(first, |&n| self.comments[n].span.start);
                    self.blank_between(self.comments[c].span.end, next)
                })
                .map(|(_, &c)| self.comments[c].span.end);
            if let Some(end) = header_end {
                self.leading(0, end, false);
                self.blank();
            }
        }
        // Whether the previously emitted statement came out as a single line.
        let mut prev_single = false;
        for (k, &i) in order.iter().enumerate() {
            let stmt = &stmts[i];
            let span = stmt.span();
            // This statement's comment region: after the previous statement's line.
            let lo = match i.checked_sub(1) {
                Some(p) => self.line_end(stmts[p].span().end).min(span.start),
                None => 0,
            };
            let next = stmts.get(i + 1).map_or(u32::MAX, |s| s.span().start);
            let limit = self.line_end(span.end).min(next);
            let prev = k.checked_sub(1).map(|k| order[k]);
            let both_imports = prev.is_some_and(|p| is_import(&stmts[p])) && is_import(stmt);
            let group_start = self.lines.len();
            self.no_blank = both_imports;
            self.prev_end = None;
            self.leading(lo, span.start, false);
            self.gap_blank(span.start);
            let stmt_start = self.lines.len();
            self.statement(stmt, limit);
            self.no_blank = false;
            let single =
                self.lines.len() == stmt_start + 1 && !self.lines[stmt_start].text.contains('\n');
            if let Some(p) = prev {
                let adjacent = both_imports
                    || (prev_single
                        && single
                        && p + 1 == i
                        && std::mem::discriminant(&stmts[p]) == std::mem::discriminant(stmt)
                        && !self.blank_between(stmts[p].span().end, span.start));
                if !adjacent && group_start > 0 && !self.lines[group_start - 1].text.is_empty() {
                    self.lines.insert(
                        group_start,
                        Line {
                            text: String::new(),
                            has_comment: false,
                        },
                    );
                }
            }
            prev_single = single;
        }
        // End-of-file comments.
        self.prev_end = stmts.last().map(|s| s.span().end);
        self.leading(0, u32::MAX, false);
    }

    fn statement(&mut self, stmt: &Statement, limit: u32) {
        let span = stmt.span();
        match stmt {
            Statement::Import(d) => {
                self.atomic(
                    format!("import {}", str_lit(&d.path)).into(),
                    span.start,
                    span.end,
                    limit,
                );
            }
            Statement::Connection(d) => {
                let header = format!("connection {} = {}", ident(&d.name), ident(&d.kind));
                let items: Vec<Item> = d.options.iter().map(Item::Opt).collect();
                self.braced(header.into(), &items, span, limit, false, false);
            }
            Statement::Runtime(d) => {
                let items: Vec<Item> = d.options.iter().map(Item::Opt).collect();
                self.braced("runtime".into(), &items, span, limit, false, false);
            }
            Statement::Source(d) => {
                let header = self.render(|| {
                    let args: Vec<String> = d.args.iter().map(|a| self.expr(a)).collect();
                    format!(
                        "source {} = {}({})",
                        ident(&d.name),
                        ident(&d.kind),
                        args.join(", ")
                    )
                });
                let mut items: Vec<Item> = d.options.iter().map(Item::Opt).collect();
                items.extend(d.schema.as_ref().map(Item::Schema));
                items.extend(d.identity.as_deref().map(Item::Identity));
                items.sort_by_key(|it| it.range().0);
                self.braced(header, &items, span, limit, false, true);
            }
            Statement::Mapping(d) => {
                let items = self.arm_items(&d.arms);
                self.braced(
                    format!("mapping {}", ident(&d.name)).into(),
                    &items,
                    span,
                    limit,
                    false,
                    false,
                );
            }
            Statement::Dataset(d) => self.dataset(d, limit),
            Statement::Validate(d) => {
                let items: Vec<Item> = d.checks.iter().map(Item::Check).collect();
                let header = format!("validate {}", rel_ref(&d.target));
                self.braced(header.into(), &items, span, limit, false, false);
            }
            Statement::Reconcile(d) => {
                let alias = |a: &Option<Ident>| {
                    a.as_ref()
                        .map_or(String::new(), |a| format!(" as {}", ident(a)))
                };
                let header = format!(
                    "reconcile {} = {}{} with {}{}",
                    ident(&d.name),
                    rel_ref(&d.a),
                    alias(&d.a_alias),
                    rel_ref(&d.b),
                    alias(&d.b_alias)
                );
                let items: Vec<Item> = d.items.iter().map(Item::Recon).collect();
                self.braced(header.into(), &items, span, limit, true, false);
            }
            Statement::Export(d) => match &d.target {
                ExportTarget::Single(rel) => {
                    let header = format!("export {} to {}", rel_ref(rel), str_lit(&d.path));
                    let items: Vec<Item> = d.options.iter().map(Item::Opt).collect();
                    self.braced(header.into(), &items, span, limit, false, true);
                }
                ExportTarget::Multi(parts) => {
                    let header = format!("export to {}", str_lit(&d.path));
                    let mut items: Vec<Item> = parts.iter().map(Item::Part).collect();
                    items.extend(d.options.iter().map(Item::Opt));
                    items.sort_by_key(|it| it.range().0);
                    self.braced(header.into(), &items, span, limit, false, false);
                }
            },
            Statement::Model(d) => {
                let items: Vec<Item> = d.items.iter().map(Item::Model).collect();
                self.braced(
                    format!("model {}", ident(&d.name)).into(),
                    &items,
                    span,
                    limit,
                    false,
                    false,
                );
            }
        }
    }

    fn dataset(&mut self, d: &DatasetDecl, limit: u32) {
        let steps: Vec<Item> = d.pipeline.steps.iter().map(Item::Step).collect();
        let header = format!("dataset {} = ", ident(&d.name));
        let head_limit = steps.first().map_or(limit, |s| s.range().0);
        match &d.pipeline.head {
            PipelineHead::Rel(r) => {
                let text = format!("{header}{}", rel_ref(r));
                if let [step] = d.pipeline.steps.as_slice() {
                    // A one-step pipeline written on one line stays there if it still fits.
                    let one_line = !self.src.as_bytes()[d.span.range()].contains(&b'\n')
                        && !self.has_pending(d.span.start, d.span.end);
                    if one_line {
                        let line = self.render(|| {
                            self.step_inline(&step.kind)
                                .map_or(String::new(), |s| format!("{text} {s}"))
                        });
                        if !line.text.is_empty() && self.fits(&line.text) {
                            return self.atomic(line, d.span.start, d.span.end, limit);
                        }
                    }
                }
                if steps.is_empty() {
                    self.atomic(text.into(), d.span.start, d.span.end, limit);
                } else {
                    self.push(&text);
                    self.set_prev_end(r.span.end);
                }
            }
            PipelineHead::NativeSql {
                options,
                schema,
                span,
            } => {
                let mut items: Vec<Item> = options.iter().map(Item::Opt).collect();
                items.extend(schema.as_ref().map(Item::Schema));
                items.sort_by_key(|it| it.range().0);
                let head_span = Span {
                    start: d.span.start,
                    ..*span
                };
                let head_limit = self.line_end(span.end).min(head_limit);
                let header = format!("{header}native_sql").into();
                self.braced(header, &items, head_span, head_limit, false, false);
            }
        }
        if !steps.is_empty() {
            self.indent += INDENT;
            self.items(&steps, limit, false);
            self.indent -= INDENT;
        }
    }

    fn step(&mut self, step: &Step, limit: u32) {
        let span = step.span;
        let text = self.render(|| self.step_inline(&step.kind).unwrap_or_default());
        // Steps that also have a block form use it unless they fit on one line and no comment
        // needs the block's lines.
        let has_block = matches!(
            step.kind,
            StepKind::Derive(_)
                | StepKind::Aggregate(_)
                | StepKind::Select(_)
                | StepKind::Rename(_)
                | StepKind::Normalize {
                    mapping: MappingRef::Inline(_),
                    ..
                }
        );
        if !text.text.is_empty()
            && (!has_block || (!self.has_pending(span.start, span.end) && self.fits(&text.text)))
        {
            return self.atomic(text, span.start, span.end, limit);
        }
        let (header, items): (String, Vec<Item>) = match &step.kind {
            StepKind::Derive(a) => ("|> derive".into(), a.iter().map(Item::Assign).collect()),
            StepKind::Aggregate(a) => ("|> aggregate".into(), a.iter().map(Item::Assign).collect()),
            StepKind::Select(s) => ("|> select".into(), s.iter().map(Item::Select).collect()),
            StepKind::Rename(r) => ("|> rename".into(), r.iter().map(Item::Rename).collect()),
            StepKind::Normalize {
                column,
                mapping: MappingRef::Inline(arms),
            } => (
                format!("|> normalize {}", column_name(column)),
                self.arm_items(arms),
            ),
            _ => unreachable!("steps without a block form are always inline"),
        };
        self.braced(header.into(), &items, span, limit, false, false);
    }

    /// `|> step` on one line, for steps that have a one-line form: every step except
    /// `aggregate`, inline `normalize` mappings, multi-assignment `derive`, `select` mixing
    /// several items with assignments, and empty `{}` forms.
    fn step_inline(&self, kind: &StepKind) -> Option<String> {
        let body = match kind {
            StepKind::Derive(a) if a.len() == 1 => format!("derive {}", self.assign(&a[0])),
            StepKind::Derive(_)
            | StepKind::Aggregate(_)
            | StepKind::Normalize {
                mapping: MappingRef::Inline(_),
                ..
            } => return None,
            StepKind::Select(items)
                if !items.is_empty()
                    && (items.len() == 1
                        || items.iter().all(|i| matches!(i, SelectItem::Column(_)))) =>
            {
                format!("select {}", join(items.iter().map(|i| self.select_item(i))))
            }
            StepKind::Select(_) => return None,
            StepKind::Rename(r) if !r.is_empty() => {
                format!("rename {}", join(r.iter().map(rename)))
            }
            StepKind::Rename(_) => return None,
            StepKind::Drop(cols) => format!("drop {}", join(cols.iter().map(column_name))),
            StepKind::Filter(e) => format!("filter {}", self.expr(e)),
            StepKind::Join {
                kind,
                rel,
                alias,
                on,
            } => {
                let kw = match kind {
                    JoinKind::Inner => "join",
                    JoinKind::Left => "left join",
                    JoinKind::Right => "right join",
                    JoinKind::Full => "full join",
                };
                let alias = alias
                    .as_ref()
                    .map_or(String::new(), |a| format!(" as {}", ident(a)));
                let on = match on {
                    JoinOn::Keys(keys) => ident_list(keys),
                    JoinOn::Expr(e) => self.expr(e),
                };
                format!("{kw} {}{alias} on {on}", rel_ref(rel))
            }
            StepKind::Group(keys) => {
                format!("group by {}", join(keys.iter().map(|k| self.expr(k))))
            }
            StepKind::Sort(keys) => {
                let keys = self.sort_keys(keys);
                // `sort by` is optional; spell it out when the first key is a column named `by`.
                if first_word(&keys) == "by" {
                    format!("sort by {keys}")
                } else {
                    format!("sort {keys}")
                }
            }
            StepKind::Distinct => "distinct".into(),
            StepKind::Union(rel) => format!("union {}", rel_ref(rel)),
            StepKind::Limit(n) => format!("limit {n}"),
            StepKind::Normalize {
                column,
                mapping: MappingRef::Named(name),
            } => {
                format!("normalize {} with {}", column_name(column), ident(name))
            }
        };
        Some(format!("|> {body}"))
    }

    // ---- clause pieces ------------------------------------------------------------------------

    fn assign(&self, a: &Assign) -> String {
        format!("{} = {}", ident(&a.name), self.expr(&a.expr))
    }

    fn select_item(&self, item: &SelectItem) -> String {
        match item {
            SelectItem::Column(c) => column_name(c),
            SelectItem::Assign(a) => self.assign(a),
        }
    }

    fn sort_keys(&self, keys: &[SortKey]) -> String {
        join(keys.iter().map(|k| {
            let e = self.expr(&k.expr);
            if k.desc { format!("{e} desc") } else { e }
        }))
    }

    fn block_spec(&self, spec: &BlockSpec) -> String {
        match spec {
            BlockSpec::None(_) => "none".into(),
            BlockSpec::Keys(keys) => join(keys.iter().map(|k| match k {
                BlockKey::Same(i) => ident(i),
                BlockKey::Pair(a, b) => format!("{} == {}", self.expr(a), self.expr(b)),
            })),
        }
    }

    fn flag(&self, f: &Flag) -> String {
        match &f.when {
            Some(e) => format!("flag {} when {}", ident(&f.name), self.expr(e)),
            None => format!("flag {}", ident(&f.name)),
        }
    }

    fn check(&self, c: &Check) -> String {
        let body = match &c.kind {
            CheckKind::Predicate(e) => self.expr(e),
            CheckKind::NotNull(e) => format!("{} not null", self.expr(e)),
            CheckKind::Unique(cols) => format!("unique({})", join(cols.iter().map(column_name))),
        };
        match &c.label {
            Some(l) => format!("{} {body} as {}", c.severity.keyword(), str_lit(l)),
            None => format!("{} {body}", c.severity.keyword()),
        }
    }

    fn pattern(&self, p: &MapPattern) -> String {
        match p {
            MapPattern::Otherwise => "otherwise".into(),
            MapPattern::Values(values) => {
                // A single value is written bare only where the parser reads it back as one
                // (bare patterns are primaries; bare `otherwise` is the fallback arm).
                if let [v] = values.as_slice() {
                    let text = self.expr(v);
                    let primary = !matches!(
                        v.kind,
                        ExprKind::List(_)
                            | ExprKind::Unary { .. }
                            | ExprKind::Binary { .. }
                            | ExprKind::IsNull { .. }
                            | ExprKind::InList { .. }
                    );
                    if primary && !text.starts_with('-') && first_word(&text) != "otherwise" {
                        return text;
                    }
                }
                format!("[{}]", join(values.iter().map(|v| self.expr(v))))
            }
        }
    }

    // ---- expressions --------------------------------------------------------------------------

    fn expr(&self, e: &Expr) -> String {
        self.expr_at(e, self.indent)
    }

    /// `indent` is the indentation of the line the expression starts on (used by multi-line
    /// `case`).
    fn expr_at(&self, e: &Expr, indent: usize) -> String {
        let list = |items: &[Expr]| join(items.iter().map(|x| self.expr_at(x, indent)));
        match &e.kind {
            ExprKind::Literal(l) => literal(l),
            ExprKind::Column(c) => column_name(c),
            ExprKind::Call {
                namespace,
                name,
                args,
            } => match namespace {
                Some(ns) => format!("{}.{}({})", ident(ns), ident(name), list(args)),
                None => format!("{}({})", ident(name), list(args)),
            },
            ExprKind::Unary {
                op: UnaryOp::Not,
                expr,
            } => format!("not {}", self.expr_at(expr, indent)),
            ExprKind::Unary {
                op: UnaryOp::Neg,
                expr,
            } => format!("-{}", self.expr_at(expr, indent)),
            ExprKind::Binary { op, left, right } => {
                format!(
                    "{} {} {}",
                    self.expr_at(left, indent),
                    op.symbol(),
                    self.expr_at(right, indent)
                )
            }
            ExprKind::IsNull { expr, negated } => {
                let not = if *negated { "not " } else { "" };
                format!("{} is {not}null", self.expr_at(expr, indent))
            }
            ExprKind::InList {
                expr,
                list: items,
                negated,
            } => {
                let not = if *negated { "not " } else { "" };
                format!("{} {not}in [{}]", self.expr_at(expr, indent), list(items))
            }
            ExprKind::Case { arms, otherwise } => {
                self.case(e.span, arms, otherwise.as_deref(), indent)
            }
            ExprKind::List(items) => format!("[{}]", list(items)),
            ExprKind::Paren(inner) => format!("({})", self.expr_at(inner, indent)),
        }
    }

    /// `case { ... }`: inline when short and comment-free, otherwise one arm per line with the
    /// comments written inside it kept before / after their arm.
    fn case(
        &self,
        span: Span,
        arms: &[CaseArm],
        otherwise: Option<&Expr>,
        indent: usize,
    ) -> String {
        let inner = indent + INDENT;
        // (start, end, text) of each arm, `otherwise` last.
        let mut parts: Vec<(u32, u32, String)> = arms
            .iter()
            .map(|a| {
                let text = format!(
                    "{} => {}",
                    self.expr_at(&a.when, inner),
                    self.expr_at(&a.then, inner)
                );
                (a.when.span.start, a.then.span.end, text)
            })
            .collect();
        if let Some(o) = otherwise {
            parts.push((
                o.span.start,
                o.span.end,
                format!("otherwise => {}", self.expr_at(o, inner)),
            ));
        }
        // Comments not yet placed (by an enclosing emission or by a nested `case`).
        let comments: Vec<usize> = {
            let embedded = self.embedded.borrow();
            self.pending_iter(span.start, span.end)
                .filter(|i| !embedded.contains(i))
                .collect()
        };
        let inline = format!("case {{ {} }}", join(parts.iter().map(|p| p.2.clone())));
        if comments.is_empty()
            && !inline.contains('\n')
            && inline.chars().count() <= CASE_INLINE_MAX
        {
            return inline;
        }
        // Each arm claims, in source order, the comments before it and those up to the end of
        // its last line; the rest go before the closing `}`.
        let mut before = vec![Vec::new(); parts.len()];
        let mut inside = vec![Vec::new(); parts.len()];
        let mut by_pos: Vec<usize> = (0..parts.len()).collect();
        by_pos.sort_by_key(|&p| parts[p].0);
        let mut lo = span.start;
        for (k, &p) in by_pos.iter().enumerate() {
            let (start, end, _) = parts[p];
            let next = by_pos.get(k + 1).map_or(span.end, |&q| parts[q].0);
            let limit = self.line_end(end).min(next);
            for &c in &comments {
                let at = self.comments[c].span.start;
                if (lo..start).contains(&at) {
                    before[p].push(c);
                } else if (start..limit).contains(&at) {
                    inside[p].push(c);
                }
            }
            lo = limit;
        }
        let close: Vec<usize> = comments
            .iter()
            .copied()
            .filter(|&c| self.comments[c].span.start >= lo)
            .collect();

        let pad = " ".repeat(inner);
        let mut lines: Vec<Line> = vec![Line {
            text: "case {".into(),
            has_comment: false,
        }];
        // A comment that followed code goes to the end of the previous line when it can.
        let place = |lines: &mut Vec<Line>, c: usize| {
            let comment = &self.comments[c];
            match lines.last_mut() {
                Some(last) if comment.trailing && !last.has_comment => {
                    last.text.push_str("  ");
                    last.text.push_str(&comment.text);
                    last.has_comment = true;
                }
                _ => lines.push(Line {
                    text: format!("{pad}{}", comment.text),
                    has_comment: true,
                }),
            }
        };
        for (p, (_, _, text)) in parts.iter().enumerate() {
            for &c in &before[p] {
                place(&mut lines, c);
            }
            // Comments inside a multi-line arm move above it, except a final trailing one.
            let mut ins = inside[p].clone();
            let tail = match ins.last() {
                Some(&c) if self.comments[c].trailing => ins.pop(),
                _ => None,
            };
            for c in ins {
                lines.push(Line {
                    text: format!("{pad}{}", self.comments[c].text),
                    has_comment: true,
                });
            }
            let mut line = format!("{pad}{text}");
            if parts.get(p + 1).is_some_and(|n| continues_expr(&n.2)) {
                line.push(',');
            }
            lines.push(Line {
                text: line,
                has_comment: false,
            });
            if let Some(c) = tail {
                place(&mut lines, c);
            }
        }
        for &c in &close {
            place(&mut lines, c);
        }
        lines.push(Line {
            text: format!("{}}}", " ".repeat(indent)),
            has_comment: false,
        });
        self.embedded.borrow_mut().extend(comments);
        lines
            .into_iter()
            .map(|l| l.text)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

// ---- leaf printing ------------------------------------------------------------------------------

fn join(items: impl Iterator<Item = String>) -> String {
    items.collect::<Vec<_>>().join(", ")
}

fn ident(i: &Ident) -> String {
    if i.quoted {
        format!("`{}`", i.name)
    } else {
        i.name.clone()
    }
}

fn ident_list(ids: &[Ident]) -> String {
    join(ids.iter().map(ident))
}

/// `x` or `[x, y]`.
fn ident_or_list(ids: &[Ident]) -> String {
    match ids {
        [one] => ident(one),
        _ => format!("[{}]", ident_list(ids)),
    }
}

fn column_name(c: &ColumnName) -> String {
    match &c.qualifier {
        Some(q) => format!("{}.{}", ident(q), ident(&c.name)),
        None => ident(&c.name),
    }
}

fn rel_ref(r: &RelRef) -> String {
    match &r.part {
        Some(p) => format!("{}.{}", ident(&r.name), ident(p)),
        None => ident(&r.name),
    }
}

fn rename(r: &Rename) -> String {
    format!("{} -> {}", column_name(&r.from), ident(&r.to))
}

fn type_expr(t: &TypeExpr) -> String {
    let mut out = ident(&t.name);
    if !t.params.is_empty() {
        let params = join(t.params.iter().map(|p| match p {
            TypeParam::Int(n) => n.to_string(),
            TypeParam::Str(s) => quote(s),
        }));
        out.push_str(&format!("({params})"));
    }
    if t.nullable {
        out.push('?');
    }
    out
}

fn literal(l: &Literal) -> String {
    match l {
        Literal::Int(n) => n.to_string(),
        Literal::Decimal(d) => d.clone(),
        Literal::Str(s) => {
            // The AST does not record `"""`; multi-line text reads best as a block string.
            if s.contains('\n') && !s.contains("\"\"\"") && !s.ends_with('"') {
                format!("\"\"\"{s}\"\"\"")
            } else {
                quote(s)
            }
        }
        Literal::Bool(b) => b.to_string(),
        Literal::Null => "null".into(),
    }
}

fn str_lit(s: &StrLit) -> String {
    if s.block {
        format!("\"\"\"{}\"\"\"", s.value)
    } else {
        quote(&s.value)
    }
}

/// A `"..."` literal with the lexer's escapes. A backslash that does not start an escape is
/// written as-is (`"\d+"` stays readable); it is doubled only where it would otherwise be read
/// as an escape or would escape the closing quote.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => match chars.peek() {
                None | Some('n' | 't' | '\\' | '"') => out.push_str("\\\\"),
                Some(_) => out.push('\\'),
            },
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn first_word(text: &str) -> &str {
    let end = text
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(text.len());
    &text[..end]
}

/// Whether an item starting with `text` could be read as the continuation of an expression
/// ending the previous item, so the two need an explicit `,` between them.
fn continues_expr(text: &str) -> bool {
    if text.starts_with('-') || text.starts_with('(') {
        return true;
    }
    match first_word(text) {
        "and" | "or" | "is" | "in" => true,
        "not" => first_word(text[3..].trim_start()) == "in",
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::token::lex;

    fn fmt(src: &str) -> String {
        match format_source(0, src) {
            Ok(s) => s,
            Err(d) => panic!("parse failed: {d:?}\n{src}"),
        }
    }

    /// Debug dump of the parsed program with every span erased.
    fn ast_shape(src: &str) -> String {
        let parsed = parse(0, src);
        assert!(
            parsed.diagnostics.is_empty(),
            "{:?}\n{src}",
            parsed.diagnostics
        );
        let dump = format!("{:#?}", parsed.program);
        let mut out = String::new();
        let mut rest = dump.as_str();
        while let Some(i) = rest.find("Span {") {
            out.push_str(&rest[..i]);
            let close = rest[i..].find('}').expect("span dump closes");
            out.push_str("Span");
            rest = &rest[i + close + 1..];
        }
        out.push_str(rest);
        out
    }

    fn comments(src: &str) -> Vec<(String, bool)> {
        lex(0, src)
            .comments
            .into_iter()
            .map(|c| (c.text, c.trailing))
            .collect()
    }

    /// Formatting is idempotent and preserves the program and its comments.
    fn assert_round_trip(src: &str) -> String {
        let once = fmt(src);
        let twice = fmt(&once);
        assert_eq!(once, twice, "not idempotent; first pass:\n{once}");
        assert_eq!(
            ast_shape(src),
            ast_shape(&once),
            "AST changed; formatted:\n{once}"
        );
        let before: Vec<String> = comments(src).into_iter().map(|c| c.0).collect();
        let after: Vec<String> = comments(&once).into_iter().map(|c| c.0).collect();
        assert_eq!(before, after, "comments changed; formatted:\n{once}");
        once
    }

    const RICH: &str = r#"
import "common.magi"
import "lookups.magi"

connection warehouse = odbc {
    dsn: "WH"
    timeout: 30
}

connection empty = odbc {}

runtime {
    threads: 4
    memory_limit: "2GB"
}

source cases = excel("cases.xlsx") {
    sheet: "FY26"
    header_row: 2

    schema {
        id: int
        `case type`: string?
        amount: decimal(18, 2)?
        opened: date("%m/%d/%Y")
        note: string("line\nbreak \"q\"")
    }
    identity id
}

source offices = sql("warehouse") {
    query: """
        SELECT office_id, state, region
        FROM dim_office
    """
    identity [office_id, state]
}

source plain = csv("plain.csv")
source no_args = duckdb()
source empty_schema = parquet("x.parquet") { schema {} }

mapping categories {
    "Firearm" => "Firearms"
    ["Gun", "Rifle"] => "Firearms"
    [-1] => "negative"
    [] => "nothing"
    [[1, 2]] => "list"
    `otherwise` => "quoted"
    upper(x) => "call"
    otherwise => original
}

dataset cleaned = cases
    |> select id, `case type`, amount, opened, category
    |> drop opened, cases.note
    |> rename `case type` -> case_type, amount -> amt
    |> derive y = lower(x)
    |> derive {
        a = 1 + 2 * 3
        b = (1 + 2) * 3
        c = -amt
        d = -5
        e = -0.5
        f = not flag_a and flag_b
        g = not a == b
        h = x is null
        i = x is not null
        j = x in [1, 2, 3]
        k = x not in ["a", "b"]
        l = case { x > 0 => "pos", otherwise => "neg" }
        m = duckdb.jaro_winkler(a, b)
        n = coalesce(x, 0.02, null, true, false)
        o = "quote \" backslash \\ tab \t"
        p = case {
            amount > 1000000 and category == "Firearms" => "large firearm case",
            -amount > 1000 => "negative medium"
            otherwise => "small ordinary case"
        }
        q = a - -5
        r = format_list(["a", "b"])
        s = a.b % 2 / 3 - 1
        t = x <= 1 or y >= 2 or z != 3 or w < 4
        u = (a == b) == c
        v = "multi\nline"
        w = --x
        type = group
    }
    |> derive { x = a, and = 1, z = -2 }
    |> filter amount > 0
    |> join offices on office_id
    |> inner join lookups as l on l.code == category and l.active
    |> left join more on a, b
    |> right join other as o on o.id == id
    |> full join other on o.id == id
    |> group by category, lower(region)
    |> aggregate total = sum(amt), n = count()
    |> sort by total desc, category asc
    |> sort by by desc, `by`
    |> distinct
    |> union other_rel.matches
    |> limit 100
    |> normalize category {
        "Firearm" => "Firearms"
        otherwise => original
    }
    |> normalize category {}
    |> normalize category with categories
    |> select id, total = amt * 2
    |> select only = 1
    |> select {}
    |> rename { a -> b }
    |> derive {}

dataset raw_sql = native_sql {
    dialect: duckdb
    query: """SELECT 1 AS x"""
    schema { x: int }
}
    |> filter x > 0

dataset alias = cleaned
dataset parts = result.matches |> limit 5

validate cleaned {
    require id not null
    require amount >= 0 as "non-negative amounts"

    expect unique(id, event_date)

    warn missing(description) < 0.05
}

validate empty {}

reconcile result = source_a as a with source_b as b {
    block by organization_id, a.x == b.y
    cardinality one_to_one
    consume both
    identity a: a_ref
    identity b: [x, y]
    ambiguity hold
    duplicates continue
    evidence { amount_diff = abs(a.amount - b.amount) }
    flag big_diff when amount_diff > 100
    flag always
    tier exact {
        require date_a == date_b
        rank by similarity(a.d, b.d) desc, a.id
        block by none
        many a to one b
        group a by k1, k2
        compare { a.amount == b.amount; -a.x > 0; (a.y) }
        evidence { score = 1 }
        flag exact_match when true
        flag tier_flag
    }
    tier fuzzy {
        one a to many b
    }
    tier more { many a to many b one a to one b }
    tier nothing {}
    consume a
}

reconcile bare = x with y {}

export result.matches to "matches.xlsx"
export summary to "summary.parquet" { compression: "zstd" }
export to "report.xlsx" {
    sheet "Matches" = result.matches
    overwrite: true
    sheet "Unmatched A" = result.unmatched_a
}
export to "db.duckdb" {
    table "t" = summary
}
export to """block.csv""" {}
"#;

    const SAMPLES: &[&str] = &[
        // named source arguments are written as options
        r#"
source cases = excel("cases.xlsx") { sheet: "FY26" }

source offices = sql("warehouse") {
    query: """
        SELECT office_id, state, region
        FROM dim_office
    """
}

dataset cases_with_region = cases
    |> join offices on office_id
    |> derive age_days = days_between(event_date, today())
"#,
        // normalize
        r#"
dataset cleaned = raw
    |> normalize category {
        "Firearm"  => "Firearms"
        "Gun"      => "Firearms"
        "Narcotic" => "Drugs"
        otherwise  => original
    }
"#,
        // validate
        r#"
validate cleaned {
    require id not null
    require amount >= 0

    expect unique(id, event_date)

    warn missing(description) < 0.05
}
"#,
        // reconcile
        r#"
reconcile result = source_a with source_b {
    block by organization_id

    tier exact {
        require date_a == date_b
        require amount_a == amount_b
        require type_a == type_b
    }

    tier strong {
        require days_between(date_a, date_b) <= 2
        require amount_a == amount_b

        rank by similarity(description_a, description_b) desc
    }

    tier likely {
        require type_a == type_b
        require similarity(description_a, description_b) >= 0.90
    }

    consume both
}
"#,
        // exports
        r#"
export result.matches to "matches.xlsx"
export result.unmatched_a to "unmatched_a.xlsx"
export result.unmatched_b to "unmatched_b.xlsx"
export summary to "summary.parquet"
"#,
        // join, group and aggregate
        r#"
source a = excel("a.xlsx") {
    sheet: "Data"
}

source b = excel("b.xlsx") {
    sheet: "Data"
}

dataset clean_a = a
    |> derive category = lower(trim(category))
    |> filter amount > 0

dataset clean_b = b
    |> derive category = lower(trim(category))
    |> filter amount > 0

dataset summary = clean_a
    |> join clean_b on organization_id
    |> group by category
    |> aggregate {
        a_total = sum(clean_a.amount)
        b_total = sum(clean_b.amount)
    }

export summary to "summary.xlsx"
"#,
    ];

    #[test]
    fn rich_sample_round_trips() {
        let out = assert_round_trip(RICH);
        for line in [
            "    |> derive y = lower(x)",
            "        l = case { x > 0 => \"pos\", otherwise => \"neg\" }",
            "        p = case {",
            "            amount > 1000000 and category == \"Firearms\" => \"large firearm case\",",
            "            -amount > 1000 => \"negative medium\"",
            "        v = \"\"\"multi",
            "        x = a,",
            "        and = 1",
            "    |> join lookups as l on l.code == category and l.active",
            "    |> sort total desc, category",
            "    |> sort by by desc, `by`",
            "    |> normalize category {}",
            "    |> select {\n        id\n        total = amt * 2\n    }",
            "    |> select only = 1",
            "        a.amount == b.amount",
            "        -a.x > 0,",
            "    evidence {",
            "    [-1]             => \"negative\"",
            "    `otherwise`      => \"quoted\"",
            "source plain = csv(\"plain.csv\")",
            "connection empty = odbc {}",
            "        note: string(\"line\\nbreak \\\"q\\\"\")",
            "    sheet \"Unmatched A\" = result.unmatched_a",
            "    table \"t\" = summary",
            "export to \"\"\"block.csv\"\"\" {}",
            "dataset raw_sql = native_sql {",
            "}\n    |> filter x > 0",
            "    tier more {\n        many a to many b\n        one a to one b\n    }",
        ] {
            assert!(out.contains(line), "missing {line:?} in:\n{out}");
        }
    }

    #[test]
    fn sample_programs_round_trip() {
        for src in SAMPLES {
            assert_round_trip(src);
        }
        // Already canonical (modulo the leading newline); the first is not (inline source block).
        for src in &SAMPLES[1..] {
            assert_eq!(fmt(src), src.trim_start());
        }
    }

    #[test]
    fn messy_input_formats_to_canonical_text() {
        let src = r#"import "b.magi"
source a=csv("a.csv"){delim:",";schema{id:int  amount:decimal(18,2)?}identity id}
mapping m {"Firearm"=>"Firearms"  ["Gun","Rifle"] => "Firearms" otherwise=>original}
dataset x=a|>filter amount>0|>derive y=lower(x),z=1|>aggregate total=sum(amount)|>sort total asc,id desc
validate x{require id not null;expect unique(id)}
reconcile r=a as p with b as q{block by id tier t1{require p.v==q.v rank by p.d desc} tier t2{block by none} consume both}
import "a.magi"
export x to "x.csv"
export to "r.xlsx" {sheet "S" = x}"#;
        let expected = r#"import "b.magi"
import "a.magi"

source a = csv("a.csv") {
    delim: ","
    schema {
        id: int
        amount: decimal(18, 2)?
    }
    identity id
}

mapping m {
    "Firearm"        => "Firearms"
    ["Gun", "Rifle"] => "Firearms"
    otherwise        => original
}

dataset x = a
    |> filter amount > 0
    |> derive {
        y = lower(x)
        z = 1
    }
    |> aggregate {
        total = sum(amount)
    }
    |> sort total, id desc

validate x {
    require id not null
    expect unique(id)
}

reconcile r = a as p with b as q {
    block by id

    tier t1 {
        require p.v == q.v
        rank by p.d desc
    }

    tier t2 {
        block by none
    }

    consume both
}

export x to "x.csv"

export to "r.xlsx" {
    sheet "S" = x
}
"#;
        assert_eq!(fmt(src), expected);
        assert_eq!(fmt(expected), expected);
    }

    #[test]
    fn comments_stay_attached() {
        let src = r#"# header

# leading for the import
import "a.magi"  # import trailing
# between imports
import "b.magi"
# leading for source
source a = csv("a.csv") {  # after brace
    # leading option
    delim: ","  # trailing option

    # end of block
}
source empty = csv("e.csv") {
    # only a comment
}
dataset x = a  # head trailing
    # before filter
    |> filter amount > 0  # keep positive
      and amount < 10  # still positive
    |> derive {
        # inside derive
        y = 1  # y trailing
    }  # after derive block
    |> filter a
       # inside an expression
       or b
reconcile r = a with b {
    tier t {  # tier trailing
        require a.x == b.x
        # end of tier
    }
    # end of reconcile
}
# eof comment

# eof after blank
"#;
        let expected = r#"# header

# leading for the import
import "a.magi"  # import trailing
# between imports
import "b.magi"

# leading for source
source a = csv("a.csv") {  # after brace
    # leading option
    delim: ","  # trailing option

    # end of block
}

source empty = csv("e.csv") {
    # only a comment
}

dataset x = a  # head trailing
    # before filter
    # keep positive
    |> filter amount > 0 and amount < 10  # still positive
    |> derive {
        # inside derive
        y = 1  # y trailing
    }  # after derive block
    # inside an expression
    |> filter a or b

reconcile r = a with b {
    tier t {  # tier trailing
        require a.x == b.x
        # end of tier
    }
    # end of reconcile
}
# eof comment

# eof after blank
"#;
        let out = fmt(src);
        assert_eq!(out, expected);
        assert_eq!(fmt(&out), out);
        assert_eq!(ast_shape(src), ast_shape(&out));
    }

    #[test]
    fn imports_hoist_with_their_comments_below_file_header() {
        let src = "# file header\n\n# about s\nsource s = csv(\"s.csv\")  # s trailing\n# about b\nimport \"b.magi\"  # b trailing\nimport \"c.magi\"\n";
        let expected = "# file header\n\n# about b\nimport \"b.magi\"  # b trailing\nimport \"c.magi\"\n\n# about s\nsource s = csv(\"s.csv\")  # s trailing\n";
        let out = fmt(src);
        assert_eq!(out, expected);
        assert_eq!(fmt(&out), out);
    }

    #[test]
    fn comments_stay_on_their_case_arms() {
        let src = r#"dataset parsed = typed
    |> derive {
        text = lower(memo)
        bucket = case {   # after brace
            # before first arm
            code == "special order" => "rush"   # business rule: always rush
            code == "HOLD" => "other"                # business rule: HOLD is not really rush

            # before otherwise
            otherwise => map(lower(trim(kind)), buckets)
            # before close
        }   # after case
        n = case {
            a => case {
                b => 1  # inner
                otherwise => 2
            }  # outer arm
            otherwise => 3
        }
    }
    |> filter case { x > 0 => true  # positive
        otherwise => false }
"#;
        let expected = r#"dataset parsed = typed
    |> derive {
        text = lower(memo)
        bucket = case {  # after brace
            # before first arm
            code == "special order" => "rush"  # business rule: always rush
            code == "HOLD" => "other"  # business rule: HOLD is not really rush
            # before otherwise
            otherwise => map(lower(trim(kind)), buckets)
            # before close
        }  # after case
        n = case {
            a => case {
                b => 1  # inner
                otherwise => 2
            }  # outer arm
            otherwise => 3
        }
    }
    |> filter case {
        x > 0 => true  # positive
        otherwise => false
    }
"#;
        let out = fmt(src);
        assert_eq!(out, expected);
        assert_eq!(fmt(&out), out);
        assert_eq!(ast_shape(src), ast_shape(&out));
        assert_eq!(comments(src).len(), comments(&out).len());
    }

    #[test]
    fn adjacent_one_line_statements_of_a_kind_stay_together() {
        let src = r#"import "a.magi"
source s = csv("s.csv")
source t = csv("t.csv")
dataset a = s |> filter x > 0
dataset b = s |> filter x <= 0
dataset c = s
    |> filter y
dataset d = s |> filter a_very_long_predicate_name_number_one > 1 and a_very_long_predicate_name_number_two < 2
export a to "a.csv"
export b to "b.csv"
# c goes elsewhere
export c to "c.csv"

export d to "d.csv"
export to "all.xlsx" { sheet "A" = a }
export s to "s.csv"
"#;
        let expected = r#"import "a.magi"

source s = csv("s.csv")
source t = csv("t.csv")

dataset a = s |> filter x > 0
dataset b = s |> filter x <= 0

dataset c = s
    |> filter y

dataset d = s
    |> filter a_very_long_predicate_name_number_one > 1 and a_very_long_predicate_name_number_two < 2

export a to "a.csv"
export b to "b.csv"
# c goes elsewhere
export c to "c.csv"

export d to "d.csv"

export to "all.xlsx" {
    sheet "A" = a
}

export s to "s.csv"
"#;
        let out = fmt(src);
        assert_eq!(out, expected);
        assert_eq!(fmt(&out), out);
        assert_eq!(ast_shape(src), ast_shape(&out));
    }

    #[test]
    fn single_item_evidence_and_compare_stay_inline_when_they_fit() {
        let src = r#"reconcile r = a with b {
    evidence { a_total = sum(a.kg) }
    tier t {
        compare { a.kg == b.kg }
        compare { a.x == b.x, a.y == b.y }
        evidence { score = similarity(a.description_with_a_long_name, b.description_with_a_long_name) }
        evidence {
            s = 1  # why
        }
        evidence {}
    }
}
"#;
        let expected = r#"reconcile r = a with b {
    evidence { a_total = sum(a.kg) }

    tier t {
        compare { a.kg == b.kg }
        compare {
            a.x == b.x
            a.y == b.y
        }
        evidence {
            score = similarity(a.description_with_a_long_name, b.description_with_a_long_name)
        }
        evidence {
            s = 1  # why
        }
        evidence {}
    }
}
"#;
        let out = fmt(src);
        assert_eq!(out, expected);
        assert_eq!(fmt(&out), out);
        assert_eq!(ast_shape(src), ast_shape(&out));
    }

    #[test]
    fn subset_clause_keeps_its_limits() {
        let src = "reconcile r = a with b {\n    tier payouts {\n        subset  b max_items 8   max_subsets 50000\n        require sum(b.amount) == a.amount\n    }\n    tier deposits { group b by entry\n subset a max_items 2 }\n}\n";
        let expected = "reconcile r = a with b {\n    tier payouts {\n        subset b max_items 8 max_subsets 50000\n        require sum(b.amount) == a.amount\n    }\n\n    tier deposits {\n        group b by entry\n        subset a max_items 2\n    }\n}\n";
        assert_eq!(assert_round_trip(src), expected);
    }

    #[test]
    fn long_mapping_patterns_do_not_push_other_arms_right() {
        let long = r#"["FIRST LONG PATTERN", "SECOND LONG PATTERN", "THIRD LONG PATTERN"]"#;
        let src = format!(
            "mapping fees {{\n{long} => \"service\"\n\"MISC\" => \"other\"\n[\"A\", \"BB\"] => \"x\"\notherwise => original\n}}\n"
        );
        // short patterns still align with each other; the long one keeps a single space
        let expected = format!(
            "mapping fees {{\n    {long} => \"service\"\n    \"MISC\"      => \"other\"\n    [\"A\", \"BB\"] => \"x\"\n    otherwise   => original\n}}\n"
        );
        assert_eq!(fmt(&src), expected);
        assert_round_trip(&expected);
    }

    #[test]
    fn parse_errors_are_returned_not_formatted() {
        let err = format_source(0, "dataset x = a |> fliter y\n").unwrap_err();
        assert_eq!(err.len(), 1);
        assert!(err[0].is_error());
        assert!(format_source(0, "source a = csv(\"unterminated").is_err());
    }

    #[test]
    fn empty_and_comment_only_files() {
        assert_eq!(fmt(""), "");
        assert_eq!(fmt("\n\n"), "");
        assert_eq!(
            fmt("  # just a note\n\n\n# another\n"),
            "# just a note\n\n# another\n"
        );
    }
}
