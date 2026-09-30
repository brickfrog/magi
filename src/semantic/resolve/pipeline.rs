//! Dataset pipelines: `head |> step |> step ...` lowered to logical plans.

use crate::ast::{self, BinaryOp, ExprKind, JoinOn, Literal, PipelineHead, StepKind};
use crate::diagnostic::Diagnostic;
use crate::plan::logical::{JoinType, LogicalPlan};
use crate::semantic::hir::*;
use crate::semantic::types::{ColType, Type};
use crate::syntax::span::Span;

use super::Analyzer;
use super::expr::{AggMode, SCol, Scope};

pub struct PipelineResult {
    pub plan: LogicalPlan,
    pub columns: Vec<Column>,
    pub identity: Option<Vec<String>>,
    pub sort: Vec<(String, bool)>,
    pub uses: Vec<String>,
    pub backend_specific: bool,
    pub open: bool,
    /// Open because of a `native_sql` result without `schema`: the columns stay unknown even
    /// when the program runs (unlike an SQL source that `magi check` does not contact).
    pub native_open: bool,
}

struct State {
    plan: LogicalPlan,
    scope: Scope,
    /// Identity columns (phys names) while they are known to survive.
    identity: Option<Vec<String>>,
    /// Sort keys (phys) if the relation is currently ordered.
    sort: Option<Vec<(String, bool)>>,
    uses: Vec<String>,
    backend_specific: bool,
    pending_group: Option<(Vec<(u8, String)>, Span)>,
    /// Name of the pipeline's input (the dataset's name for `native_sql`): the `<input>` of
    /// `<input>_<column>` when a join renames a column made inside the pipeline.
    input: String,
    /// The open inputs are `native_sql` results without `schema` (see `PipelineResult`).
    native_open: bool,
}

fn passthrough(scope: &Scope) -> Vec<(TExpr, String)> {
    scope
        .cols
        .iter()
        .map(|c| (TExpr::col(0, c.phys.clone(), c.ty), c.phys.clone()))
        .collect()
}

/// `want`, or `want_2`, `want_3`, ... if a column of `scope` already has that physical name.
/// DuckDB names are case-insensitive, so the comparison is too.
fn unique_phys(scope: &Scope, want: &str) -> String {
    let taken = |n: &str| scope.cols.iter().any(|c| c.phys.eq_ignore_ascii_case(n));
    if !taken(want) {
        return want.to_string();
    }
    (2..)
        .map(|i| format!("{want}_{i}"))
        .find(|n| !taken(n))
        .unwrap()
}

/// Same name apart from letter case: DuckDB would treat the two as one column.
fn case_twin(a: &str, b: &str) -> bool {
    a != b && a.eq_ignore_ascii_case(b)
}

/// Unqualified column names referenced by an AST expression.
fn referenced_names(e: &ast::Expr) -> Vec<String> {
    let mut out = Vec::new();
    e.walk(&mut |x| {
        if let ExprKind::Column(c) = &x.kind
            && c.qualifier.is_none()
        {
            out.push(c.name.name.clone());
        }
    });
    out
}

impl<'a> Analyzer<'a> {
    /// A new column name that differs from an existing column only in letter case.
    fn case_clash(&mut self, name: &str, span: Span, existing: &str) {
        self.err(
            Diagnostic::error(
                "M003",
                format!("column `{name}` clashes with column `{existing}`"),
            )
            .label(span, "column names are case-insensitive")
            .help(format!(
                "write `{existing}` to replace that column, or choose another name"
            )),
        );
    }

    /// A step that lists every column it keeps, over an input whose columns are unknown.
    fn unknown_columns(&mut self, step: &str, span: Span) {
        self.err(
            Diagnostic::error(
                "M124",
                format!("`{step}` needs to know every column of its input"),
            )
            .label(span, "the input's columns are unknown; they would be dropped")
            .help("declare the columns of the `native_sql` block with `schema { name: type ... }`, or `select` the columns you need first"),
        );
    }

