//! Expression resolution and type checking.

use crate::ast::{self, BinaryOp, ExprKind, Literal, UnaryOp};
use crate::diagnostic::{Diagnostic, did_you_mean};
use crate::semantic::functions::{self, Resolved};
use crate::semantic::hir::*;
use crate::semantic::types::{ColType, Type};
use crate::syntax::span::Span;

use super::Analyzer;

/// A column visible to expressions.
#[derive(Debug, Clone)]
pub struct SCol {
    /// Relation name (or alias) the column came from; used for `x.col` references.
    pub qualifier: Option<String>,
    /// Name analysts use.
    pub name: String,
    /// Unique name of the column in the operator's input.
    pub phys: String,
    pub ty: ColType,
    pub lineage: LineageId,
    /// Operator input the column belongs to.
    pub slot: u8,
}

#[derive(Debug, Clone, Default)]
pub struct Scope {
    pub cols: Vec<SCol>,
    /// Inputs whose columns are not known statically: `(qualifier, slot)`.
    pub open: Vec<(Option<String>, u8)>,
    /// Extra qualifiers naming a whole input slot (reconcile `a` / `b`).
    pub slot_names: Vec<(String, u8)>,
    /// Named expressions usable by bare name (reconcile-level evidence inside tiers).
    pub aliases: Vec<(String, TExpr)>,
}

impl Scope {
    pub fn single(rel: &Relation, qualifier: &str) -> Scope {
        let q = Some(qualifier.to_string());
        Scope {
            cols: rel
                .columns
                .iter()
                .map(|c| SCol {
                    qualifier: q.clone(),
                    name: c.name.clone(),
                    phys: c.name.clone(),
                    ty: c.ty,
                    lineage: c.lineage,
                    slot: 0,
                })
                .collect(),
            open: if rel.open { vec![(q, 0)] } else { Vec::new() },
            slot_names: Vec::new(),
            aliases: Vec::new(),
        }
    }

    fn qualifier_matches(&self, c: &SCol, q: &str) -> bool {
        c.qualifier.as_deref() == Some(q)
            || self.slot_names.iter().any(|(n, s)| n == q && *s == c.slot)
    }

    pub fn qualifiers(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for c in &self.cols {
            if let Some(q) = &c.qualifier
                && !out.contains(q)
            {
                out.push(q.clone());
            }
        }
        for (n, _) in &self.slot_names {
            if !out.contains(n) {
                out.push(n.clone());
            }
        }
        out
    }
}

