//! Resolution of `chart` declarations (drawn by an .svg export).

use crate::ast;
use crate::diagnostic::{Diagnostic, did_you_mean};
use crate::semantic::hir::*;
use crate::semantic::types::Type;
use crate::syntax::span::Span;

use super::Analyzer;

const KINDS: [&str; 4] = ["bar", "line", "pie", "waterfall"];

impl<'a> Analyzer<'a> {
    pub(super) fn chart(&mut self, d: &'a ast::ChartDecl) {
        let name = &d.name.name;
        // ---- name
        if !self.not_reserved(&d.name) {
            return;
        }
        if self.canon.contains_key(&name.to_ascii_lowercase()) {
            self.err(
                Diagnostic::error("M003", format!("`{name}` is already a relation"))
                    .label(d.name.span, "charts and relations share one namespace"),
            );
            return;
        }
        if !self.chart_decls.insert(name.to_ascii_lowercase()) {
            self.err(
                Diagnostic::error("M003", format!("chart `{name}` is defined twice"))
                    .label(d.name.span, "second definition"),
            );
            return;
        }
        // ---- relation
        let Some(ri) = self.rel_ref(&d.rel) else {
            return;
        };
        let rel = self.relation(ri).clone();
        if rel.open {
            self.err(
                Diagnostic::error(
                    "M508",
                    format!(
                        "the columns of `{}` are not known, so chart `{name}` cannot be checked",
                        rel.name
                    ),
                )
                .label(d.rel.span, "columns unknown")
                .help("declare the relation's `schema`"),
            );
            return;
        }
        // ---- kind
        let mut opts: Vec<&'a ast::Opt> = Vec::new();
        let mut twice = false;
        for o in &d.options {
            if opts.iter().any(|p| p.key.name == o.key.name) {
                self.err(
                    Diagnostic::error("M508", format!("`{}` is given twice", o.key.name))
                        .label(o.key.span, "second time"),
                );
                twice = true;
            } else {
                opts.push(o);
            }
        }
        if twice {
            return;
        }
        let Some(kind_opt) = opts.iter().find(|o| o.key.name == "kind") else {
            self.err(
                Diagnostic::error("M508", format!("chart `{name}` needs a `kind`"))
                    .label(d.name.span, "no `kind`")
                    .help("`kind: bar`, `line`, `pie` or `waterfall`"),
            );
            return;
        };
        let Some((k, k_span)) = self.opt_column(kind_opt) else {
            return;
        };
        let kind = match k.to_ascii_lowercase().as_str() {
            "bar" => ChartKind::Bar,
            "line" => ChartKind::Line,
            "pie" => ChartKind::Pie,
            "waterfall" => ChartKind::Waterfall,
            _ => {
                let help = match did_you_mean(&k, KINDS) {
                    Some(s) => format!("did you mean `{s}`?"),
                    None => "`kind: bar`, `line`, `pie` or `waterfall`".to_string(),
                };
                self.err(
                    Diagnostic::error("M508", format!("unknown chart kind `{k}`"))
                        .label(k_span, "not a chart kind")
                        .help(help),
                );
                return;
            }
        };
        // ---- options and columns
        let allowed: &[&str] = match kind {
            ChartKind::Bar | ChartKind::Line => {
                &["kind", "x", "y", "series", "title", "width", "height"]
            }
            ChartKind::Pie => &["kind", "x", "y", "title", "width", "height"],
            ChartKind::Waterfall => &["kind", "x", "y", "total", "title", "width", "height"],
        };
        let mut ok = true;
        let (mut title, mut width, mut height) = (None, 600u32, 400u32);
        let (mut x, mut y, mut series, mut total) = (None, None, None, None);
        for o in &opts {
            let key = o.key.name.as_str();
            match key {
                "kind" => {}
                "series" if !allowed.contains(&"series") => {
                    self.err(
                        Diagnostic::error("M508", "`series` is only for bar and line charts")
                            .label(o.key.span, "not for this kind"),
                    );
                    ok = false;
                }
                "total" if !allowed.contains(&"total") => {
                    self.err(
                        Diagnostic::error("M508", "`total` is only for waterfall charts")
                            .label(o.key.span, "not for this kind"),
                    );
                    ok = false;
                }
                "x" | "y" | "series" | "total" => {
                    let Some(c) = self.chart_column(&rel, o) else {
                        ok = false;
                        continue;
                    };
                    let slot = match key {
                        "x" => &mut x,
                        "y" => &mut y,
                        "series" => &mut series,
                        _ => &mut total,
                    };
                    *slot = Some(c);
                }
                "title" => match self.opt_str(o) {
                    Some(t) => title = Some(t),
                    None => ok = false,
                },
                "width" | "height" => match self.opt_int(o) {
                    Some(v) if (200..=4000).contains(&v) => {
                        let v = v as u32;
                        if key == "width" {
                            width = v
                        } else {
                            height = v
                        }
                    }
                    Some(_) => {
                        self.err(
                            Diagnostic::error(
                                "M508",
                                format!("`{key}` must be between 200 and 4000 pixels"),
                            )
                            .label(o.value.span, "out of range"),
                        );
                        ok = false;
                    }
                    None => ok = false,
                },
                _ => {
                    self.unknown_option(o, allowed, "a chart");
                    ok = false;
                }
            }
        }
        for key in ["x", "y"] {
            if !opts.iter().any(|o| o.key.name == key) {
                self.err(
                    Diagnostic::error("M508", format!("chart `{name}` needs `{key}`"))
                        .label(d.name.span, format!("no `{key}`")),
                );
                ok = false;
            }
        }
        // ---- types
        if let Some((c, span)) = &y {
            match c.ty.ty {
                Type::Int | Type::Decimal(..) | Type::Float => {}
                Type::Unknown => {
                    self.err(
                        Diagnostic::error("M508", format!("the type of `{}` is not known", c.name))
                            .label(*span, "y of the chart")
                            .help(format!(
                                "cast it in the dataset, e.g. `derive {0} = to_decimal({0})`",
                                c.name
                            )),
                    );
                    ok = false;
                }
                ty => {
                    self.err(
                        Diagnostic::error(
                            "M508",
                            format!("`y` of a chart must be a number; `{}` is {ty}", c.name),
                        )
                        .label(*span, "not a number"),
                    );
                    ok = false;
                }
            }
        }
        for (c, span) in [&x, &series].into_iter().flatten() {
            if matches!(
                c.ty.ty,
                Type::Json | Type::Binary | Type::Unknown | Type::Null
            ) {
                self.err(
                    Diagnostic::error(
                        "M508",
                        format!("`{}` ({}) cannot be drawn", c.name, c.ty.ty),
                    )
                    .label(*span, "not drawable"),
                );
                ok = false;
            }
        }
        if let Some((c, span)) = &total
            && c.ty.ty != Type::Bool
        {
            self.err(
                Diagnostic::error(
                    "M508",
                    format!("`total` must be a bool column; `{}` is {}", c.name, c.ty.ty),
                )
                .label(*span, "not a bool"),
            );
            ok = false;
        }
        if !ok {
            return;
        }
        let (Some((x, _)), Some((y, _))) = (x, y) else {
            return;
        };
        // ---- axis
        let axis = match (kind, x.ty.ty) {
            (ChartKind::Line, Type::Date | Type::Timestamp | Type::TimestampTz) => ChartAxis::Time,
            (ChartKind::Line, Type::Int | Type::Decimal(..) | Type::Float) => ChartAxis::Value,
            _ => ChartAxis::Category,
        };
        self.hir.charts.push(Chart {
            name: name.clone(),
            relation: rel.name.clone(),
            kind,
            axis,
            x: x.name,
            x_ty: x.ty.ty,
            y: y.name,
            series: series.map(|(c, _)| c.name),
            total: total.map(|(c, _)| c.name),
            title,
            width,
            height,
        });
    }

    /// The relation column an `x`/`y`/`series`/`total` option names, and the option value's span.
    fn chart_column(&mut self, rel: &Relation, o: &ast::Opt) -> Option<(Column, Span)> {
        let (c, span) = self.opt_column(o)?;
        if let Some(col) = rel.column(&c) {
            return Some((col.clone(), span));
        }
        let mut d = Diagnostic::error("M508", format!("`{}` has no column `{c}`", rel.name))
            .label(span, "unknown column");
        if let Some(s) = did_you_mean(&c, rel.columns.iter().map(|c| c.name.as_str())) {
            d = d.help(format!("did you mean `{s}`?"));
        }
        self.err(d);
        None
    }

    /// Index of the resolved chart called `name`, ignoring ASCII case.
    pub(super) fn chart_named(&self, name: &str) -> Option<usize> {
        self.hir
            .charts
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    }
}