    pub(super) fn pipeline(
        &mut self,
        p: &'a ast::Pipeline,
        dataset: &str,
    ) -> Option<PipelineResult> {
        let natives_before = self.natives_used;
        let mut st = match &p.head {
            PipelineHead::Rel(r) => {
                let rel = self.rel_ref(r)?;
                let relation = self.relation(rel);
                let qualifier = r.part.as_ref().unwrap_or(&r.name).name.clone();
                State {
                    plan: LogicalPlan::scan(r.display()),
                    scope: Scope::single(relation, &qualifier),
                    identity: relation.identity.clone(),
                    sort: None,
                    uses: vec![r.display()],
                    backend_specific: false,
                    pending_group: None,
                    native_open: self.native_open.contains(&r.display()),
                    input: qualifier,
                }
            }
            PipelineHead::NativeSql {
                options,
                schema,
                span,
            } => self.native_sql(options, schema.as_ref(), *span, dataset)?,
        };
        let mut ok = true;
        for step in &p.steps {
            if let Some((_, gspan)) = &st.pending_group
                && !matches!(step.kind, StepKind::Aggregate(_))
            {
                let gspan = *gspan;
                self.err(
                    Diagnostic::error("M113", "`group by` must be followed by `aggregate { ... }`")
                        .label(gspan, "group")
                        .label(step.span, "found this instead"),
                );
                return None;
            }
            if self.step(&mut st, step, dataset).is_none() {
                ok = false;
                break;
            }
        }
        if let Some((_, gspan)) = st.pending_group {
            self.err(
                Diagnostic::error("M113", "`group by` must be followed by `aggregate { ... }`")
                    .label(gspan, "nothing is aggregated"),
            );
            return None;
        }
        if !ok {
            return None;
        }

        // Output names: analyst names; a name several columns share (ignoring case, as DuckDB
        // does) falls back to the column's physical name, numbered if an output already has it.
        let shared = |c: &SCol| {
            st.scope
                .cols
                .iter()
                .filter(|o| o.name.eq_ignore_ascii_case(&c.name))
                .count()
                > 1
        };
        let mut used: Vec<String> = st
            .scope
            .cols
            .iter()
            .filter(|c| !shared(c))
            .map(|c| c.name.clone())
            .collect();
        let mut columns = Vec::new();
        let mut exprs = Vec::new();
        let mut renamed = Vec::new();
        let mut needs_project = false;
        for c in &st.scope.cols {
            let dup = shared(c);
            let out = if dup {
                let free = |n: &str| !used.iter().any(|u| u.eq_ignore_ascii_case(n));
                let out = if free(&c.phys) {
                    c.phys.clone()
                } else {
                    (2..)
                        .map(|n| format!("{}_{n}", c.phys))
                        .find(|n| free(n))
                        .unwrap()
                };
                used.push(out.clone());
                renamed.push(format!(
                    "`{}.{}` -> `{out}`",
                    c.qualifier.as_deref().unwrap_or(&st.input),
                    c.name
                ));
                out
            } else {
                c.name.clone()
            };
            needs_project |= out != c.phys;
            let lineage = self
                .hir
                .lineage
                .add(format!("{dataset}.{out}"), vec![c.lineage]);
            columns.push(Column {
                name: out.clone(),
                ty: c.ty,
                lineage,
            });
            exprs.push((TExpr::col(0, c.phys.clone(), c.ty), out));
        }
        if !renamed.is_empty() {
            self.err(
                Diagnostic::note("M112", format!("dataset `{dataset}` has same-named columns from different inputs; they are renamed"))
                    .label(p.span, renamed.join(", "))
                    .help("select or rename the columns you want to keep explicitly"),
            );
        }
        let out_name = |phys: &str| {
            exprs
                .iter()
                .find(|(e, _)| matches!(&e.kind, TExprKind::Column { name, .. } if name == phys))
                .map(|(_, n)| n.clone())
        };
        let identity = st
            .identity
            .as_ref()
            .and_then(|ids| ids.iter().map(|i| out_name(i)).collect::<Option<Vec<_>>>());
        let sort = st
            .sort
            .as_ref()
            .and_then(|keys| {
                keys.iter()
                    .map(|(k, d)| out_name(k).map(|n| (n, *d)))
                    .collect::<Option<Vec<_>>>()
            })
            .unwrap_or_default();
        let plan = if needs_project {
            st.plan.project(exprs)
        } else {
            st.plan
        };
        Some(PipelineResult {
            plan,
            columns,
            identity,
            sort,
            uses: st.uses,
            backend_specific: st.backend_specific || self.natives_used > natives_before,
            // `select` and `aggregate` name every output column
            open: !st.scope.open.is_empty(),
            native_open: st.native_open && !st.scope.open.is_empty(),
        })
    }

    fn native_sql(
        &mut self,
        options: &[ast::Opt],
        schema: Option<&ast::SchemaBlock>,
        span: Span,
        dataset: &str,
    ) -> Option<State> {
        let mut query = None;
        let mut uses = Vec::new();
        for o in options {
            match o.key.name.as_str() {
                "dialect" => {
                    let ok = matches!(&o.value.kind, ExprKind::Column(c) if c.qualifier.is_none() && c.name.name == "duckdb")
                        || matches!(&o.value.kind, ExprKind::Literal(Literal::Str(s)) if s == "duckdb");
                    if !ok {
                        self.err(
                            Diagnostic::error(
                                "M114",
                                "native SQL is only supported for the DuckDB dialect",
                            )
                            .label(o.value.span, "expected `duckdb`"),
                        );
                        return None;
                    }
                }
                "query" => query = self.opt_str(o),
                "inputs" => {
                    let ExprKind::List(items) = &o.value.kind else {
                        self.err(
                            Diagnostic::error("M114", "`inputs` is a list of relation names")
                                .label(o.value.span, "expected `[a, b]`"),
                        );
                        return None;
                    };
                    for i in items {
                        let ExprKind::Column(c) = &i.kind else {
                            self.err(
                                Diagnostic::error("M114", "expected a relation name")
                                    .label(i.span, "here"),
                            );
                            return None;
                        };
                        let name = c.display();
                        self.ensure(&name, Some(i.span))?;
                        uses.push(name);
                    }
                }
                _ => self.unknown_option(o, &["dialect", "query", "inputs"], "native_sql"),
            }
        }
        let Some(query) = query else {
            self.err(Diagnostic::error("M114", "native_sql needs `query`").label(span, "here"));
            return None;
        };
        self.err(
            Diagnostic::warning(
                "M115",
                format!("dataset `{dataset}` contains DuckDB-specific SQL"),
            )
            .label(span, "not portable; MAGI cannot check or trace inside it")
            .help("list every relation the query reads in `inputs: [...]` so it runs after them"),
        );
        let mut scope = Scope::default();
        match schema {
            Some(s) => {
                for dc in self.declared_schema(s) {
                    let lineage = self
                        .hir
                        .lineage
                        .add(format!("{dataset}.{} (native SQL)", dc.name), Vec::new());
                    scope.cols.push(SCol {
                        qualifier: None,
                        name: dc.name.clone(),
                        phys: dc.name,
                        ty: dc.ty,
                        lineage,
                        slot: 0,
                    });
                }
            }
            None => scope.open.push((None, 0)),
        }
        Some(State {
            plan: LogicalPlan::NativeSql { sql: query },
            scope,
            identity: None,
            sort: None,
            uses,
            backend_specific: true,
            pending_group: None,
            native_open: schema.is_none(),
            input: dataset.to_string(),
        })
    }