/// Where aggregate functions are allowed.
pub enum AggMode {
    Forbidden(&'static str),
    /// Allowed anywhere (validation checks; mixing is checked separately).
    Whole,
    /// `group by ... |> aggregate`: bare columns must be group keys `(slot, phys)`.
    Group(Vec<(u8, String)>),
    /// Rollup tiers: aggregates only over grouped slots; bare columns on a grouped slot must be
    /// its group keys.
    Rollup {
        grouped: [bool; 2],
        keys: [Vec<String>; 2],
    },
}

impl<'a> Analyzer<'a> {
    pub(super) fn lookup(&mut self, c: &ast::ColumnName, scope: &Scope) -> Option<SCol> {
        let name = &c.name.name;
        let matches: Vec<&SCol> = match &c.qualifier {
            Some(q) => scope
                .cols
                .iter()
                .filter(|s| &s.name == name && scope.qualifier_matches(s, &q.name))
                .collect(),
            None => scope.cols.iter().filter(|s| &s.name == name).collect(),
        };
        match matches.as_slice() {
            [one] => return Some((*one).clone()),
            [] => {}
            many => {
                let options = many
                    .iter()
                    .map(|m| match &m.qualifier {
                        Some(q) => format!("`{q}.{name}`"),
                        None => format!("`{name}`"),
                    })
                    .collect::<Vec<_>>()
                    .join(" or ");
                self.err(
                    Diagnostic::error("M011", format!("column `{}` is ambiguous", c.display()))
                        .label(c.span, "matches more than one column")
                        .help(format!("qualify it: {options}")),
                );
                return None;
            }
        }
        // open inputs accept any column
        let open = scope.open.iter().find(|(q, slot)| match &c.qualifier {
            None => true,
            Some(want) => {
                q.as_deref() == Some(want.name.as_str())
                    || scope
                        .slot_names
                        .iter()
                        .any(|(n, s)| n == &want.name && s == slot)
            }
        });
        if let Some((q, slot)) = open {
            let lineage = self
                .hir
                .lineage
                .add(format!("{} (unchecked column)", c.display()), Vec::new());
            return Some(SCol {
                qualifier: q.clone(),
                name: name.clone(),
                phys: name.clone(),
                ty: ColType::nullable(Type::Unknown),
                lineage,
                slot: *slot,
            });
        }
        if let Some(q) = &c.qualifier {
            let quals = scope.qualifiers();
            if !quals.contains(&q.name) {
                let mut d =
                    Diagnostic::error("M010", format!("`{}` is not available here", q.name))
                        .label(q.span, "unknown relation qualifier");
                d = match did_you_mean(&q.name, quals.iter().map(String::as_str)) {
                    Some(s) => d.help(format!("did you mean `{s}`?")),
                    None if quals.is_empty() => {
                        d.help("this position has no qualified inputs; use the bare column name")
                    }
                    None => d.help(format!(
                        "available: {}",
                        quals
                            .iter()
                            .map(|x| format!("`{x}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                };
                self.err(d);
                return None;
            }
        }
        let candidates: Vec<&str> = scope
            .cols
            .iter()
            .filter(|s| {
                c.qualifier
                    .as_ref()
                    .is_none_or(|q| scope.qualifier_matches(s, &q.name))
            })
            .map(|s| s.name.as_str())
            .collect();
        let mut d = Diagnostic::error("M012", format!("column `{}` does not exist", c.display()))
            .label(c.span, "unknown column");
        if let Some(s) = did_you_mean(name, candidates.iter().copied()) {
            d = d.help(format!("did you mean `{s}`?"));
        } else if !candidates.is_empty() && candidates.len() <= 30 {
            d = d.help(format!(
                "available columns: {}",
                candidates
                    .iter()
                    .map(|x| format!("`{x}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        self.err(d);
        None
    }

    /// Resolve and type an expression.
    pub(super) fn expr(&mut self, e: &ast::Expr, scope: &Scope, mode: &AggMode) -> Option<TExpr> {
        self.ex(e, scope, mode, None)
    }

    /// Lineage ids of the columns an expression reads.
    pub(super) fn deps(&self, t: &TExpr, scope: &Scope) -> Vec<LineageId> {
        let mut out = Vec::new();
        t.walk(&mut |x| {
            if let TExprKind::Column { slot, name } = &x.kind
                && let Some(c) = scope
                    .cols
                    .iter()
                    .find(|c| c.slot == *slot && &c.phys == name)
                && !out.contains(&c.lineage)
            {
                out.push(c.lineage);
            }
        });
        out
    }

    /// `in_agg`: the slot being aggregated when inside an aggregate call.
    fn ex(
        &mut self,
        e: &ast::Expr,
        scope: &Scope,
        mode: &AggMode,
        in_agg: Option<u8>,
    ) -> Option<TExpr> {
        match &e.kind {
            ExprKind::Literal(l) => Some(TExpr::lit(match l {
                Literal::Int(v) => Lit::Int(*v),
                Literal::Decimal(d) => Lit::Decimal(d.clone()),
                Literal::Str(s) => Lit::Str(s.clone()),
                Literal::Bool(b) => Lit::Bool(*b),
                Literal::Null => Lit::Null,
            })),
            ExprKind::Paren(inner) => self.ex(inner, scope, mode, in_agg),
            ExprKind::Column(c) => {
                if c.qualifier.is_none()
                    && let Some((_, alias)) = scope.aliases.iter().find(|(n, _)| n == &c.name.name)
                {
                    if let AggMode::Rollup { grouped, keys } = mode {
                        for slot in 0..2u8 {
                            let cols = alias.columns_of(slot);
                            if grouped[slot as usize]
                                && cols.iter().any(|x| !keys[slot as usize].contains(x))
                            {
                                self.err(
                                    Diagnostic::error("M104", format!("`{}` reads single rows and cannot be used in a rollup tier", c.name.name))
                                        .label(c.span, "evidence of individual rows")
                                        .help("write the condition with aggregates over the grouped side, e.g. `sum(a.amount)`"),
                                );
                                return None;
                            }
                        }
                    }
                    return Some(alias.clone());
                }
                let col = self.lookup(c, scope)?;
                match (mode, in_agg) {
                    (AggMode::Group(keys), None) => {
                        if !keys.iter().any(|(s, p)| *s == col.slot && p == &col.phys) {
                            self.err(
                                Diagnostic::error(
                                    "M104",
                                    format!("`{}` is neither grouped nor aggregated", c.display()),
                                )
                                .label(c.span, "not a group key")
                                .help(format!(
                                    "add it to `group by`, or aggregate it, e.g. `max({})`",
                                    c.display()
                                )),
                            );
                            return None;
                        }
                    }
                    (AggMode::Rollup { grouped, keys }, agg_slot) => {
                        let slot = col.slot as usize;
                        match agg_slot {
                            None if grouped[slot] && !keys[slot].contains(&col.phys) => {
                                self.err(
                                    Diagnostic::error(
                                        "M104",
                                        format!(
                                            "`{}` is not a group key of this rollup",
                                            c.display()
                                        ),
                                    )
                                    .label(c.span, "varies within the group")
                                    .help(format!(
                                        "aggregate it, e.g. `sum({})`, or add it to `group ... by`",
                                        c.display()
                                    )),
                                );
                                return None;
                            }
                            Some(s) if s as usize != slot => {
                                self.err(
                                    Diagnostic::error(
                                        "M104",
                                        "an aggregate may only read columns of the grouped side",
                                    )
                                    .label(c.span, "column of the other side"),
                                );
                                return None;
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
                Some(TExpr::col(col.slot, col.phys, col.ty))
            }
            ExprKind::List(items) => {
                let mut out = Vec::new();
                for i in items {
                    out.push(self.ex(i, scope, mode, in_agg)?);
                }
                Some(TExpr::new(
                    TExprKind::List(out),
                    ColType::required(Type::Unknown),
                ))
            }
            ExprKind::Unary { op, expr } => {
                let t = self.ex(expr, scope, mode, in_agg)?;
                let ok = match op {
                    UnaryOp::Neg => t.ty.ty.is_numeric(),
                    UnaryOp::Not => matches!(t.ty.ty, Type::Bool | Type::Unknown | Type::Null),
                };
                if !ok {
                    let want = if *op == UnaryOp::Neg {
                        "a number"
                    } else {
                        "a condition"
                    };
                    self.err(
                        Diagnostic::error("M105", format!("expected {want}, found `{}`", t.ty.ty))
                            .label(expr.span, "wrong type"),
                    );
                    return None;
                }
                let ty = t.ty;
                Some(TExpr::new(
                    TExprKind::Unary {
                        op: *op,
                        expr: Box::new(t),
                    },
                    ty,
                ))
            }
            ExprKind::Binary { op, left, right } => {
                let l = self.ex(left, scope, mode, in_agg);
                let r = self.ex(right, scope, mode, in_agg);
                let (l, r) = (l?, r?);
                match binary_type(*op, l.ty, r.ty) {
                    Ok(ty) => Some(TExpr::new(
                        TExprKind::Binary {
                            op: *op,
                            left: Box::new(l),
                            right: Box::new(r),
                        },
                        ty,
                    )),
                    Err((msg, help)) => {
                        let mut d = Diagnostic::error("M105", msg)
                            .label(left.span, format!("`{}`", l.ty))
                            .label(right.span, format!("`{}`", r.ty));
                        if let Some(h) = help {
                            d = d.help(h);
                        }
                        self.err(d);
                        None
                    }
                }
            }
            ExprKind::IsNull { expr, negated } => {
                let t = self.ex(expr, scope, mode, in_agg)?;
                Some(TExpr::new(
                    TExprKind::IsNull {
                        expr: Box::new(t),
                        negated: *negated,
                    },
                    ColType::required(Type::Bool),
                ))
            }
            ExprKind::InList {
                expr,
                list,
                negated,
            } => {
                let t = self.ex(expr, scope, mode, in_agg)?;
                let mut items = Vec::new();
                let mut nullable = t.ty.nullable;
                for i in list {
                    let it = self.ex(i, scope, mode, in_agg)?;
                    if !Type::comparable(t.ty.ty, it.ty.ty) {
                        self.err(
                            Diagnostic::error(
                                "M105",
                                format!("cannot compare `{}` with `{}`", t.ty.ty, it.ty.ty),
                            )
                            .label(i.span, "list item")
                            .label(expr.span, "value"),
                        );
                        return None;
                    }
                    nullable |= it.ty.nullable;
                    items.push(it);
                }
                Some(TExpr::new(
                    TExprKind::InList {
                        expr: Box::new(t),
                        list: items,
                        negated: *negated,
                    },
                    ColType::new(Type::Bool, nullable),
                ))
            }
            ExprKind::Case { arms, otherwise } => {
                let mut out = Vec::new();
                let mut ty = Type::Null;
                let mut nullable = otherwise.is_none();
                for arm in arms {
                    let w = self.ex(&arm.when, scope, mode, in_agg)?;
                    if !self.expect_bool(&w, arm.when.span, "a `case` condition") {
                        return None;
                    }
                    let t = self.ex(&arm.then, scope, mode, in_agg)?;
                    ty = self.unify_at(ty, t.ty.ty, arm.then.span)?;
                    nullable |= t.ty.nullable;
                    out.push((w, t));
                }
                let other = match otherwise {
                    Some(o) => {
                        let t = self.ex(o, scope, mode, in_agg)?;
                        ty = self.unify_at(ty, t.ty.ty, o.span)?;
                        nullable |= t.ty.nullable;
                        Some(Box::new(t))
                    }
                    None => None,
                };
                Some(TExpr::new(
                    TExprKind::Case {
                        arms: out,
                        otherwise: other,
                    },
                    ColType::new(ty, nullable),
                ))
            }
            ExprKind::Call {
                namespace,
                name,
                args,
            } => self.call(e, namespace.as_ref(), name, args, scope, mode, in_agg),
        }
    }

    fn unify_at(&mut self, a: Type, b: Type, span: Span) -> Option<Type> {
        match Type::unify(a, b) {
            Some(t) => Some(t),
            None => {
                self.err(
                    Diagnostic::error(
                        "M106",
                        format!("branches have incompatible types `{a}` and `{b}`"),
                    )
                    .label(span, format!("`{b}`")),
                );
                None
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn call(
        &mut self,
        e: &ast::Expr,
        namespace: Option<&ast::Ident>,
        name: &ast::Ident,
        args: &[ast::Expr],
        scope: &Scope,
        mode: &AggMode,
        in_agg: Option<u8>,
    ) -> Option<TExpr> {
        if let Some(ns) = namespace {
            if ns.name != "duckdb" {
                self.err(
                    Diagnostic::error("M107", format!("unknown function namespace `{}`", ns.name))
                        .label(ns.span, "only `duckdb.` is available")
                        .help("backend functions are called as `duckdb.function_name(...)`"),
                );
                return None;
            }
            use crate::semantic::native::{self, Lookup, Stability};
            match native::lookup(&name.name) {
                Ok(Lookup::Missing) => {
                    let mut d = Diagnostic::error(
                        "M107",
                        format!("DuckDB has no function `{}`", name.name),
                    )
                    .label(name.span, "unknown DuckDB function");
                    if let Some(s) = did_you_mean(&name.name, native::names()) {
                        d = d.help(format!("did you mean `duckdb.{s}`?"));
                    }
                    self.err(d);
                    return None;
                }
                Ok(Lookup::Aggregate) => {
                    self.err(
                        Diagnostic::error(
                            "M107",
                            format!("`duckdb.{}` is an aggregate function", name.name),
                        )
                        .label(name.span, "`duckdb.` only calls scalar functions")
                        .help("aggregate with MAGI's functions in `group by ... |> aggregate { ... }` (`sum`, `count`, `mean`, `min`, `max`, ...)"),
                    );
                    return None;
                }
                Ok(Lookup::Table) => {
                    self.err(
                        Diagnostic::error(
                            "M107",
                            format!("`duckdb.{}` is a table function", name.name),
                        )
                        .label(name.span, "`duckdb.` only calls scalar functions")
                        .help("a table function produces rows, not a value; read files with a `source`, or use `native_sql`"),
                    );
                    return None;
                }
                Ok(Lookup::Scalar(Stability::Volatile)) => self.err(
                    Diagnostic::warning(
                        "M123",
                        format!(
                            "`duckdb.{}` returns different values on every run",
                            name.name
                        ),
                    )
                    .label(name.span, "results that depend on it are not reproducible"),
                ),
                Ok(Lookup::Scalar(Stability::PerRun)) => {
                    let lower = name.name.to_ascii_lowercase();
                    if let Err(i) = self.hir.run_time_natives.binary_search(&lower) {
                        self.hir.run_time_natives.insert(i, lower);
                    }
                    self.err(
                        Diagnostic::warning(
                            "M123",
                            format!(
                                "`duckdb.{}` depends on when the run happens",
                                name.name
                            ),
                        )
                        .label(
                            name.span,
                            "results that depend on it are not reproducible; `--today` does not pin it",
                        )
                        .help("for the run date use `today()`, which `--today YYYY-MM-DD` pins"),
                    )
                }
                Ok(Lookup::Scalar(Stability::Stable)) | Err(_) => {}
            }
            self.natives_used += 1;
            if self.natives_noted.insert(name.name.to_ascii_lowercase()) {
                self.err(
                    Diagnostic::warning(
                        "M115",
                        format!("`duckdb.{}` is DuckDB-specific", name.name),
                    )
                    .label(name.span, "not portable; MAGI does not type check it"),
                );
            }
            let mut out = Vec::new();
            for a in args {
                out.push(self.ex(a, scope, mode, in_agg)?);
            }
            return Some(TExpr::new(
                TExprKind::Native {
                    name: name.name.clone(),
                    args: out,
                },
                ColType::nullable(Type::Unknown),
            ));
        }
        let fname = name.name.as_str();
        // functions that need literal arguments or mapping names
        match fname {
            "today" => {
                if !args.is_empty() {
                    self.err(
                        Diagnostic::error("M108", "`today()` takes no arguments")
                            .label(e.span, "here"),
                    );
                    return None;
                }
                self.hir.uses_today = true;
                return Some(TExpr::lit(Lit::Date(self.hir.today.clone())));
            }
            "date" if args.len() == 1 => {
                if let ExprKind::Literal(Literal::Str(s)) = &args[0].kind {
                    if !valid_iso_date(s) {
                        self.err(
                            Diagnostic::error("M108", format!("`{s}` is not a valid date"))
                                .label(args[0].span, "expected YYYY-MM-DD")
                                .help("for other formats use `parse_date(text, \"%d/%m/%Y\")`"),
                        );
                        return None;
                    }
                    return Some(TExpr::lit(Lit::Date(s.clone())));
                }
                let t = self.ex(&args[0], scope, mode, in_agg)?;
                return Some(TExpr::new(
                    TExprKind::Call {
                        func: "to_date",
                        args: vec![t],
                    },
                    ColType::nullable(Type::Date),
                ));
            }
            "date" => {
                if args.len() != 3 {
                    self.err(
                        Diagnostic::error(
                            "M108",
                            "`date` takes a literal `\"YYYY-MM-DD\"` or `(year, month, day)`",
                        )
                        .label(e.span, "here"),
                    );
                    return None;
                }
                let mut out = Vec::new();
                for a in args {
                    let t = self.ex(a, scope, mode, in_agg)?;
                    if !matches!(t.ty.ty, Type::Int | Type::Unknown | Type::Null) {
                        self.err(
                            Diagnostic::error(
                                "M108",
                                format!("date parts must be int, found `{}`", t.ty.ty),
                            )
                            .label(a.span, "here"),
                        );
                        return None;
                    }
                    out.push(t);
                }
                // invalid dates (month 13) become null
                return Some(TExpr::new(
                    TExprKind::Call {
                        func: "make_date",
                        args: out,
                    },
                    ColType::nullable(Type::Date),
                ));
            }
            "parse_date" | "parse_timestamp" => {
                if args.len() < 2 {
                    self.err(
                        Diagnostic::error(
                            "M108",
                            format!("`{fname}` needs the text and at least one format"),
                        )
                        .label(e.span, "here")
                        .help(format!(
                            "e.g. `{fname}(event_date, \"%m/%d/%Y\", \"%Y-%m-%d\")`"
                        )),
                    );
                    return None;
                }
                let t = self.ex(&args[0], scope, mode, in_agg)?;
                if !matches!(t.ty.ty, Type::String | Type::Unknown | Type::Null) {
                    self.err(
                        Diagnostic::error(
                            "M108",
                            format!("`{fname}` reads text, found `{}`", t.ty.ty),
                        )
                        .label(args[0].span, "not text")
                        .help("the value is already typed; no parsing needed"),
                    );
                    return None;
                }
                let mut formats = Vec::new();
                for a in &args[1..] {
                    match &a.kind {
                        ExprKind::Literal(Literal::Str(f)) => {
                            formats.push(TExpr::lit(Lit::Str(f.clone())))
                        }
                        _ => {
                            self.err(
                                Diagnostic::error("M108", "formats must be string literals")
                                    .label(a.span, "expected e.g. \"%d/%m/%Y\""),
                            );
                            return None;
                        }
                    }
                }
                let (func, ty) = if fname == "parse_date" {
                    ("parse_date", Type::Date)
                } else {
                    ("parse_timestamp", Type::Timestamp)
                };
                return Some(TExpr::new(
                    TExprKind::Call {
                        func,
                        args: vec![
                            t,
                            TExpr::new(TExprKind::List(formats), ColType::required(Type::Unknown)),
                        ],
                    },
                    ColType::nullable(ty),
                ));
            }
            // DuckDB needs the capture group number when it plans the query
            "regexp_extract"
                if args.len() == 3
                    && !matches!(args[2].kind, ExprKind::Literal(Literal::Int(g)) if g >= 0) =>
            {
                self.err(
                    Diagnostic::error(
                        "M108",
                        "the group of `regexp_extract` must be a literal number",
                    )
                    .label(args[2].span, "expected e.g. `1`"),
                );
                return None;
            }
            "to_decimal" => {
                let lit_u8 = |a: &ast::Expr| match a.kind {
                    ExprKind::Literal(Literal::Int(v)) if (0..=38).contains(&v) => Some(v as u8),
                    _ => None,
                };
                let (p, s) = match args {
                    [_, p, s] => (lit_u8(p), lit_u8(s)),
                    _ => (None, None),
                };
                let (Some(p), Some(s)) = (p, s) else {
                    self.err(Diagnostic::error("M108", "`to_decimal(value, precision, scale)` needs literal precision and scale").label(e.span, "here"));
                    return None;
                };
                if p == 0 || s > p {
                    self.err(
                        Diagnostic::error(
                            "M108",
                            "precision must be 1..38 and scale at most precision",
                        )
                        .label(e.span, "here"),
                    );
                    return None;
                }
                let t = self.ex(&args[0], scope, mode, in_agg)?;
                return Some(TExpr::new(
                    TExprKind::TryCast {
                        expr: Box::new(t),
                        ty: Type::Decimal(p, s),
                    },
                    ColType::nullable(Type::Decimal(p, s)),
                ));
            }
            "map" | "replace_words" => {
                let [value, mapping] = args else {
                    self.err(
                        Diagnostic::error(
                            "M108",
                            format!("`{fname}(value, mapping_name)` takes two arguments"),
                        )
                        .label(e.span, "here"),
                    );
                    return None;
                };
                let ExprKind::Column(ast::ColumnName {
                    qualifier: None,
                    name: mname,
                    ..
                }) = &mapping.kind
                else {
                    self.err(
                        Diagnostic::error("M108", "the second argument must be a mapping name")
                            .label(mapping.span, "expected a mapping"),
                    );
                    return None;
                };
                let decl = self.mapping(mname)?;
                let t = self.ex(value, scope, mode, in_agg)?;
                if fname == "map" {
                    return self.mapping_case(t, value.span, &decl.arms, None);
                }
                if !matches!(t.ty.ty, Type::String | Type::Unknown | Type::Null) {
                    self.err(
                        Diagnostic::error("M108", "`replace_words` works on text")
                            .label(value.span, format!("`{}`", t.ty.ty)),
                    );
                    return None;
                }
                let mut pairs = Vec::new();
                for arm in &decl.arms {
                    match (&arm.pattern, &arm.value) {
                        (
                            ast::MapPattern::Values(vs),
                            ast::MapValue::Expr(ast::Expr {
                                kind: ExprKind::Literal(Literal::Str(to)),
                                ..
                            }),
                        ) => {
                            for v in vs {
                                if let ExprKind::Literal(Literal::Str(from)) = &v.kind {
                                    pairs.push(TExpr::lit(Lit::Str(from.clone())));
                                    pairs.push(TExpr::lit(Lit::Str(to.clone())));
                                } else {
                                    self.err(
                                        Diagnostic::error("M109", "word mappings map text to text")
                                            .label(v.span, "not a string"),
                                    );
                                    return None;
                                }
                            }
                        }
                        (ast::MapPattern::Otherwise, ast::MapValue::Original) => {}
                        _ => {
                            self.err(
                                Diagnostic::error("M109", "`replace_words` needs a mapping of `\"word\" => \"replacement\"` pairs")
                                    .label(arm.span, "unsupported arm"),
                            );
                            return None;
                        }
                    }
                }
                let ty = t.ty;
                return Some(TExpr::new(
                    TExprKind::Call {
                        func: "replace_words",
                        args: vec![
                            t,
                            TExpr::new(TExprKind::List(pairs), ColType::required(Type::Unknown)),
                        ],
                    },
                    ty,
                ));
            }
            _ => {}
        }

        let is_agg = functions::is_aggregate(fname);
        if is_agg {
            if let Some(_outer) = in_agg {
                self.err(
                    Diagnostic::error("M110", "aggregates cannot be nested")
                        .label(e.span, "aggregate inside an aggregate"),
                );
                return None;
            }
            match mode {
                AggMode::Forbidden(ctx) => {
                    self.err(
                        Diagnostic::error("M110", format!("aggregate `{fname}` is not allowed in {ctx}"))
                            .label(e.span, "aggregate")
                            .help("aggregates belong in `aggregate { ... }`, aggregate checks, or rollup tiers"),
                    );
                    return None;
                }
                AggMode::Rollup { grouped, .. } => {
                    // which side is aggregated: the slot of the argument's columns
                    let slot = self.agg_slot(args, scope, *grouped, e.span)?;
                    return self.finish_agg(e, fname, args, scope, mode, slot);
                }
                _ => return self.finish_agg(e, fname, args, scope, mode, 0),
            }
        }
        let mut targs = Vec::new();
        let mut failed = false;
        for a in args {
            match self.ex(a, scope, mode, in_agg) {
                Some(t) => targs.push(t),
                None => failed = true,
            }
        }
        if failed {
            return None;
        }
        let types: Vec<ColType> = targs.iter().map(|t| t.ty).collect();
        match functions::resolve(fname, &types) {
            Ok(Resolved::Scalar(func, ty)) => {
                Some(TExpr::new(TExprKind::Call { func, args: targs }, ty))
            }
            Ok(Resolved::Agg(..)) => unreachable!("aggregates handled above"),
            Err(fe) => {
                let span = fe.arg.and_then(|i| args.get(i)).map_or(e.span, |a| a.span);
                let unknown = fe.message_is_unknown(fname);
                let mut d = Diagnostic::error("M108", fe.message).label(span, "here");
                if unknown {
                    if let Some(s) = did_you_mean(fname, functions::ALL.iter().copied()) {
                        d = d.help(format!("did you mean `{s}`?"));
                    } else {
                        d = d.help(format!(
                            "DuckDB functions can be called as `duckdb.{fname}(...)` (not portable)"
                        ));
                    }
                }
                self.err(d);
                None
            }
        }
    }

    fn agg_slot(
        &mut self,
        args: &[ast::Expr],
        scope: &Scope,
        grouped: [bool; 2],
        span: Span,
    ) -> Option<u8> {
        let mut slots = Vec::new();
        for a in args {
            a.walk(&mut |x| {
                if let ExprKind::Column(c) = &x.kind {
                    let q = c.qualifier.as_ref().map(|q| q.name.as_str());
                    for col in &scope.cols {
                        if col.name == c.name.name
                            && q.is_none_or(|q| scope.qualifier_matches(col, q))
                            && !slots.contains(&col.slot)
                        {
                            slots.push(col.slot);
                        }
                    }
                }
            });
        }
        let slot = match slots.as_slice() {
            [s] => *s,
            [] if grouped[0] != grouped[1] => u8::from(grouped[1]),
            [] => {
                self.err(
                    Diagnostic::error("M110", "say which side to count")
                        .label(span, "both sides are grouped")
                        .help("e.g. `count(a.id)`"),
                );
                return None;
            }
            _ => {
                self.err(
                    Diagnostic::error("M110", "an aggregate may only read one side")
                        .label(span, "reads both sides"),
                );
                return None;
            }
        };
        if !grouped[slot as usize] {
            self.err(
                Diagnostic::error(
                    "M110",
                    "aggregates are only allowed over the grouped side of a rollup",
                )
                .label(span, "this side is not grouped"),
            );
            return None;
        }
        Some(slot)
    }

    fn finish_agg(
        &mut self,
        e: &ast::Expr,
        fname: &str,
        args: &[ast::Expr],
        scope: &Scope,
        mode: &AggMode,
        slot: u8,
    ) -> Option<TExpr> {
        let mut targs = Vec::new();
        for a in args {
            targs.push(self.ex(a, scope, mode, Some(slot))?);
        }
        let types: Vec<ColType> = targs.iter().map(|t| t.ty).collect();
        match functions::resolve(fname, &types) {
            Ok(Resolved::Agg(func, ty)) => {
                if matches!(func, AggFunc::Sum | AggFunc::Mean)
                    && targs.first().is_some_and(|a| a.ty.ty == Type::Float)
                {
                    self.err(
                        Diagnostic::warning("M122", format!("`{fname}` over float values can differ in the last digits between runs"))
                            .label(e.span, "parallel summation order is not fixed")
                            .help("use a decimal column (declare `decimal(p, s)` or `to_decimal(x, p, s)`) for reproducible totals"),
                    );
                }
                Some(TExpr::new(
                    TExprKind::Agg {
                        func,
                        arg: targs.into_iter().next().map(Box::new),
                    },
                    ty,
                ))
            }
            Ok(Resolved::Scalar(..)) => unreachable!("scalar handled by caller"),
            Err(fe) => {
                let span = fe.arg.and_then(|i| args.get(i)).map_or(e.span, |a| a.span);
                self.err(Diagnostic::error("M108", fe.message).label(span, "here"));
                None
            }
        }
    }

    /// `CASE WHEN input IN (...) THEN value ... ELSE otherwise END` for a mapping.
    pub(super) fn mapping_case(
        &mut self,
        input: TExpr,
        input_span: Span,
        arms: &[ast::MapArm],
        scope: Option<&Scope>,
    ) -> Option<TExpr> {
        let empty = Scope::default();
        let scope = scope.unwrap_or(&empty);
        let mut out = Vec::new();
        let mut ty = Type::Null;
        let mut nullable = false;
        let mut otherwise = None;
        let mut seen: Vec<(Lit, Span)> = Vec::new();
        let mut failed = false;
        for arm in arms {
            let value = match &arm.value {
                ast::MapValue::Original => input.clone(),
                ast::MapValue::Expr(v) => {
                    match self.expr(v, scope, &AggMode::Forbidden("a mapping")) {
                        Some(t) => t,
                        None => {
                            failed = true;
                            continue;
                        }
                    }
                }
            };
            let vspan = match &arm.value {
                ast::MapValue::Expr(v) => v.span,
                ast::MapValue::Original => arm.span,
            };
            ty = self.unify_at(ty, value.ty.ty, vspan)?;
            nullable |= value.ty.nullable;
            match &arm.pattern {
                ast::MapPattern::Otherwise => {
                    if otherwise.is_some() {
                        self.err(
                            Diagnostic::error("M109", "mapping has more than one `otherwise`")
                                .label(arm.span, "second `otherwise`"),
                        );
                        failed = true;
                    }
                    otherwise = Some(Box::new(value));
                }
                ast::MapPattern::Values(vs) => {
                    let mut list = Vec::new();
                    for v in vs {
                        let Some(t) = self.expr(v, &empty, &AggMode::Forbidden("a mapping")) else {
                            failed = true;
                            continue;
                        };
                        let TExprKind::Literal(l) = &t.kind else {
                            self.err(
                                Diagnostic::error(
                                    "M109",
                                    "mapping patterns must be literal values",
                                )
                                .label(v.span, "not a literal"),
                            );
                            failed = true;
                            continue;
                        };
                        if !Type::comparable(input.ty.ty, t.ty.ty) {
                            self.err(
                                Diagnostic::error("M109", format!("pattern type `{}` does not match the mapped column type `{}`", t.ty.ty, input.ty.ty))
                                    .label(v.span, "pattern")
                                    .label(input_span, "mapped value"),
                            );
                            failed = true;
                            continue;
                        }
                        if let Some((_, prev)) = seen.iter().find(|(x, _)| x == l) {
                            let prev = *prev;
                            self.err(
                                Diagnostic::error("M109", "value is mapped twice")
                                    .label(v.span, "duplicate pattern")
                                    .label(prev, "first mapped here"),
                            );
                            failed = true;
                            continue;
                        }
                        seen.push((l.clone(), v.span));
                        list.push(t);
                    }
                    if list.is_empty() {
                        continue;
                    }
                    let cond = TExpr::new(
                        TExprKind::InList {
                            expr: Box::new(input.clone()),
                            list,
                            negated: false,
                        },
                        ColType::new(Type::Bool, input.ty.nullable),
                    );
                    out.push((cond, value));
                }
            }
        }
        if failed {
            return None;
        }
        if otherwise.is_none() {
            nullable = true;
        }
        Some(TExpr::new(
            TExprKind::Case {
                arms: out,
                otherwise,
            },
            ColType::new(ty, nullable),
        ))
    }
}

impl functions::FnError {
    fn message_is_unknown(&self, name: &str) -> bool {
        self.message == format!("unknown function `{name}`")
    }
}

fn valid_iso_date(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    let [y, m, d] = parts.as_slice() else {
        return false;
    };
    let (Ok(y), Ok(m), Ok(d)) = (y.parse::<i32>(), m.parse::<u32>(), d.parse::<u32>()) else {
        return false;
    };
    if y.to_string().len() != 4 && !(y > 0 && s.starts_with(&format!("{y:04}"))) {
        return false;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    (1..=12).contains(&m) && d >= 1 && d <= days[m as usize - 1]
}

fn int_as_decimal(t: Type) -> Option<(u8, u8)> {
    match t {
        Type::Int => Some((19, 0)),
        Type::Decimal(p, s) => Some((p, s)),
        _ => None,
    }
}

fn arith(op: BinaryOp, a: Type, b: Type) -> Type {
    use Type::*;
    match (a, b) {
        (Unknown, _) | (_, Unknown) => Unknown,
        (Null, x) | (x, Null) => x,
        (Float, _) | (_, Float) => Float,
        (Int, Int) if op != BinaryOp::Div => Int,
        _ if op == BinaryOp::Div => Float,
        _ => {
            let (p1, s1) = int_as_decimal(a).unwrap_or((18, 0));
            let (p2, s2) = int_as_decimal(b).unwrap_or((18, 0));
            match op {
                BinaryOp::Mul => {
                    let s = (s1 + s2).min(38);
                    Decimal((p1 as u16 + p2 as u16).min(38) as u8, s)
                }
                _ => {
                    let s = s1.max(s2);
                    let int = (p1 - s1).max(p2 - s2);
                    Decimal((int as u16 + s as u16 + 1).min(38) as u8, s)
                }
            }
        }
    }
}

type TypeError = (String, Option<String>);

pub fn binary_type(op: BinaryOp, l: ColType, r: ColType) -> Result<ColType, TypeError> {
    use Type::*;
    let nullable = l.nullable || r.nullable;
    let (a, b) = (l.ty, r.ty);
    let loose = |t: Type| matches!(t, Unknown | Null);
    let ty = match op {
        BinaryOp::And | BinaryOp::Or => {
            if (a == Bool || loose(a)) && (b == Bool || loose(b)) {
                Bool
            } else {
                return Err((
                    format!(
                        "`{}` needs conditions on both sides, found `{a}` and `{b}`",
                        op.symbol()
                    ),
                    None,
                ));
            }
        }
        op if op.is_comparison() => {
            if Type::comparable(a, b) {
                Bool
            } else {
                let help = match (a, b) {
                    (Date | Timestamp, String) | (String, Date | Timestamp) => {
                        Some("write date literals as `date(\"2026-01-31\")`, or parse text with `parse_date(...)`".to_string())
                    }
                    (String, x) | (x, String) if x.is_numeric() => Some("convert the text first, e.g. `parse_number(text)`".to_string()),
                    _ => None,
                };
                return Err((format!("cannot compare `{a}` with `{b}`"), help));
            }
        }
        BinaryOp::Add | BinaryOp::Sub => match (a, b) {
            (x, y) if x.is_numeric() && y.is_numeric() => arith(op, x, y),
            (Date, Int) => Date,
            (Int, Date) if op == BinaryOp::Add => Date,
            (Date, Date) if op == BinaryOp::Sub => Int,
            (String, String) if op == BinaryOp::Add => {
                return Err((
                    "`+` does not join text".into(),
                    Some("use `concat(a, b)`".into()),
                ));
            }
            (Date, _) | (_, Date) => {
                return Err((
                    format!("cannot apply `{}` to `{a}` and `{b}`", op.symbol()),
                    Some("use `add_days(date, n)` or `date_diff(a, b)`".into()),
                ));
            }
            _ => {
                return Err((
                    format!("cannot apply `{}` to `{a}` and `{b}`", op.symbol()),
                    None,
                ));
            }
        },
        BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
            if a.is_numeric() && b.is_numeric() {
                arith(op, a, b)
            } else {
                return Err((
                    format!("cannot apply `{}` to `{a}` and `{b}`", op.symbol()),
                    None,
                ));
            }
        }
        _ => unreachable!(),
    };
    // division by zero yields null
    let nullable = nullable || op == BinaryOp::Div || op == BinaryOp::Mod;
    Ok(ColType::new(ty, nullable))
}
