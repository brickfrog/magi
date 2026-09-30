//! Resolution of `model` declarations (BI semantic models).

use crate::ast::{self, ExprKind, Literal, ModelItem};
use crate::diagnostic::Diagnostic;
use crate::export::tmdl;
use crate::semantic::hir::*;
use crate::semantic::types::Type;
use crate::syntax::span::Span;

use super::Analyzer;
use super::expr::{AggMode, SCol, Scope};

/// A type-checked metric. It is translated to DAX once every `dimension` has named its columns.
struct PendingMetric<'a> {
    name: &'a ast::Ident,
    value: TExpr,
    value_span: Span,
    home: usize,
    format: String,
    description: Option<String>,
}

/// Power BI object names ignore case.
fn same_name(a: &str, b: &str) -> bool {
    a.chars()
        .flat_map(char::to_lowercase)
        .eq(b.chars().flat_map(char::to_lowercase))
}

impl<'a> Analyzer<'a> {
    pub(super) fn model(&mut self, d: &'a ast::ModelDecl) {
        // tables, in order of first mention
        let mut names: Vec<(String, crate::syntax::span::Span)> = Vec::new();
        let mut mention = |n: &ast::Ident| {
            if !names.iter().any(|(x, _)| x == &n.name) {
                names.push((n.name.clone(), n.span));
            }
        };
        for item in &d.items {
            match item {
                ModelItem::Table { rel, .. } => mention(rel),
                ModelItem::Relationship { from, to, .. } => {
                    for c in [from, to] {
                        if let Some(q) = &c.qualifier {
                            mention(q);
                        }
                    }
                }
                ModelItem::Dimension { column, .. } => {
                    if let Some(q) = &column.qualifier {
                        mention(q);
                    }
                }
                ModelItem::Metric { options, .. } => {
                    for o in options.iter().filter(|o| o.key.name == "value") {
                        o.value.walk(&mut |e| {
                            if let ExprKind::Column(c) = &e.kind
                                && let Some(q) = &c.qualifier
                            {
                                mention(q);
                            }
                        });
                    }
                }
            }
        }
        let mut ok = true;
        let mut tables: Vec<ModelTable> = Vec::new();
        let mut scope = Scope::default();
        for (slot, (name, span)) in names.iter().enumerate() {
            let Some(rel) = self.ensure(name, Some(*span)) else {
                ok = false;
                continue;
            };
            let rel = self.relation(rel).clone();
            let export = self
                .hir
                .exports
                .iter()
                .find(|e| {
                    e.parts.len() == 1
                        && e.parts[0].relation == *name
                        && matches!(e.format, ExportFormat::Parquet)
                })
                .map(|e| e.path.clone());
            let Some(path) = export else {
                self.err(
                    Diagnostic::error(
                        "M601",
                        format!("model table `{name}` is not exported to Parquet"),
                    )
                    .label(*span, "Power BI imports model tables from Parquet files")
                    .help(format!(
                        "add `export {name} to \"{name}.parquet\"` (Parquet keeps column types and nulls; CSV does not)"
                    )),
                );
                ok = false;
                continue;
            };
            for c in &rel.columns {
                scope.cols.push(SCol {
                    qualifier: Some(name.clone()),
                    name: c.name.clone(),
                    phys: c.name.clone(),
                    ty: c.ty,
                    lineage: c.lineage,
                    slot: slot as u8,
                });
            }
            // Power BI needs a type for every column; a guess would change aggregates (SUM
            // rejects text) and comparisons
            for c in rel.columns.iter().filter(|c| c.ty.ty == Type::Unknown) {
                self.err(
                    Diagnostic::error(
                        "M606",
                        format!(
                            "column `{}` of model table `{name}` has a type MAGI does not know",
                            c.name
                        ),
                    )
                    .label(*span, "Power BI needs the type of every column")
                    .help(format!(
                        "cast it in the dataset, e.g. `derive {0} = to_float({0})` (or to_decimal, to_int, to_string, to_date, to_bool)",
                        c.name
                    )),
                );
            }
            let columns = rel
                .columns
                .iter()
                .map(|c| ModelColumn {
                    source: c.name.clone(),
                    display: c.name.clone(),
                    ty: c.ty.ty,
                    hidden: false,
                    description: None,
                })
                .collect();
            tables.push(ModelTable {
                name: name.clone(),
                columns,
                path,
                description: None,
            });
        }
        if !ok {
            return;
        }
        let column =
            |this: &mut Self, c: &ast::ColumnName, what: &str| -> Option<(usize, String, Type)> {
                if c.qualifier.is_none() {
                    this.err(
                        Diagnostic::error("M602", format!("{what} must name its table"))
                            .label(c.span, "expected `table.column`"),
                    );
                    return None;
                }
                let sc = this.lookup(c, &scope)?;
                Some((sc.slot as usize, sc.phys, sc.ty.ty))
            };
        let mut relationships: Vec<ModelRelationship> = Vec::new();
        let mut relationship_spans: Vec<Span> = Vec::new();
        let mut metric_names: Vec<&ast::Ident> = Vec::new();
        let mut pending: Vec<PendingMetric> = Vec::new();
        for item in &d.items {
            match item {
                ModelItem::Table { rel, options, .. } => {
                    for o in options {
                        match o.key.name.as_str() {
                            "description" => {
                                let desc = self.opt_str(o);
                                if let Some(t) = tables.iter_mut().find(|t| t.name == rel.name) {
                                    t.description = desc;
                                }
                            }
                            _ => self.unknown_option(o, &["description"], "a model table"),
                        }
                    }
                }
                ModelItem::Relationship { from, to, span } => {
                    let (Some((ft, fc, fty)), Some((tt, tc, tty))) = (
                        column(self, from, "a relationship column"),
                        column(self, to, "a relationship column"),
                    ) else {
                        continue;
                    };
                    if ft == tt {
                        self.err(
                            Diagnostic::error(
                                "M602",
                                "a relationship must connect two different tables",
                            )
                            .label(*span, "same table on both ends"),
                        );
                        continue;
                    }
                    if tmdl::data_type(fty) != tmdl::data_type(tty) {
                        self.err(
                            Diagnostic::error("M602", format!("relationship columns have different types (`{fty}` and `{tty}`)"))
                                .label(*span, "Power BI needs matching key types")
                                .help("cast one side in a dataset, e.g. `derive id = to_int(id)`"),
                        );
                        continue;
                    }
                    let (from_table, to_table) = (tables[ft].name.clone(), tables[tt].name.clone());
                    if relationships.iter().any(|r| {
                        r.from_table == from_table
                            && r.from_column == fc
                            && r.to_table == to_table
                            && r.to_column == tc
                    }) {
                        self.err(
                            Diagnostic::error("M602", "this relationship is declared twice")
                                .label(*span, "duplicate"),
                        );
                        continue;
                    }
                    // foreign keys on the many side are hidden, as Power BI recommends
                    if let Some(col) = tables[ft].columns.iter_mut().find(|c| c.source == fc) {
                        col.hidden = true;
                    }
                    relationships.push(ModelRelationship {
                        from_table,
                        from_column: fc,
                        to_table,
                        to_column: tc,
                    });
                    relationship_spans.push(*span);
                }
                ModelItem::Dimension {
                    name,
                    column: c,
                    options,
                    span,
                } => {
                    let Some((t, col, _)) = column(self, c, "a dimension") else {
                        continue;
                    };
                    let mut description = None;
                    for o in options {
                        match o.key.name.as_str() {
                            "description" => description = self.opt_str(o),
                            _ => self.unknown_option(o, &["description"], "a dimension"),
                        }
                    }
                    let table = &mut tables[t];
                    if let Some(other) = table
                        .columns
                        .iter()
                        .find(|x| same_name(&x.display, &name.name) && x.source != col)
                    {
                        let mut diag = Diagnostic::error(
                            "M603",
                            format!(
                                "table `{}` already has a column named `{}`",
                                table.name, other.display
                            ),
                        )
                        .label(*span, "rename the dimension");
                        if other.display != name.name {
                            diag = diag.help("Power BI names ignore case");
                        }
                        self.err(diag);
                        continue;
                    }
                    if let Some(x) = table.columns.iter_mut().find(|x| x.source == col) {
                        x.display = name.name.clone();
                        x.hidden = false;
                        x.description = description;
                    }
                }
                ModelItem::Metric {
                    name,
                    options,
                    span,
                } => {
                    if let Some(first) =
                        metric_names.iter().find(|m| same_name(&m.name, &name.name))
                    {
                        let mut diag = Diagnostic::error(
                            "M603",
                            format!("metric `{}` is defined twice", name.name),
                        )
                        .label(name.span, "duplicate")
                        .label(first.span, "first defined here");
                        if first.name != name.name {
                            diag = diag.help("Power BI names ignore case");
                        }
                        self.err(diag);
                        continue;
                    }
                    metric_names.push(name);
                    let (mut value, mut format, mut description) = (None, None, None);
                    for o in options {
                        match o.key.name.as_str() {
                            "value" => value = Some(&o.value),
                            "description" => description = self.opt_str(o),
                            "format" => {
                                format = match &o.value.kind {
                                    ExprKind::Column(c)
                                        if c.qualifier.is_none()
                                            && ["currency", "percent", "integer", "decimal"]
                                                .contains(&c.name.name.as_str()) =>
                                    {
                                        Some(tmdl::format_string(&c.name.name))
                                    }
                                    ExprKind::Literal(Literal::Str(s)) => Some(s.clone()),
                                    _ => {
                                        self.err(
                                            Diagnostic::error("M007", "`format` is currency, percent, integer, decimal or a format string")
                                                .label(o.value.span, "unknown format"),
                                        );
                                        None
                                    }
                                }
                            }
                            _ => self.unknown_option(
                                o,
                                &["value", "format", "description"],
                                "a metric",
                            ),
                        }
                    }
                    let Some(value) = value else {
                        self.err(
                            Diagnostic::error(
                                "M604",
                                format!("metric `{}` needs `value`", name.name),
                            )
                            .label(*span, "e.g. `value: sum(sales.amount)`"),
                        );
                        continue;
                    };
                    let Some(t) = self.expr(value, &scope, &AggMode::Whole) else {
                        continue;
                    };
                    let home = (0..tables.len()).find(|s| !t.columns_of(*s as u8).is_empty());
                    let Some(home) = home else {
                        self.err(
                            Diagnostic::error("M604", "a metric must read a table")
                                .label(value.span, "e.g. `count(sales.id)` instead of `count()`"),
                        );
                        continue;
                    };
                    let format = format.unwrap_or_else(|| tmdl::measure_format(t.ty.ty));
                    pending.push(PendingMetric {
                        name,
                        value: t,
                        value_span: value.span,
                        home,
                        format,
                        description,
                    });
                }
            }
        }
        // relationships and measures name the model's columns, which a dimension may have renamed
        for r in &mut relationships {
            for (table, column) in [
                (&r.from_table, &mut r.from_column),
                (&r.to_table, &mut r.to_column),
            ] {
                if let Some(c) = tables
                    .iter()
                    .find(|t| &t.name == table)
                    .and_then(|t| t.columns.iter().find(|c| &c.source == column))
                {
                    *column = c.display.clone();
                }
            }
        }
        let inactive = tmdl::inactive(&relationships);
        for (i, why) in inactive.iter().enumerate() {
            let Some(why) = why else {
                continue;
            };
            let r = &relationships[i];
            // a second relationship between the same two tables (either direction)
            let same_pair = match why.path[..] {
                [p] => {
                    let (a, b) = (&relationships[p].from_table, &relationships[p].to_table);
                    (a, b) == (&r.from_table, &r.to_table) || (a, b) == (&r.to_table, &r.from_table)
                }
                _ => false,
            };
            let message = if same_pair {
                format!(
                    "`{}` and `{}` are already related; this relationship is inactive in Power BI",
                    r.from_table, r.to_table
                )
            } else if why.from == why.to {
                format!(
                    "this relationship closes a cycle of relationships through `{}`; it is inactive in Power BI",
                    why.from
                )
            } else {
                format!(
                    "this relationship adds a second filter path from `{}` to `{}`; it is inactive in Power BI",
                    why.from, why.to
                )
            };
            let mut diag =
                Diagnostic::warning("M605", message).label(relationship_spans[i], "inactive");
            for &p in &why.path {
                diag = diag.label(
                    relationship_spans[p],
                    if same_pair {
                        "the active relationship"
                    } else {
                        "active path"
                    },
                );
            }
            self.err(diag.help(if same_pair {
                format!(
                    "Power BI keeps one active relationship between two tables, and reports filter through it. A DAX measure can use this one with `CALCULATE(<measure>, {})`",
                    tmdl::use_relationship(r)
                )
            } else {
                "Power BI keeps one active filter path between two tables, and reports filter through it; declare the relationship reports should use first, or remove one of the paths".to_string()
            }));
        }
        let mut metrics: Vec<ModelMetric> = Vec::new();
        for m in pending {
            let clash = tables.iter().find_map(|t| {
                t.columns
                    .iter()
                    .find(|c| same_name(&c.display, &m.name.name))
                    .map(|c| (t, c))
            });
            if let Some((t, c)) = clash {
                self.err(
                    Diagnostic::error(
                        "M603",
                        format!(
                            "metric `{}` has the same name as column `{}` of table `{}`",
                            m.name.name, c.display, t.name
                        ),
                    )
                    .label(m.name.span, "rename the metric")
                    .help("Power BI needs measure names that differ from every column name in the model (ignoring case); a `dimension` can rename the column instead"),
                );
                continue;
            }
            let dax = match tmdl::dax(&m.value, &tables, &tables[m.home].name) {
                Ok(d) => d,
                Err(msg) => {
                    self.err(
                        Diagnostic::error("M604", format!("metric `{}` cannot be expressed as a Power BI measure: {msg}", m.name.name))
                            .label(m.value_span, "not translatable")
                            .help("MAGI only translates aggregates whose meaning is the same in DuckDB and DAX"),
                    );
                    continue;
                }
            };
            if tmdl::compares_text(&m.value) {
                self.err(
                    Diagnostic::note(
                        "M607",
                        format!(
                            "metric `{}` compares text, which Power BI does ignoring case",
                            m.name.name
                        ),
                    )
                    .label(m.value_span, "`count_distinct`, `min` and `max` over text")
                    .help("values that differ only in case (\"abc\", \"ABC\") are one value in Power BI and two in MAGI; normalise them in the dataset, e.g. `derive k = lower(k)`, if the measure must match"),
                );
            }
            metrics.push(ModelMetric {
                name: m.name.name.clone(),
                table: tables[m.home].name.clone(),
                dax,
                format: m.format,
                description: m.description,
            });
        }
        if self.hir.models.iter().any(|m| m.name == d.name.name) {
            self.err(
                Diagnostic::error("M003", format!("model `{}` is defined twice", d.name.name))
                    .label(d.name.span, "duplicate"),
            );
            return;
        }
        if tables.is_empty() {
            self.err(
                Diagnostic::error("M604", "a model needs at least one table")
                    .label(d.name.span, "add `table <relation>`"),
            );
            return;
        }
        self.hir.models.push(Model {
            name: d.name.name.clone(),
            tables,
            relationships,
            metrics,
        });
    }
}