    fn step(&mut self, st: &mut State, step: &'a ast::Step, dataset: &str) -> Option<()> {
        // These steps list every column they keep; columns MAGI does not know (a `native_sql`
        // result without `schema`) would silently disappear.
        let keeps_columns = match &step.kind {
            StepKind::Derive(_) => Some("derive"),
            StepKind::Drop(_) => Some("drop"),
            StepKind::Rename(_) => Some("rename"),
            StepKind::Normalize { .. } => Some("normalize"),
            StepKind::Join { .. } => Some("join"),
            StepKind::Union(_) => Some("union"),
            _ => None,
        };
        if let Some(what) = keeps_columns
            && st.native_open
            && !st.scope.open.is_empty()
        {
            self.unknown_columns(what, step.span);
            return None;
        }
        match &step.kind {
            StepKind::Select(items) => {
                let mut new = Scope {
                    open: Vec::new(),
                    ..Scope::default()
                };
                let mut exprs = Vec::new();
                // which items of `new` are named by the analyst (`name = expr`)
                let mut assigned: Vec<bool> = Vec::new();
                for item in items {
                    let (col, expr, name_span) = match item {
                        ast::SelectItem::Column(c) => {
                            let sc = self.lookup(c, &st.scope)?;
                            let e = TExpr::col(0, sc.phys.clone(), sc.ty);
                            (
                                SCol {
                                    name: c.name.name.clone(),
                                    ..sc
                                },
                                e,
                                c.span,
                            )
                        }
                        ast::SelectItem::Assign(a) => {
                            if !self.not_reserved(&a.name) {
                                return None;
                            }
                            let t = self.expr(&a.expr, &st.scope, &AggMode::Forbidden("select"))?;
                            let deps = self.deps(&t, &st.scope);
                            let lineage = self.hir.lineage.add(
                                format!("{dataset}: {} = {}", a.name.name, self.text(a.expr.span)),
                                deps,
                            );
                            (
                                SCol {
                                    qualifier: None,
                                    name: a.name.name.clone(),
                                    phys: String::new(),
                                    ty: t.ty,
                                    lineage,
                                    slot: 0,
                                },
                                t,
                                a.name.span,
                            )
                        }
                    };
                    let is_assign = matches!(item, ast::SelectItem::Assign(_));
                    if let Some(prev) = new.cols.iter().zip(&assigned).find_map(|(c, a)| {
                        ((is_assign || *a) && case_twin(&c.name, &col.name)).then_some(&c.name)
                    }) {
                        let prev = prev.clone();
                        self.case_clash(&col.name, name_span, &prev);
                        return None;
                    }
                    if new
                        .cols
                        .iter()
                        .any(|c| c.name == col.name && c.qualifier == col.qualifier)
                    {
                        self.err(
                            Diagnostic::error(
                                "M003",
                                format!("column `{}` is selected twice", col.name),
                            )
                            .label(name_span, "duplicate"),
                        );
                        return None;
                    }
                    let phys = unique_phys(&new, &col.name);
                    exprs.push((expr, phys.clone()));
                    new.cols.push(SCol { phys, ..col });
                    assigned.push(is_assign);
                }
                st.identity = st.identity.take().and_then(|ids| {
                    ids.iter().map(|i| exprs.iter().find(|(e, _)| matches!(&e.kind, TExprKind::Column { name, .. } if name == i)).map(|(_, n)| n.clone())).collect()
                });
                st.sort = None;
                st.plan = std::mem::replace(&mut st.plan, LogicalPlan::scan("")).project(exprs);
                st.scope = new;
            }
            StepKind::Drop(cols) => {
                for c in cols {
                    let sc = self.lookup(c, &st.scope)?;
                    st.scope.cols.retain(|x| x.phys != sc.phys);
                    if st
                        .identity
                        .as_ref()
                        .is_some_and(|ids| ids.contains(&sc.phys))
                    {
                        st.identity = None;
                    }
                }
                if st.scope.cols.is_empty() && st.scope.open.is_empty() {
                    self.err(
                        Diagnostic::error("M116", "`drop` removes every column")
                            .label(step.span, "nothing left"),
                    );
                    return None;
                }
                st.sort = None;
                st.plan = std::mem::replace(&mut st.plan, LogicalPlan::scan(""))
                    .project(passthrough(&st.scope));
            }
            StepKind::Rename(renames) => {
                let mut exprs = passthrough(&st.scope);
                for r in renames {
                    if !self.not_reserved(&r.to) {
                        return None;
                    }
                    let sc = self.lookup(&r.from, &st.scope)?;
                    if let Some(twin) = st
                        .scope
                        .cols
                        .iter()
                        .find(|c| case_twin(&c.name, &r.to.name) && c.phys != sc.phys)
                    {
                        let twin = twin.name.clone();
                        self.case_clash(&r.to.name, r.to.span, &twin);
                        return None;
                    }
                    if st
                        .scope
                        .cols
                        .iter()
                        .any(|c| c.name == r.to.name && c.phys != sc.phys)
                    {
                        self.err(
                            Diagnostic::error(
                                "M003",
                                format!("column `{}` already exists", r.to.name),
                            )
                            .label(r.to.span, "name taken"),
                        );
                        return None;
                    }
                    let new_phys = if st
                        .scope
                        .cols
                        .iter()
                        .any(|c| c.phys.eq_ignore_ascii_case(&r.to.name) && c.phys != sc.phys)
                    {
                        unique_phys(&st.scope, &r.to.name)
                    } else {
                        r.to.name.clone()
                    };
                    let idx = st
                        .scope
                        .cols
                        .iter()
                        .position(|c| c.phys == sc.phys)
                        .unwrap();
                    exprs[idx].1 = new_phys.clone();
                    if let Some(ids) = &mut st.identity {
                        for i in ids.iter_mut() {
                            if *i == sc.phys {
                                *i = new_phys.clone();
                            }
                        }
                    }
                    if let Some(keys) = &mut st.sort {
                        for (k, _) in keys.iter_mut() {
                            if *k == sc.phys {
                                *k = new_phys.clone();
                            }
                        }
                    }
                    let col = &mut st.scope.cols[idx];
                    col.name = r.to.name.clone();
                    col.phys = new_phys;
                    col.qualifier = None;
                }
                st.plan = std::mem::replace(&mut st.plan, LogicalPlan::scan("")).project(exprs);
            }
            StepKind::Derive(assigns) => {
                let mut batch: Vec<(String, TExpr, LineageId, Span)> = Vec::new();
                for a in assigns {
                    if referenced_names(&a.expr)
                        .iter()
                        .any(|n| batch.iter().any(|(b, ..)| b == n))
                    {
                        self.flush_derive(st, std::mem::take(&mut batch))?;
                    }
                    if batch.iter().any(|(b, ..)| b == &a.name.name) {
                        self.err(
                            Diagnostic::error(
                                "M003",
                                format!("`{}` is derived twice in one step", a.name.name),
                            )
                            .label(a.name.span, "duplicate"),
                        );
                        return None;
                    }
                    if !self.not_reserved(&a.name) {
                        return None;
                    }
                    let t = self.expr(&a.expr, &st.scope, &AggMode::Forbidden("derive"))?;
                    let deps = self.deps(&t, &st.scope);
                    let lineage = self.hir.lineage.add(
                        format!("{dataset}: {} = {}", a.name.name, self.text(a.expr.span)),
                        deps,
                    );
                    batch.push((a.name.name.clone(), t, lineage, a.name.span));
                }
                self.flush_derive(st, batch)?;
            }
            StepKind::Filter(e) => {
                let t = self.expr(e, &st.scope, &AggMode::Forbidden("filter"))?;
                if !self.expect_bool(&t, e.span, "`filter`") {
                    return None;
                }
                refine_nulls(&mut st.scope, &t);
                st.plan = std::mem::replace(&mut st.plan, LogicalPlan::scan("")).filter(t);
            }
            StepKind::Join {
                kind,
                rel,
                alias,
                on,
            } => self.join(st, *kind, rel, alias.as_ref(), on, dataset)?,
            StepKind::Group(keys) => {
                let mut out = Vec::new();
                for k in keys {
                    let ExprKind::Column(c) = &k.kind else {
                        self.err(
                            Diagnostic::error("M113", "`group by` takes column names")
                                .label(k.span, "expression")
                                .help(
                                    "derive the value first: `|> derive key = ... |> group by key`",
                                ),
                        );
                        return None;
                    };
                    let sc = self.lookup(c, &st.scope)?;
                    out.push((0u8, sc.phys));
                }
                st.pending_group = Some((out, step.span));
            }
            StepKind::Aggregate(assigns) => {
                let keys = st.pending_group.take().map(|(k, _)| k).unwrap_or_default();
                let mode = AggMode::Group(keys.clone());
                let mut new = Scope::default();
                let mut group = Vec::new();
                for (_, phys) in &keys {
                    let c = st
                        .scope
                        .cols
                        .iter()
                        .find(|c| &c.phys == phys)
                        .unwrap()
                        .clone();
                    group.push((TExpr::col(0, c.phys.clone(), c.ty), c.phys.clone()));
                    new.cols.push(c);
                }
                let mut aggs = Vec::new();
                for a in assigns {
                    if !self.not_reserved(&a.name) {
                        return None;
                    }
                    let t = self.expr(&a.expr, &st.scope, &mode)?;
                    if new.cols.iter().any(|c| c.name == a.name.name) {
                        self.err(
                            Diagnostic::error(
                                "M003",
                                format!("column `{}` already exists", a.name.name),
                            )
                            .label(a.name.span, "duplicate name"),
                        );
                        return None;
                    }
                    if let Some(twin) = new.cols.iter().find(|c| case_twin(&c.name, &a.name.name)) {
                        let twin = twin.name.clone();
                        self.case_clash(&a.name.name, a.name.span, &twin);
                        return None;
                    }
                    let deps = self.deps(&t, &st.scope);
                    let lineage = self.hir.lineage.add(
                        format!("{dataset}: {} = {}", a.name.name, self.text(a.expr.span)),
                        deps,
                    );
                    let phys = unique_phys(&new, &a.name.name);
                    new.cols.push(SCol {
                        qualifier: None,
                        name: a.name.name.clone(),
                        phys: phys.clone(),
                        ty: t.ty,
                        lineage,
                        slot: 0,
                    });
                    aggs.push((t, phys));
                }
                st.identity = if group.is_empty() {
                    None
                } else {
                    Some(group.iter().map(|(_, n)| n.clone()).collect())
                };
                st.sort = None;
                st.plan =
                    std::mem::replace(&mut st.plan, LogicalPlan::scan("")).aggregate(group, aggs);
                st.scope = new;
            }
            StepKind::Sort(keys) => {
                let mut out = Vec::new();
                let mut phys_keys = Vec::new();
                for k in keys {
                    let ExprKind::Column(c) = &k.expr.kind else {
                        self.err(
                            Diagnostic::error("M117", "`sort` takes column names")
                                .label(k.expr.span, "expression")
                                .help("derive the value first, then sort by it"),
                        );
                        return None;
                    };
                    let sc = self.lookup(c, &st.scope)?;
                    out.push(OrderKey {
                        expr: TExpr::col(0, sc.phys.clone(), sc.ty),
                        desc: k.desc,
                    });
                    phys_keys.push((sc.phys, k.desc));
                }
                // every other column breaks ties, so the order is total and reproducible
                for c in &st.scope.cols {
                    if !phys_keys.iter().any(|(p, _)| p == &c.phys) {
                        out.push(OrderKey {
                            expr: TExpr::col(0, c.phys.clone(), c.ty),
                            desc: false,
                        });
                    }
                }
                st.sort = Some(phys_keys);
                st.plan = std::mem::replace(&mut st.plan, LogicalPlan::scan("")).sort(out);
            }
            StepKind::Distinct => {
                st.plan = std::mem::replace(&mut st.plan, LogicalPlan::scan("")).distinct();
                st.sort = None;
            }
            StepKind::Limit(n) => {
                if st.sort.is_none() {
                    self.err(
                        Diagnostic::error(
                            "M118",
                            "`limit` without `sort` would keep arbitrary rows",
                        )
                        .label(step.span, "nondeterministic")
                        .help("sort first: `|> sort amount desc |> limit 10`"),
                    );
                    return None;
                }
                st.plan = LogicalPlan::Limit {
                    input: Box::new(std::mem::replace(&mut st.plan, LogicalPlan::scan(""))),
                    n: *n,
                };
            }
            StepKind::Union(rel) => {
                let other = self.rel_ref(rel)?;
                let other_rel = self.relation(other).clone();
                st.uses.push(rel.display());
                let names: Vec<&str> = st.scope.cols.iter().map(|c| c.name.as_str()).collect();
                if names
                    .iter()
                    .enumerate()
                    .any(|(i, n)| names[..i].iter().any(|p| p.eq_ignore_ascii_case(n)))
                {
                    self.err(
                        Diagnostic::error("M119", "`union` needs unique column names on the left")
                            .label(step.span, "rename or drop duplicates first"),
                    );
                    return None;
                }
                let missing: Vec<&str> = names
                    .iter()
                    .copied()
                    .filter(|n| other_rel.column(n).is_none())
                    .collect();
                let extra: Vec<&str> = other_rel
                    .columns
                    .iter()
                    .map(|c| c.name.as_str())
                    .filter(|n| !names.contains(n))
                    .collect();
                if (!missing.is_empty() || !extra.is_empty())
                    && !other_rel.open
                    && st.scope.open.is_empty()
                {
                    let mut msg = Vec::new();
                    if !missing.is_empty() {
                        msg.push(format!("missing on the right: {}", missing.join(", ")));
                    }
                    if !extra.is_empty() {
                        msg.push(format!("only on the right: {}", extra.join(", ")));
                    }
                    self.err(
                        Diagnostic::error(
                            "M119",
                            format!("`union` inputs have different columns ({})", msg.join("; ")),
                        )
                        .label(rel.span, "union matches columns by name")
                        .help("select/rename so both sides have the same column names"),
                    );
                    return None;
                }
                let mut left = Vec::new();
                let mut right = Vec::new();
                let mut new = Scope::default();
                for c in &st.scope.cols {
                    let rc = other_rel.column(&c.name);
                    let ty = match rc {
                        Some(rc) => {
                            match Type::unify(c.ty.ty, rc.ty.ty) {
                                Some(t) => ColType::new(t, c.ty.nullable || rc.ty.nullable),
                                None => {
                                    self.err(
                                    Diagnostic::error("M119", format!("column `{}` is `{}` on the left and `{}` on the right", c.name, c.ty.ty, rc.ty.ty))
                                        .label(rel.span, "incompatible types"),
                                );
                                    return None;
                                }
                            }
                        }
                        None => c.ty,
                    };
                    left.push((TExpr::col(0, c.phys.clone(), c.ty), c.name.clone()));
                    right.push((TExpr::col(0, c.name.clone(), ty), c.name.clone()));
                    let lineage = self.hir.lineage.add(
                        format!("{dataset}: {} (union)", c.name),
                        [Some(c.lineage), rc.map(|r| r.lineage)]
                            .into_iter()
                            .flatten()
                            .collect(),
                    );
                    new.cols.push(SCol {
                        qualifier: None,
                        name: c.name.clone(),
                        phys: c.name.clone(),
                        ty,
                        lineage,
                        slot: 0,
                    });
                }
                let left_plan =
                    std::mem::replace(&mut st.plan, LogicalPlan::scan("")).project(left);
                let right_plan = LogicalPlan::scan(rel.display()).project(right);
                st.plan = LogicalPlan::Union {
                    inputs: vec![left_plan, right_plan],
                };
                new.open = st.scope.open.clone();
                st.scope = new;
                st.identity = None;
                st.sort = None;
            }
            StepKind::Normalize { column, mapping } => {
                let sc = self.lookup(column, &st.scope)?;
                let input = TExpr::col(0, sc.phys.clone(), sc.ty);
                let t = match mapping {
                    ast::MappingRef::Inline(arms) => {
                        self.mapping_case(input, column.span, arms, Some(&st.scope.clone()))?
                    }
                    ast::MappingRef::Named(n) => {
                        let decl = self.mapping(n)?;
                        self.mapping_case(input, column.span, &decl.arms, None)?
                    }
                };
                let deps = self.deps(&t, &st.scope);
                let using = match mapping {
                    ast::MappingRef::Inline(_) => "an inline mapping".to_string(),
                    ast::MappingRef::Named(n) => format!("mapping {}", n.name),
                };
                let lineage = self.hir.lineage.add(
                    format!("{dataset}: normalize {} using {using}", sc.name),
                    deps,
                );
                let idx = st
                    .scope
                    .cols
                    .iter()
                    .position(|c| c.phys == sc.phys)
                    .unwrap();
                let mut exprs = passthrough(&st.scope);
                exprs[idx].0 = t.clone();
                let col = &mut st.scope.cols[idx];
                col.ty = t.ty;
                col.lineage = lineage;
                if st
                    .identity
                    .as_ref()
                    .is_some_and(|ids| ids.contains(&sc.phys))
                {
                    st.identity = None;
                }
                st.sort = None;
                st.plan = std::mem::replace(&mut st.plan, LogicalPlan::scan("")).project(exprs);
            }
        }
        Some(())
    }

    fn flush_derive(
        &mut self,
        st: &mut State,
        batch: Vec<(String, TExpr, LineageId, Span)>,
    ) -> Option<()> {
        if batch.is_empty() {
            return Some(());
        }
        let mut exprs = passthrough(&st.scope);
        let mut scope = st.scope.clone();
        for (name, t, lineage, span) in batch {
            if !scope.cols.iter().any(|c| c.name == name)
                && let Some(twin) = scope.cols.iter().find(|c| case_twin(&c.name, &name))
            {
                let twin = twin.name.clone();
                self.case_clash(&name, span, &twin);
                return None;
            }
            let existing: Vec<usize> = st
                .scope
                .cols
                .iter()
                .enumerate()
                .filter(|(_, c)| c.name == name)
                .map(|(i, _)| i)
                .collect();
            match existing.as_slice() {
                [i] => {
                    exprs[*i].0 = t.clone();
                    let c = &mut scope.cols[*i];
                    c.ty = t.ty;
                    c.lineage = lineage;
                    if st
                        .identity
                        .as_ref()
                        .is_some_and(|ids| ids.contains(&c.phys))
                    {
                        st.identity = None;
                    }
                    if st
                        .sort
                        .as_ref()
                        .is_some_and(|k| k.iter().any(|(p, _)| p == &c.phys))
                    {
                        st.sort = None;
                    }
                }
                [] => {
                    let phys = unique_phys(&scope, &name);
                    exprs.push((t.clone(), phys.clone()));
                    scope.cols.push(SCol {
                        qualifier: None,
                        name,
                        phys,
                        ty: t.ty,
                        lineage,
                        slot: 0,
                    });
                }
                _ => {
                    self.err(
                        Diagnostic::error(
                            "M011",
                            format!("`{name}` is ambiguous: it exists on more than one input"),
                        )
                        .label(span, "which one to replace?")
                        .help("derive a new name instead, or drop/rename one of the columns first"),
                    );
                    return None;
                }
            }
        }
        st.scope = scope;
        st.plan = std::mem::replace(&mut st.plan, LogicalPlan::scan("")).project(exprs);
        Some(())
    }

    fn join(
        &mut self,
        st: &mut State,
        kind: ast::JoinKind,
        rel: &ast::RelRef,
        alias: Option<&ast::Ident>,
        on: &ast::JoinOn,
        dataset: &str,
    ) -> Option<()> {
        let r = self.rel_ref(rel)?;
        let right_rel = self.relation(r).clone();
        st.uses.push(rel.display());
        let qualifier = alias
            .map(|a| a.name.clone())
            .unwrap_or_else(|| rel.part.as_ref().unwrap_or(&rel.name).name.clone());
        if st
            .scope
            .cols
            .iter()
            .any(|c| c.qualifier.as_deref() == Some(qualifier.as_str()))
        {
            self.err(
                Diagnostic::error(
                    "M120",
                    format!("`{qualifier}` is already an input of this pipeline"),
                )
                .label(rel.span, "joined twice")
                .help(format!(
                    "give it another name: `join {} as other on ...`",
                    rel.display()
                )),
            );
            return None;
        }
        let mut right = Scope::single(&right_rel, &qualifier);
        if !right.open.is_empty() && self.native_open.contains(&rel.display()) {
            self.unknown_columns("join", rel.span);
            return None;
        }
        for c in &mut right.cols {
            c.slot = 1;
        }
        for o in &mut right.open {
            o.1 = 1;
        }
        let mut combined = st.scope.clone();
        combined.cols.extend(right.cols.iter().cloned());
        combined.open.extend(right.open.iter().cloned());

        let (cond, merged_keys) = match on {
            JoinOn::Keys(keys) => {
                let mut cond: Option<TExpr> = None;
                let mut merged = Vec::new();
                for k in keys {
                    let cn = ast::ColumnName {
                        qualifier: None,
                        name: k.clone(),
                        span: k.span,
                    };
                    let l = self.lookup(&cn, &st.scope)?;
                    let rc = self.lookup(&cn, &right)?;
                    if !Type::comparable(l.ty.ty, rc.ty.ty) {
                        self.err(
                            Diagnostic::error(
                                "M105",
                                format!(
                                    "join key `{}` is `{}` on the left and `{}` on the right",
                                    k.name, l.ty.ty, rc.ty.ty
                                ),
                            )
                            .label(k.span, "incompatible types"),
                        );
                        return None;
                    }
                    let eq = TExpr::new(
                        TExprKind::Binary {
                            op: BinaryOp::Eq,
                            left: Box::new(TExpr::col(0, l.phys.clone(), l.ty)),
                            right: Box::new(TExpr::col(1, rc.phys.clone(), rc.ty)),
                        },
                        ColType::new(Type::Bool, l.ty.nullable || rc.ty.nullable),
                    );
                    cond = Some(match cond {
                        None => eq,
                        Some(prev) => TExpr::new(
                            TExprKind::Binary {
                                op: BinaryOp::And,
                                left: Box::new(prev),
                                right: Box::new(eq),
                            },
                            ColType::nullable(Type::Bool),
                        ),
                    });
                    merged.push((l.phys.clone(), rc.phys.clone()));
                }
                (cond.unwrap(), merged)
            }
            JoinOn::Expr(e) => {
                let t = self.expr(e, &combined, &AggMode::Forbidden("a join condition"))?;
                if !self.expect_bool(&t, e.span, "a join condition") {
                    return None;
                }
                // `on true` is an explicit cross join; any other condition that never looks at
                // the joined relation is probably a mistake
                let explicit_cross = matches!(t.kind, TExprKind::Literal(Lit::Bool(true)));
                let cols = t.columns_of(1);
                if cols.is_empty() && right.open.is_empty() && !explicit_cross {
                    self.err(
                        Diagnostic::warning(
                            "M121",
                            "join condition does not mention the joined relation",
                        )
                        .label(e.span, "every row matches every row")
                        .help(format!("compare with `{qualifier}.column`")),
                    );
                }
                (t, Vec::new())
            }
        };
        let jt = match kind {
            ast::JoinKind::Inner => JoinType::Inner,
            ast::JoinKind::Left => JoinType::Left,
            ast::JoinKind::Right => JoinType::Right,
            ast::JoinKind::Full => JoinType::Full,
        };
        let (left_nullable, right_nullable) = match jt {
            JoinType::Left => (false, true),
            JoinType::Right => (true, false),
            JoinType::Full => (true, true),
            _ => (false, false),
        };
        let mut out_cols: Vec<(SCol, TExpr)> = Vec::new();
        for c in &st.scope.cols {
            let merged = merged_keys.iter().find(|(l, _)| l == &c.phys);
            // a merged key of a right join is the right key; of a full join, the left key or
            // (for right-only rows) the right key, so it is null when the key of either side is
            let (expr, ty) = match (merged, jt) {
                (Some((_, rp)), JoinType::Full) => {
                    let rc = right.cols.iter().find(|x| &x.phys == rp).unwrap();
                    let ty = ColType::new(
                        Type::unify(c.ty.ty, rc.ty.ty).unwrap_or(c.ty.ty),
                        c.ty.nullable || rc.ty.nullable,
                    );
                    let e = TExpr::new(
                        TExprKind::Call {
                            func: "coalesce",
                            args: vec![
                                TExpr::col(0, c.phys.clone(), c.ty),
                                TExpr::col(1, rp.clone(), rc.ty),
                            ],
                        },
                        ty,
                    );
                    (e, ty)
                }
                (Some((_, rp)), JoinType::Right) => {
                    let rc = right.cols.iter().find(|x| &x.phys == rp).unwrap();
                    (TExpr::col(1, rp.clone(), rc.ty), rc.ty)
                }
                (Some(_), _) => (TExpr::col(0, c.phys.clone(), c.ty), c.ty),
                (None, _) => (
                    TExpr::col(0, c.phys.clone(), c.ty),
                    c.ty.with_nullable(c.ty.nullable || left_nullable),
                ),
            };
            // a merged key of a right join comes from the right input, of a full join from both
            let lineage = match (merged, jt) {
                (Some((_, rp)), JoinType::Right | JoinType::Full) => {
                    let rc = right.cols.iter().find(|x| &x.phys == rp).unwrap();
                    if jt == JoinType::Right {
                        rc.lineage
                    } else {
                        self.hir.lineage.add(
                            format!("{dataset}: {} (full join key)", c.name),
                            vec![c.lineage, rc.lineage],
                        )
                    }
                }
                _ => c.lineage,
            };
            out_cols.push((
                SCol {
                    ty,
                    slot: 0,
                    lineage,
                    ..c.clone()
                },
                expr,
            ));
        }
        for c in &right.cols {
            if merged_keys.iter().any(|(_, r)| r == &c.phys) {
                continue;
            }
            out_cols.push((
                SCol {
                    ty: c.ty.with_nullable(c.ty.nullable || right_nullable),
                    slot: 0,
                    ..c.clone()
                },
                TExpr::col(1, c.phys.clone(), c.ty),
            ));
        }
        // Physical names must be unique (ignoring case, as DuckDB does): a colliding name gets
        // its input's name as prefix (`<input>_<column>`; a column made in this pipeline counts
        // as the pipeline's input), then a number if that is still taken.
        let snapshot: Vec<String> = out_cols.iter().map(|(c, _)| c.phys.clone()).collect();
        let mut used: Vec<String> = Vec::new();
        for (i, (c, _)) in out_cols.iter_mut().enumerate() {
            let taken_by_other = |n: &str| {
                snapshot
                    .iter()
                    .enumerate()
                    .any(|(j, p)| j != i && p.eq_ignore_ascii_case(n))
            };
            let collides = taken_by_other(&c.phys);
            let base = if collides {
                format!("{}_{}", c.qualifier.as_deref().unwrap_or(&st.input), c.name)
            } else {
                c.phys.clone()
            };
            let free = |n: &str| {
                !used.iter().any(|u| u.eq_ignore_ascii_case(n)) && !(collides && taken_by_other(n))
            };
            let phys = if free(&base) {
                base
            } else {
                (2..)
                    .map(|n| format!("{base}_{n}"))
                    .find(|n| free(n))
                    .unwrap()
            };
            used.push(phys.clone());
            c.phys = phys;
        }
        let output: Vec<(TExpr, String)> = out_cols
            .iter()
            .map(|(c, e)| (e.clone(), c.phys.clone()))
            .collect();
        let mut new = Scope {
            cols: out_cols.into_iter().map(|(c, _)| c).collect(),
            open: st.scope.open.clone(),
            ..Scope::default()
        };
        new.open
            .extend(right.open.iter().map(|(q, _)| (q.clone(), 0)));
        let right_plan = LogicalPlan::scan(rel.display());
        st.plan = std::mem::replace(&mut st.plan, LogicalPlan::scan("")).join(
            right_plan,
            jt,
            Some(cond),
            output,
        );
        st.scope = new;
        st.identity = None;
        st.sort = None;
        Some(())
    }
}

/// After `filter`, columns that the predicate requires to be non-null are no longer nullable.
fn refine_nulls(scope: &mut Scope, pred: &TExpr) {
    let mut conjuncts = Vec::new();
    fn split<'e>(e: &'e TExpr, out: &mut Vec<&'e TExpr>) {
        if let TExprKind::Binary {
            op: BinaryOp::And,
            left,
            right,
        } = &e.kind
        {
            split(left, out);
            split(right, out);
        } else {
            out.push(e);
        }
    }
    split(pred, &mut conjuncts);
    let mut non_null = Vec::new();
    for c in conjuncts {
        match &c.kind {
            TExprKind::IsNull {
                expr,
                negated: true,
            } => {
                if let TExprKind::Column { name, .. } = &expr.kind {
                    non_null.push(name.clone());
                }
            }
            TExprKind::Binary { op, left, right } if op.is_comparison() => {
                for side in [left, right] {
                    if let TExprKind::Column { name, .. } = &side.kind {
                        non_null.push(name.clone());
                    }
                }
            }
            TExprKind::InList {
                expr,
                negated: false,
                ..
            } => {
                if let TExprKind::Column { name, .. } = &expr.kind {
                    non_null.push(name.clone());
                }
            }
            _ => {}
        }
    }
    for c in &mut scope.cols {
        if non_null.contains(&c.phys) {
            c.ty.nullable = false;
        }
    }
}
