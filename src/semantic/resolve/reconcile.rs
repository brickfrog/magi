//! Resolution of `reconcile` declarations into the reconciliation model.

use crate::ast::{self, BlockKey, BlockSpec, ReconcileItem, TierItem};
use crate::diagnostic::{Diagnostic, did_you_mean};
use crate::reconcile::model::*;
use crate::semantic::hir::*;
use crate::semantic::types::{ColType, Type};
use crate::syntax::span::Span;

use super::Analyzer;
use super::expr::{AggMode, SCol, Scope};

struct Ctx {
    scope: Scope,
    a_alias: String,
    b_alias: String,
}

impl Ctx {
    fn side_slot(&self, id: &ast::Ident) -> Option<u8> {
        let is = |n: &str| id.name.eq_ignore_ascii_case(n);
        if is("a") || is(&self.a_alias) {
            Some(0)
        } else if is("b") || is(&self.b_alias) {
            Some(1)
        } else {
            None
        }
    }
    fn col(&self, slot: u8, name: &str) -> Option<&SCol> {
        self.scope
            .cols
            .iter()
            .find(|c| c.slot == slot && c.name.eq_ignore_ascii_case(name))
    }
}

impl<'a> Analyzer<'a> {
    pub(super) fn reconcile(&mut self, d: &'a ast::ReconcileDecl) -> bool {
        let (Some(ai), Some(bi)) = (self.rel_ref(&d.a), self.rel_ref(&d.b)) else {
            return false;
        };
        let a_rel = self.relation(ai).clone();
        let b_rel = self.relation(bi).clone();
        for (rel, r) in [(&a_rel, &d.a), (&b_rel, &d.b)] {
            if rel.open {
                self.err(
                    Diagnostic::error(
                        "M300",
                        format!("reconciliation needs the columns of `{}`", rel.name),
                    )
                    .label(r.span, "schema unknown")
                    .help("declare the source schema or run `magi check --sources`"),
                );
                return false;
            }
        }
        let a_alias = d
            .a_alias
            .as_ref()
            .map_or("a".to_string(), |x| x.name.clone());
        let b_alias = d
            .b_alias
            .as_ref()
            .map_or("b".to_string(), |x| x.name.clone());
        if a_alias == b_alias {
            self.err(
                Diagnostic::error("M300", "both sides have the same name")
                    .label(d.span, "use `as` to name them differently"),
            );
            return false;
        }
        let mut scope = Scope::default();
        for (slot, rel) in [(0u8, &a_rel), (1u8, &b_rel)] {
            for c in &rel.columns {
                scope.cols.push(SCol {
                    qualifier: None,
                    name: c.name.clone(),
                    phys: c.name.clone(),
                    ty: c.ty,
                    lineage: c.lineage,
                    slot,
                });
            }
        }
        let a_name = d.a.part.as_ref().unwrap_or(&d.a.name).name.clone();
        let b_name = d.b.part.as_ref().unwrap_or(&d.b.name).name.clone();
        let mut names = vec![(a_alias.clone(), 0u8), (b_alias.clone(), 1u8)];
        let mut add = |n: String, s: u8| {
            if !names.iter().any(|(x, _)| *x == n) {
                names.push((n, s));
            }
        };
        add("a".into(), 0);
        add("b".into(), 1);
        if a_name != b_name {
            add(a_name, 0);
            add(b_name, 1);
        }
        scope.slot_names = names;
        let mut ctx = Ctx {
            scope,
            a_alias: a_alias.clone(),
            b_alias: b_alias.clone(),
        };

        let mut cardinality = Cardinality::OneToOne;
        let mut consume = Consume::Both;
        let mut ambiguity = Hold::Hold;
        let mut duplicates = Hold::Hold;
        let mut default_block: Option<Vec<KeyPair>> = None;
        let mut identity_a: Option<Vec<String>> = None;
        let mut identity_b: Option<Vec<String>> = None;
        let mut evidence = Vec::new();
        let mut flags = Vec::new();
        let mut policy = Policy::default();
        let mut ok = true;

        let keyword =
            |this: &mut Self, v: &ast::Ident, allowed: &[&str], what: &str| -> Option<usize> {
                match allowed.iter().position(|x| *x == v.name) {
                    Some(i) => Some(i),
                    None => {
                        let mut diag =
                            Diagnostic::error("M301", format!("unknown {what} `{}`", v.name))
                                .label(v.span, format!("expected one of: {}", allowed.join(", ")));
                        if let Some(s) = did_you_mean(&v.name, allowed.iter().copied()) {
                            diag = diag.help(format!("did you mean `{s}`?"));
                        }
                        this.err(diag);
                        None
                    }
                }
            };

        // reconcile-level clauses
        for item in &d.items {
            match item {
                ReconcileItem::Block(spec, _) => {
                    match self.block(spec, &ctx, [false, false], [&[], &[]]) {
                        Some(b) => default_block = Some(b),
                        None => ok = false,
                    }
                }
                ReconcileItem::Cardinality(v, _) => match keyword(
                    self,
                    v,
                    &["one_to_one", "one_to_many", "many_to_one", "many_to_many"],
                    "cardinality",
                ) {
                    Some(i) => {
                        cardinality = [
                            Cardinality::OneToOne,
                            Cardinality::OneToMany,
                            Cardinality::ManyToOne,
                            Cardinality::ManyToMany,
                        ][i]
                    }
                    None => ok = false,
                },
                ReconcileItem::Consume(v, _) => {
                    match keyword(self, v, &["both", "a", "b", "none"], "consume policy") {
                        Some(i) => {
                            consume = [Consume::Both, Consume::A, Consume::B, Consume::None][i]
                        }
                        None => ok = false,
                    }
                }
                ReconcileItem::Ambiguity(v, _) => {
                    match keyword(self, v, &["hold", "continue"], "ambiguity policy") {
                        Some(i) => ambiguity = [Hold::Hold, Hold::Continue][i],
                        None => ok = false,
                    }
                }
                ReconcileItem::Duplicates(v, _) => {
                    match keyword(self, v, &["hold", "continue"], "duplicates policy") {
                        Some(i) => duplicates = [Hold::Hold, Hold::Continue][i],
                        None => ok = false,
                    }
                }
                ReconcileItem::Identity { side, cols, span } => {
                    let Some(slot) = ctx.side_slot(side) else {
                        self.err(
                            Diagnostic::error(
                                "M301",
                                format!("`{}` is not a side of this reconciliation", side.name),
                            )
                            .label(
                                side.span,
                                format!("expected `a` or `b` ({a_alias}/{b_alias})"),
                            ),
                        );
                        ok = false;
                        continue;
                    };
                    let mut names = Vec::new();
                    for c in cols {
                        match ctx.col(slot, &c.name) {
                            Some(sc) => {
                                if sc.ty.nullable {
                                    self.err(
                                        Diagnostic::warning(
                                            "M206",
                                            format!("identity column `{}` may be null", c.name),
                                        )
                                        .label(
                                            c.span,
                                            "rows with a null identity make the run fail",
                                        ),
                                    );
                                }
                                names.push(sc.name.clone())
                            }
                            None => {
                                self.missing_side_column(slot, &c.name, c.span, &ctx);
                                ok = false;
                            }
                        }
                    }
                    let target = if slot == 0 {
                        &mut identity_a
                    } else {
                        &mut identity_b
                    };
                    if target.is_some() {
                        self.err(
                            Diagnostic::error("M003", "identity declared twice for this side")
                                .label(*span, "duplicate"),
                        );
                        ok = false;
                    }
                    *target = Some(names);
                }
                ReconcileItem::Evidence(assigns, _) => {
                    for a in assigns {
                        match self.expr(
                            &a.expr,
                            &ctx.scope,
                            &AggMode::Forbidden("reconciliation evidence"),
                        ) {
                            Some(t) => {
                                policy.collect(&t);
                                ctx.scope.aliases.push((a.name.name.clone(), t.clone()));
                                evidence.push(Named {
                                    name: a.name.name.clone(),
                                    expr: t,
                                    text: self.text(a.expr.span).to_string(),
                                });
                            }
                            None => ok = false,
                        }
                    }
                }
                ReconcileItem::Flag(f) => {
                    let Some(when) = &f.when else {
                        self.err(
                            Diagnostic::error(
                                "M301",
                                "a reconcile-level flag needs `when <condition>`",
                            )
                            .label(f.span, "no condition")
                            .help("flags without a condition belong inside a tier"),
                        );
                        ok = false;
                        continue;
                    };
                    match self.expr(when, &ctx.scope, &AggMode::Forbidden("a flag")) {
                        Some(t) if self.expect_bool(&t, when.span, "a flag") => {
                            policy.collect(&t);
                            flags.push(Named {
                                name: f.name.name.clone(),
                                expr: t,
                                text: self.text(when.span).to_string(),
                            });
                        }
                        _ => ok = false,
                    }
                }
                ReconcileItem::Tier(_) => {}
            }
        }
        for k in default_block.iter().flatten() {
            policy.collect(&k.a);
            policy.collect(&k.b);
        }

        let mut tiers = Vec::new();
        let mut tier_names: Vec<(String, Span)> = Vec::new();
        for item in &d.items {
            let ReconcileItem::Tier(t) = item else {
                continue;
            };
            if let Some((_, prev)) = tier_names.iter().find(|(n, _)| n == &t.name.name) {
                let prev = *prev;
                self.err(
                    Diagnostic::error("M003", format!("tier `{}` is defined twice", t.name.name))
                        .label(t.name.span, "duplicate")
                        .label(prev, "first here"),
                );
                ok = false;
                continue;
            }
            tier_names.push((t.name.name.clone(), t.name.span));
            match self.tier(t, &ctx, default_block.as_deref(), cardinality, &mut policy) {
                Some(tier) => tiers.push(tier),
                None => ok = false,
            }
        }
        if tiers.is_empty() && ok {
            self.err(
                Diagnostic::error("M301", "a reconciliation needs at least one `tier`")
                    .label(d.name.span, "no tiers"),
            );
            return false;
        }
        if !ok {
            return false;
        }

        // identity
        let mut side = |this: &mut Self,
                        slot: u8,
                        declared: Option<Vec<String>>,
                        rel: &Relation,
                        alias: &str|
         -> Side {
            let identity = match declared.or_else(|| rel.identity.clone()) {
                Some(ids) => Identity::Declared(ids),
                None => {
                    let r = if slot == 0 { &d.a } else { &d.b };
                    this.err(
                        Diagnostic::warning("M306", format!("side `{alias}` (`{}`) has no row identity", rel.name))
                            .label(r.span, "matches will refer to synthetic row numbers")
                            .help(format!(
                                "declare `identity {alias}: <column>` (or `identity <column>` on the source); synthetic numbers are reproducible for identical input but not stable across refreshes"
                            )),
                    );
                    Identity::Synthetic
                }
            };
            let mut cols = policy.columns(usize::from(slot), consume);
            cols.retain(|c| rel.column(c).is_some());
            Side {
                relation: rel.name.clone(),
                alias: alias.to_string(),
                columns: rel.columns.clone(),
                identity,
                policy_columns: cols,
            }
        };
        let a_side = side(self, 0, identity_a, &a_rel, &a_alias);
        let b_side = side(self, 1, identity_b, &b_rel, &b_alias);

        // tier-level evidence and flags become shared `matches` columns
        let mut tier_columns: Vec<TierColumn> = Vec::new();
        let fixed: Vec<String> = evidence
            .iter()
            .chain(flags.iter())
            .map(|n| n.name.clone())
            .collect();
        for t in &tiers {
            let cols = t
                .evidence
                .iter()
                .map(|n| (n.name.clone(), n.expr.ty, false))
                .chain(
                    t.flags
                        .iter()
                        .map(|(n, _, _)| (n.clone(), ColType::nullable(Type::Bool), true)),
                );
            for (name, ty, is_flag) in cols {
                if fixed.iter().any(|f| f.eq_ignore_ascii_case(&name)) {
                    self.err(
                        Diagnostic::error("M307", format!("`{name}` is defined both for the whole reconciliation and in tier `{}`", t.name))
                            .label(t.span, "rename one of them"),
                    );
                    return false;
                }
                // one output column per name regardless of case, as DuckDB would see it
                match tier_columns
                    .iter_mut()
                    .find(|c| c.name.eq_ignore_ascii_case(&name))
                {
                    Some(c) => {
                        if c.is_flag != is_flag {
                            self.err(
                                Diagnostic::error(
                                    "M307",
                                    format!(
                                        "`{name}` is a flag in one tier and evidence in another"
                                    ),
                                )
                                .label(t.span, "here"),
                            );
                            return false;
                        }
                        match Type::unify(c.ty.ty, ty.ty) {
                            Some(u) => c.ty = ColType::nullable(u),
                            None => {
                                self.err(
                                    Diagnostic::error("M307", format!("evidence `{name}` has type `{}` in one tier and `{}` in tier `{}`", c.ty.ty, ty.ty, t.name))
                                        .label(t.span, "incompatible"),
                                );
                                return false;
                            }
                        }
                    }
                    None => tier_columns.push(TierColumn {
                        name,
                        ty: ColType::nullable(ty.ty),
                        is_flag,
                    }),
                }
            }
        }

        if consume != Consume::Both && tiers.len() > 1 {
            let reused = match consume {
                Consume::None => "every row stays available",
                Consume::A => "B rows stay available",
                _ => "A rows stay available",
            };
            self.err(
                Diagnostic::note(
                    "M311",
                    format!("with `consume {}`, {reused} to later tiers", consume.name()),
                )
                .label(d.name.span, "a row may be matched once in each tier")
                .help(
                    "cardinality applies within a tier; a pair matched once is not matched again",
                ),
            );
        }
        if cardinality == Cardinality::ManyToMany && tiers.iter().any(|t| !t.rank.is_empty()) {
            self.err(
                Diagnostic::warning(
                    "M312",
                    "`rank by` has no effect with `cardinality many_to_many`",
                )
                .label(d.name.span, "every candidate is matched")
                .help("remove `rank by`, or choose a cardinality that selects"),
            );
        }
        let rc = Reconcile {
            name: d.name.name.clone(),
            a: a_side,
            b: b_side,
            cardinality,
            consume,
            ambiguity,
            duplicates,
            evidence,
            flags,
            tiers,
            tier_columns,
            span: d.name.span,
        };
        if !self.register_parts(&rc, d) {
            return false;
        }
        let idx = self.hir.reconciles.len();
        self.hir.reconciles.push(rc);
        self.hir.order.push(Node::Reconcile(idx));
        true
    }

    fn missing_side_column(&mut self, slot: u8, name: &str, span: Span, ctx: &Ctx) {
        let alias = if slot == 0 {
            &ctx.a_alias
        } else {
            &ctx.b_alias
        };
        let mut diag = Diagnostic::error(
            "M012",
            format!("column `{name}` does not exist on side `{alias}`"),
        )
        .label(span, "unknown column");
        if let Some(s) = did_you_mean(
            name,
            ctx.scope
                .cols
                .iter()
                .filter(|c| c.slot == slot)
                .map(|c| c.name.as_str()),
        ) {
            diag = diag.help(format!("did you mean `{s}`?"));
        }
        self.err(diag);
    }

    /// Blocking keys as `a`-side / `b`-side expression pairs.
    fn block(
        &mut self,
        spec: &BlockSpec,
        ctx: &Ctx,
        grouped: [bool; 2],
        keys: [&[String]; 2],
    ) -> Option<Vec<KeyPair>> {
        let BlockSpec::Keys(list) = spec else {
            return Some(Vec::new());
        };
        let mut out = Vec::new();
        for k in list {
            let (a, b, text, span) = match k {
                BlockKey::Same(id) => {
                    let ac = ctx.col(0, &id.name).cloned();
                    let bc = ctx.col(1, &id.name).cloned();
                    let (Some(ac), Some(bc)) = (ac, bc) else {
                        for (slot, c) in [(0u8, ctx.col(0, &id.name)), (1u8, ctx.col(1, &id.name))]
                        {
                            if c.is_none() {
                                self.missing_side_column(slot, &id.name, id.span, ctx);
                            }
                        }
                        return None;
                    };
                    (
                        TExpr::col(0, ac.phys, ac.ty),
                        TExpr::col(1, bc.phys, bc.ty),
                        id.name.clone(),
                        id.span,
                    )
                }
                BlockKey::Pair(x, y) => {
                    let tx = self.expr(x, &ctx.scope, &AggMode::Forbidden("a blocking key"))?;
                    let ty = self.expr(y, &ctx.scope, &AggMode::Forbidden("a blocking key"))?;
                    let span = x.span.to(y.span);
                    let side = |t: &TExpr| match (
                        t.columns_of(0).is_empty(),
                        t.columns_of(1).is_empty(),
                    ) {
                        (false, true) => Some(0u8),
                        (true, false) => Some(1u8),
                        _ => None,
                    };
                    let (a, b) = match (side(&tx), side(&ty)) {
                        (Some(0), Some(1)) => (tx, ty),
                        (Some(1), Some(0)) => (ty, tx),
                        _ => {
                            self.err(
                                Diagnostic::error(
                                    "M302",
                                    "each side of a blocking key must read exactly one input",
                                )
                                .label(span, "expected `a.x == b.y`"),
                            );
                            return None;
                        }
                    };
                    (a, b, self.text(span).to_string(), span)
                }
            };
            if !Type::comparable(a.ty.ty, b.ty.ty) {
                self.err(
                    Diagnostic::error(
                        "M105",
                        format!("blocking key compares `{}` with `{}`", a.ty.ty, b.ty.ty),
                    )
                    .label(span, "incompatible types"),
                );
                return None;
            }
            for (slot, e) in [(0usize, &a), (1usize, &b)] {
                if grouped[slot] {
                    let is_key = matches!(&e.kind, TExprKind::Column { name, .. } if keys[slot].contains(name));
                    if !is_key {
                        self.err(
                            Diagnostic::error(
                                "M303",
                                "on a grouped side, blocking keys must be group keys",
                            )
                            .label(span, "not a group key")
                            .help("add the column to `group ... by`"),
                        );
                        return None;
                    }
                }
            }
            out.push(KeyPair { a, b, text });
        }
        Some(out)
    }

    fn tier(
        &mut self,
        t: &'a ast::TierDecl,
        ctx: &Ctx,
        default_block: Option<&[KeyPair]>,
        cardinality: Cardinality,
        policy: &mut Policy,
    ) -> Option<Tier> {
        // shape, grouping and subset first: they decide how expressions are checked
        let mut shape: Option<(bool, bool, Span)> = None;
        let mut groups: [Option<(Vec<String>, Span)>; 2] = [None, None];
        // (slot, max_items, max_subsets, span)
        let mut subset: Option<(usize, u32, u64, Span)> = None;
        for item in &t.items {
            match item {
                TierItem::Shape {
                    a_many,
                    b_many,
                    a_side,
                    b_side,
                    span,
                } => {
                    if ctx.side_slot(a_side) != Some(0) || ctx.side_slot(b_side) != Some(1) {
                        self.err(
                            Diagnostic::error("M304", "write the shape as `many a to one b`, `one a to many b` or `many a to many b`")
                                .label(*span, format!("sides are `{}` then `{}`", ctx.a_alias, ctx.b_alias)),
                        );
                        return None;
                    }
                    if !a_many && !b_many {
                        self.err(
                            Diagnostic::error(
                                "M304",
                                "`one a to one b` is the default; remove this line",
                            )
                            .label(*span, "not a rollup"),
                        );
                        return None;
                    }
                    shape = Some((*a_many, *b_many, *span));
                }
                TierItem::Group { side, keys, span } => {
                    let Some(slot) = ctx.side_slot(side) else {
                        self.err(
                            Diagnostic::error(
                                "M304",
                                format!("`{}` is not a side of this reconciliation", side.name),
                            )
                            .label(side.span, "expected `a` or `b`"),
                        );
                        return None;
                    };
                    let mut names = Vec::new();
                    for k in keys {
                        match ctx.col(slot, &k.name) {
                            Some(c) => names.push(c.phys.clone()),
                            None => {
                                self.missing_side_column(slot, &k.name, k.span, ctx);
                                return None;
                            }
                        }
                    }
                    groups[slot as usize] = Some((names, *span));
                }
                TierItem::Subset {
                    side,
                    max_items,
                    max_subsets,
                    span,
                } => {
                    let Some(slot) = ctx.side_slot(side) else {
                        self.err(
                            Diagnostic::error(
                                "M313",
                                format!("`{}` is not a side of this reconciliation", side.name),
                            )
                            .label(
                                side.span,
                                format!("expected `a` or `b` ({}/{})", ctx.a_alias, ctx.b_alias),
                            ),
                        );
                        return None;
                    };
                    if let Some((_, _, _, prev)) = subset {
                        self.err(
                            Diagnostic::error("M313", "a tier has at most one `subset` clause")
                                .label(*span, "second clause")
                                .label(prev, "first here"),
                        );
                        return None;
                    }
                    let Some(items) = u32::try_from(*max_items)
                        .ok()
                        .filter(|n| (1..=MAX_SUBSET_ITEMS).contains(n))
                    else {
                        self.err(
                            Diagnostic::error(
                                "M313",
                                format!(
                                    "`max_items` must be an integer from 1 to {MAX_SUBSET_ITEMS}"
                                ),
                            )
                            .label(*span, format!("`max_items {max_items}`"))
                            .help("the number of subsets grows as n^max_items; keep it small"),
                        );
                        return None;
                    };
                    let budget = match max_subsets {
                        None => DEFAULT_MAX_SUBSETS,
                        Some(m) => match u64::try_from(*m) {
                            Ok(m) if m > 0 => m,
                            _ => {
                                self.err(
                                    Diagnostic::error(
                                        "M313",
                                        "`max_subsets` must be a positive integer",
                                    )
                                    .label(*span, format!("`max_subsets {m}`")),
                                );
                                return None;
                            }
                        },
                    };
                    subset = Some((slot as usize, items, budget, *span));
                }
                _ => {}
            }
        }
        if let Some((s, _, _, span)) = subset {
            let o = 1 - s;
            let alias = |slot: usize| {
                if slot == 0 {
                    &ctx.a_alias
                } else {
                    &ctx.b_alias
                }
            };
            if let Some((_, gspan)) = &groups[s] {
                self.err(
                    Diagnostic::error(
                        "M313",
                        format!("the subset side `{}` cannot be grouped", alias(s)),
                    )
                    .label(*gspan, "remove this group")
                    .help(format!(
                        "a subset tier combines single rows of `{}`; only the other side may be grouped",
                        alias(s)
                    )),
                );
                return None;
            }
            if let Some((a_many, b_many, sspan)) = shape {
                let many = [a_many, b_many];
                let expected = [s == 0 || groups[0].is_some(), s == 1 || groups[1].is_some()];
                if many != expected {
                    let card = |m: bool| if m { "many" } else { "one" };
                    self.err(
                        Diagnostic::error(
                            "M313",
                            format!(
                                "this subset tier has the shape `{} {} to {} {}`",
                                card(expected[0]),
                                ctx.a_alias,
                                card(expected[1]),
                                ctx.b_alias
                            ),
                        )
                        .label(sspan, "inconsistent with `subset` and `group`")
                        .label(span, format!("combines rows of `{}`", alias(s)))
                        .help(format!(
                            "`{}` is `many` only when it is grouped; the shape line is optional",
                            alias(o)
                        )),
                    );
                    return None;
                }
            }
            if cardinality != Cardinality::OneToOne {
                self.err(
                    Diagnostic::error("M313", "subset tiers need `cardinality one_to_one`")
                        .label(t.span, "a unit matches one subset"),
                );
                return None;
            }
        } else {
            if let Some((a_many, b_many, span)) = shape {
                for (slot, many) in [(0usize, a_many), (1usize, b_many)] {
                    let alias = if slot == 0 {
                        &ctx.a_alias
                    } else {
                        &ctx.b_alias
                    };
                    if many && groups[slot].is_none() {
                        self.err(
                            Diagnostic::error("M304", format!("`many {alias}` needs `group {alias} by <columns>`"))
                                .label(span, "rollups compare explicitly defined groups")
                                .help(format!("say which rows form a group; to search combinations of rows (bounded subset-sum), write `subset {alias} max_items <n>` instead")),
                        );
                        return None;
                    }
                    if !many && let Some((_, gspan)) = &groups[slot] {
                        self.err(
                            Diagnostic::error(
                                "M304",
                                format!("side `{alias}` is `one` but is grouped"),
                            )
                            .label(*gspan, "remove the group or make the side `many`"),
                        );
                        return None;
                    }
                }
            }
            if (groups[0].is_some() || groups[1].is_some()) && cardinality != Cardinality::OneToOne
            {
                self.err(
                    Diagnostic::error("M304", "rollup tiers need `cardinality one_to_one`")
                        .label(t.span, "a group matches one counterpart")
                        .help("the rollup shape already expresses the many side"),
                );
                return None;
            }
        }
        let grouped = [groups[0].is_some(), groups[1].is_some()];
        let keys: [Vec<String>; 2] = [
            groups[0].as_ref().map(|g| g.0.clone()).unwrap_or_default(),
            groups[1].as_ref().map(|g| g.0.clone()).unwrap_or_default(),
        ];
        let subset_slot = subset.map(|(s, ..)| s);
        // Expressions of a subset tier are checked like a rollup whose subset side is grouped by
        // every column: its rows may be read directly (member-level) or aggregated (per subset).
        let mut agg_grouped = grouped;
        let mut agg_keys = keys.clone();
        if let Some(s) = subset_slot {
            agg_grouped[s] = true;
            agg_keys[s] = ctx
                .scope
                .cols
                .iter()
                .filter(|c| usize::from(c.slot) == s)
                .map(|c| c.phys.clone())
                .collect();
        }
        let mode = if agg_grouped[0] || agg_grouped[1] {
            AggMode::Rollup {
                grouped: agg_grouped,
                keys: agg_keys,
            }
        } else {
            AggMode::Forbidden("a tier without `group ... by` or `subset`")
        };

        let mut block = None;
        let mut requires = Vec::new();
        // per require of a subset tier: whether it is checked per subset (aggregates the side)
        let mut subset_level: Vec<bool> = Vec::new();
        let mut rank = Vec::new();
        let mut evidence = Vec::new();
        let mut flags = Vec::new();
        let mut raw: Vec<TExpr> = Vec::new();
        let mut ok = true;
        for item in &t.items {
            match item {
                TierItem::Block(spec, _) => {
                    match self.block(spec, ctx, grouped, [&keys[0], &keys[1]]) {
                        Some(pairs) => {
                            for k in &pairs {
                                raw.push(k.a.clone());
                                raw.push(k.b.clone());
                            }
                            block = Some(pairs);
                        }
                        None => ok = false,
                    }
                }
                TierItem::Require(e) => match self.expr(e, &ctx.scope, &mode) {
                    Some(x) if self.expect_bool(&x, e.span, "`require`") => {
                        if let Some(s) = subset_slot {
                            match self.subset_use(&x, e.span, s, ctx, None) {
                                Some(level) => subset_level.push(level),
                                None => {
                                    ok = false;
                                    continue;
                                }
                            }
                        }
                        raw.push(x.clone());
                        requires.push(Named {
                            name: String::new(),
                            expr: x,
                            text: self.text(e.span).to_string(),
                        });
                    }
                    _ => ok = false,
                },
                TierItem::Compare(es, _) => {
                    for e in es {
                        match self.expr(e, &ctx.scope, &mode) {
                            Some(x) if self.expect_bool(&x, e.span, "`compare`") => {
                                if let Some(s) = subset_slot {
                                    match self.subset_use(&x, e.span, s, ctx, None) {
                                        Some(level) => subset_level.push(level),
                                        None => {
                                            ok = false;
                                            continue;
                                        }
                                    }
                                }
                                raw.push(x.clone());
                                requires.push(Named {
                                    name: String::new(),
                                    expr: x,
                                    text: self.text(e.span).to_string(),
                                });
                            }
                            _ => ok = false,
                        }
                    }
                }
                TierItem::Rank(keys_, _) => {
                    for k in keys_ {
                        match self.expr(&k.expr, &ctx.scope, &mode) {
                            Some(x) => {
                                if matches!(x.ty.ty, Type::Binary | Type::Json) {
                                    self.err(
                                        Diagnostic::error(
                                            "M308",
                                            format!("cannot rank by `{}`", x.ty.ty),
                                        )
                                        .label(k.expr.span, "not orderable"),
                                    );
                                    ok = false;
                                    continue;
                                }
                                if let Some(s) = subset_slot
                                    && self
                                        .subset_use(&x, k.expr.span, s, ctx, Some("`rank by`"))
                                        .is_none()
                                {
                                    ok = false;
                                    continue;
                                }
                                raw.push(x.clone());
                                rank.push(RankKey {
                                    expr: x,
                                    desc: k.desc,
                                    text: self.text(k.expr.span).to_string(),
                                });
                            }
                            None => ok = false,
                        }
                    }
                }
                TierItem::Evidence(assigns, _) => {
                    for a in assigns {
                        match self.expr(&a.expr, &ctx.scope, &mode) {
                            Some(x) => {
                                if let Some(s) = subset_slot
                                    && self
                                        .subset_use(&x, a.expr.span, s, ctx, Some("tier evidence"))
                                        .is_none()
                                {
                                    ok = false;
                                    continue;
                                }
                                raw.push(x.clone());
                                evidence.push(Named {
                                    name: a.name.name.clone(),
                                    expr: x,
                                    text: self.text(a.expr.span).to_string(),
                                });
                            }
                            None => ok = false,
                        }
                    }
                }
                TierItem::Flag(f) => match &f.when {
                    None => flags.push((f.name.name.clone(), None, String::new())),
                    Some(w) => match self.expr(w, &ctx.scope, &mode) {
                        Some(x) if self.expect_bool(&x, w.span, "a flag") => {
                            if let Some(s) = subset_slot
                                && self
                                    .subset_use(&x, w.span, s, ctx, Some("a flag"))
                                    .is_none()
                            {
                                ok = false;
                                continue;
                            }
                            raw.push(x.clone());
                            flags.push((
                                f.name.name.clone(),
                                Some(x),
                                self.text(w.span).to_string(),
                            ));
                        }
                        _ => ok = false,
                    },
                },
                TierItem::Shape { .. } | TierItem::Group { .. } | TierItem::Subset { .. } => {}
            }
        }
        if !ok {
            return None;
        }
        // A `block` clause with keys always yields at least one key, so without a clause here or
        // at reconcile level the tier compares every pair by default; `block by none` says so on
        // purpose and is not warned about.
        let unblocked_by_default = block.is_none() && default_block.is_none();
        let block = match block {
            Some(b) => b,
            None => match default_block {
                Some(b) => {
                    for (slot, side) in [(0usize, "a"), (1usize, "b")] {
                        if grouped[slot] {
                            for k in b {
                                let e = if slot == 0 { &k.a } else { &k.b };
                                if !matches!(&e.kind, TExprKind::Column { name, .. } if keys[slot].contains(name))
                                {
                                    self.err(
                                        Diagnostic::error("M303", format!("blocking key `{}` is not a group key of side `{side}`", k.text))
                                            .label(t.span, "rollup groups must include the blocking keys"),
                                    );
                                    return None;
                                }
                            }
                        }
                    }
                    b.to_vec()
                }
                None => Vec::new(),
            },
        };
        if unblocked_by_default {
            self.err(
                Diagnostic::warning(
                    "M305",
                    format!("tier `{}` has no blocking keys", t.name.name),
                )
                .label(
                    t.name.span,
                    "every remaining A row is compared with every remaining B row",
                )
                .help(
                    "add `block by <column>`, or `block by none` if every pair should be compared",
                ),
            );
        }
        if requires.is_empty() && block.is_empty() {
            self.err(
                Diagnostic::warning("M305", format!("tier `{}` accepts any pair", t.name.name))
                    .label(t.name.span, "no `require` and no blocking keys"),
            );
        }
        for x in &raw {
            policy.collect(x);
        }
        for (slot, g) in groups.iter().enumerate() {
            if let Some((ks, _)) = g {
                for k in ks {
                    if !policy.rows[slot].contains(k) {
                        policy.rows[slot].push(k.clone());
                    }
                }
            }
        }

        // lift aggregates of grouped sides into per-group columns, and aggregates over the subset
        // side into per-subset columns
        let mut group_aggs: [Vec<(TExpr, String)>; 2] = [Vec::new(), Vec::new()];
        let mut subset_aggs: Vec<(TExpr, String)> = Vec::new();
        let mut lift = |e: TExpr| -> TExpr {
            e.transform(&mut |x| {
                let TExprKind::Agg { arg, .. } = &x.kind else {
                    return x;
                };
                let slot = match arg
                    .as_ref()
                    .map(|a| (a.columns_of(0).is_empty(), a.columns_of(1).is_empty()))
                {
                    Some((false, _)) => 0usize,
                    Some((true, false)) => 1usize,
                    // reads no column: the only aggregated side (checked during resolution)
                    _ => subset_slot.unwrap_or(usize::from(!grouped[0])),
                };
                let as_row = x
                    .clone()
                    .map_columns(&mut |_, name, ty| TExpr::col(0, name, ty));
                let (list, prefix) = if subset_slot == Some(slot) {
                    (&mut subset_aggs, "__magi_sub_")
                } else {
                    (&mut group_aggs[slot], "__magi_agg_")
                };
                let name = match list.iter().find(|(e, _)| *e == as_row) {
                    Some((_, n)) => n.clone(),
                    None => {
                        let n = format!("{prefix}{}", list.len() + 1);
                        list.push((as_row, n.clone()));
                        n
                    }
                };
                TExpr::col(slot as u8, name, x.ty)
            })
        };
        let requires: Vec<Named> = requires
            .into_iter()
            .map(|n| Named {
                expr: lift(n.expr),
                ..n
            })
            .collect();
        let rank = rank
            .into_iter()
            .map(|r| RankKey {
                expr: lift(r.expr),
                ..r
            })
            .collect();
        let evidence = evidence
            .into_iter()
            .map(|n| Named {
                expr: lift(n.expr),
                ..n
            })
            .collect();
        let flags = flags
            .into_iter()
            .map(|(n, e, s)| (n, e.map(&mut lift), s))
            .collect();
        let (requires, subset) = match subset {
            None => (requires, None),
            Some((slot, max_items, max_subsets, span)) => {
                let (per_subset, per_member): (Vec<_>, Vec<_>) = requires
                    .into_iter()
                    .zip(subset_level)
                    .partition(|(_, level)| *level);
                let subset = Subset {
                    slot: slot as u8,
                    max_items,
                    max_subsets,
                    requires: per_subset.into_iter().map(|(r, _)| r).collect(),
                    aggs: subset_aggs,
                    span,
                };
                (
                    per_member.into_iter().map(|(r, _)| r).collect(),
                    Some(subset),
                )
            }
        };
        let [aggs_a, aggs_b] = group_aggs;
        let group_a = groups[0].as_ref().map(|(k, _)| Group {
            keys: k.clone(),
            aggs: aggs_a,
        });
        let group_b = groups[1].as_ref().map(|(k, _)| Group {
            keys: k.clone(),
            aggs: aggs_b,
        });
        Some(Tier {
            name: t.name.name.clone(),
            block,
            requires,
            rank,
            group_a,
            group_b,
            subset,
            evidence,
            flags,
            span: t.span,
        })
    }

    /// Checks an expression of a subset tier whose side `s` is combined. `per_subset` names a
    /// clause that has one value per subset (rank key, evidence, flag); a `require` (`None`) may
    /// instead be checked per member. Returns whether the expression aggregates over the subset
    /// side, or `None` after reporting an error.
    fn subset_use(
        &mut self,
        x: &TExpr,
        span: Span,
        s: usize,
        ctx: &Ctx,
        per_subset: Option<&str>,
    ) -> Option<bool> {
        let (s8, o8) = (s as u8, 1 - s as u8);
        let (mut mixed, mut over_subset) = (false, false);
        x.walk(&mut |e| {
            if let TExprKind::Agg { arg, .. } = &e.kind {
                let (reads_s, reads_o) = arg.as_ref().map_or((false, false), |a| {
                    (!a.columns_of(s8).is_empty(), !a.columns_of(o8).is_empty())
                });
                mixed |= reads_s && reads_o;
                // an aggregate reading no column counts the subset (the other side is not
                // grouped, or resolution would have asked which side to count)
                over_subset |= reads_s || !reads_o;
            }
        });
        if mixed {
            self.err(
                Diagnostic::error("M110", "an aggregate may only read one side")
                    .label(span, "reads both sides"),
            );
            return None;
        }
        if per_subset.is_none() && !over_subset {
            return Some(false);
        }
        let bare = x
            .clone()
            .transform(&mut |e| match e.kind {
                TExprKind::Agg { .. } => TExpr::lit(Lit::Null),
                _ => e,
            })
            .columns_of(s8);
        let Some(col) = bare.first() else {
            return Some(over_subset);
        };
        let alias = if s == 0 { &ctx.a_alias } else { &ctx.b_alias };
        let diag = match per_subset {
            None => Diagnostic::error(
                "M314",
                format!("`{alias}.{col}` is read outside an aggregate in a condition checked per subset"),
            )
            .label(span, format!("aggregates over `{alias}` make this a subset-level `require`"))
            .help(format!(
                "aggregate it (e.g. `max({alias}.{col})`), or move the per-row part into its own `require`, which is checked for every member"
            )),
            Some(clause) => Diagnostic::error(
                "M314",
                format!("`{alias}.{col}` varies within a subset"),
            )
            .label(span, format!("{clause} of a subset tier has one value per subset"))
            .help(format!("aggregate it, e.g. `max({alias}.{col})`")),
        };
        self.err(diag);
        None
    }

    fn register_parts(&mut self, rc: &Reconcile, d: &ast::ReconcileDecl) -> bool {
        let span = d.span;
        let name = &rc.name;
        let lin = |this: &mut Self, label: String, deps: Vec<LineageId>| {
            this.hir.lineage.add(label, deps)
        };
        let col_lineage =
            |side: &Side, c: &str| side.columns.iter().find(|x| x.name == c).map(|x| x.lineage);

        let id_cols = |this: &mut Self, side: &Side, part: &str| -> Vec<Column> {
            match &side.identity {
                Identity::Synthetic => {
                    let l = lin(
                        this,
                        format!(
                            "{name}.{part}.{}_row (synthetic row number of {})",
                            side.alias, side.relation
                        ),
                        Vec::new(),
                    );
                    vec![Column {
                        name: format!("{}_row", side.alias),
                        ty: ColType::required(Type::Int),
                        lineage: l,
                    }]
                }
                Identity::Declared(ids) => ids
                    .iter()
                    .map(|c| {
                        let src = side.columns.iter().find(|x| &x.name == c).unwrap();
                        let l = lin(
                            this,
                            format!("{name}.{part}.{}_{c}", side.alias),
                            vec![src.lineage],
                        );
                        Column {
                            name: format!("{}_{c}", side.alias),
                            ty: src.ty,
                            lineage: l,
                        }
                    })
                    .collect(),
            }
        };
        let prefixed =
            |this: &mut Self, side: &Side, part: &str, skip_identity: bool| -> Vec<Column> {
                let ids: Vec<String> = match &side.identity {
                    Identity::Declared(ids) if skip_identity => ids.clone(),
                    _ => Vec::new(),
                };
                side.columns
                    .iter()
                    .filter(|c| !ids.contains(&c.name))
                    .map(|c| {
                        let l = lin(
                            this,
                            format!("{name}.{part}.{}_{}", side.alias, c.name),
                            vec![c.lineage],
                        );
                        Column {
                            name: format!("{}_{}", side.alias, c.name),
                            ty: c.ty.with_nullable(true),
                            lineage: l,
                        }
                    })
                    .collect()
            };
        let evidence_cols = |this: &mut Self, part: &str| -> Vec<Column> {
            let mut out = Vec::new();
            for n in rc.evidence.iter().chain(rc.flags.iter()) {
                let mut deps = Vec::new();
                for (slot, side) in [(0u8, &rc.a), (1u8, &rc.b)] {
                    for c in n.expr.columns_of(slot) {
                        if let Some(l) = col_lineage(side, &c) {
                            deps.push(l);
                        }
                    }
                }
                let l = lin(this, format!("{name}.{part}.{} = {}", n.name, n.text), deps);
                let ty = if rc
                    .flags
                    .iter()
                    .any(|f| f.name.eq_ignore_ascii_case(&n.name))
                {
                    ColType::nullable(Type::Bool)
                } else {
                    n.expr.ty.with_nullable(true)
                };
                out.push(Column {
                    name: n.name.clone(),
                    ty,
                    lineage: l,
                });
            }
            for tc in &rc.tier_columns {
                // the inputs of every tier's definition of this column
                let mut deps = Vec::new();
                for t in &rc.tiers {
                    let evidence = t
                        .evidence
                        .iter()
                        .filter(|n| n.name.eq_ignore_ascii_case(&tc.name))
                        .map(|n| &n.expr);
                    let flags = t
                        .flags
                        .iter()
                        .filter(|f| f.0.eq_ignore_ascii_case(&tc.name))
                        .filter_map(|f| f.1.as_ref());
                    for e in evidence.chain(flags) {
                        for (slot, side, group) in
                            [(0u8, &rc.a, &t.group_a), (1u8, &rc.b, &t.group_b)]
                        {
                            for c in e.columns_of(slot) {
                                // a rollup or subset aggregate column stands for the side
                                // columns it reads
                                let agg = group
                                    .as_ref()
                                    .map(|g| &g.aggs)
                                    .into_iter()
                                    .chain(
                                        t.subset
                                            .as_ref()
                                            .filter(|s| s.slot == slot)
                                            .map(|s| &s.aggs),
                                    )
                                    .flatten()
                                    .find(|(_, n)| *n == c);
                                let cols = match agg {
                                    Some((expr, _)) => expr.columns_of(0),
                                    None => vec![c],
                                };
                                for c in cols {
                                    if let Some(l) = col_lineage(side, &c)
                                        && !deps.contains(&l)
                                    {
                                        deps.push(l);
                                    }
                                }
                            }
                        }
                    }
                }
                let l = lin(
                    this,
                    format!(
                        "{name}.{part}.{} (tier {})",
                        tc.name,
                        if tc.is_flag { "flag" } else { "evidence" }
                    ),
                    deps,
                );
                out.push(Column {
                    name: tc.name.clone(),
                    ty: tc.ty,
                    lineage: l,
                });
            }
            out
        };
        let simple = |this: &mut Self, part: &str, cols: &[(&str, ColType)]| -> Vec<Column> {
            cols.iter()
                .map(|(n, t)| {
                    let l = lin(this, format!("{name}.{part}.{n}"), Vec::new());
                    Column {
                        name: n.to_string(),
                        ty: *t,
                        lineage: l,
                    }
                })
                .collect()
        };
        let s = ColType::required(Type::String);
        let i = ColType::required(Type::Int);
        let oi = ColType::nullable(Type::Int);

        let mut matches = simple(
            self,
            "matches",
            &[
                ("tier", s),
                ("tier_index", i),
                ("a_group", oi),
                ("b_group", oi),
            ],
        );
        matches.extend(id_cols(self, &rc.a, "matches"));
        matches.extend(id_cols(self, &rc.b, "matches"));
        matches.extend(evidence_cols(self, "matches"));
        matches.extend(simple(
            self,
            "matches",
            &[
                ("flags", s),
                ("candidates_a", i),
                ("candidates_b", i),
                ("candidate_rank", i),
                ("rank_values", ColType::nullable(Type::String)),
                ("reason", s),
            ],
        ));
        matches.extend(prefixed(self, &rc.a, "matches", true));
        matches.extend(prefixed(self, &rc.b, "matches", true));

        let unmatched = |this: &mut Self, side: &Side, part: &str| -> Vec<Column> {
            let mut cols = simple(
                this,
                part,
                &[
                    ("match_status", s),
                    ("duplicate_of", ColType::nullable(Type::String)),
                ],
            );
            if matches!(side.identity, Identity::Synthetic) {
                cols.extend(id_cols(this, side, part));
            }
            for c in &side.columns {
                let l = lin(this, format!("{name}.{part}.{}", c.name), vec![c.lineage]);
                cols.push(Column {
                    name: c.name.clone(),
                    ty: c.ty,
                    lineage: l,
                });
            }
            cols
        };
        let unmatched_a = unmatched(self, &rc.a, "unmatched_a");
        let unmatched_b = unmatched(self, &rc.b, "unmatched_b");

        let mut candidates = simple(
            self,
            "candidates",
            &[
                ("tier", s),
                ("tier_index", i),
                ("a_group", oi),
                ("b_group", oi),
            ],
        );
        candidates.extend(id_cols(self, &rc.a, "candidates"));
        candidates.extend(id_cols(self, &rc.b, "candidates"));
        candidates.extend(simple(
            self,
            "candidates",
            &[
                ("outcome", s),
                ("candidate_rank", i),
                ("rank_values", ColType::nullable(Type::String)),
            ],
        ));
        candidates.extend(evidence_cols(self, "candidates"));

        let mut ambiguous = simple(
            self,
            "ambiguous",
            &[
                ("tier", s),
                ("tier_index", i),
                ("ambiguity_id", i),
                ("a_group", oi),
                ("b_group", oi),
            ],
        );
        ambiguous.extend(id_cols(self, &rc.a, "ambiguous"));
        ambiguous.extend(id_cols(self, &rc.b, "ambiguous"));
        ambiguous.extend(evidence_cols(self, "ambiguous"));
        ambiguous.extend(prefixed(self, &rc.a, "ambiguous", true));
        ambiguous.extend(prefixed(self, &rc.b, "ambiguous", true));

        let summary = simple(
            self,
            "summary",
            &[
                ("position", i),
                ("step", s),
                ("tier_index", oi),
                ("pairs", i),
                ("a_rows", i),
                ("b_rows", i),
            ],
        );

        for (part, cols) in [
            ("matches", &matches),
            ("unmatched_a", &unmatched_a),
            ("unmatched_b", &unmatched_b),
            ("candidates", &candidates),
            ("ambiguous", &ambiguous),
        ] {
            let mut seen: Vec<&str> = Vec::new();
            for c in cols.iter() {
                if seen.contains(&c.name.as_str()) {
                    self.err(
                        Diagnostic::error("M307", format!("`{name}.{part}` would have two columns named `{}`", c.name))
                            .label(d.name.span, "rename the evidence/flag, or use `as` to give the sides other prefixes"),
                    );
                    return false;
                }
                seen.push(&c.name);
            }
        }
        let identity_of = |side: &Side| match &side.identity {
            Identity::Declared(ids) => ids
                .iter()
                .map(|c| format!("{}_{c}", side.alias))
                .collect::<Vec<_>>(),
            Identity::Synthetic => vec![format!("{}_row", side.alias)],
        };
        let pair_identity = [identity_of(&rc.a), identity_of(&rc.b)].concat();
        let add = |this: &mut Self,
                   part: &str,
                   columns: Vec<Column>,
                   identity: Option<Vec<String>>,
                   sort: &[&str]| {
            this.hir.add_relation(Relation {
                name: format!("{name}.{part}"),
                kind: RelKind::ReconcilePart,
                columns,
                open: false,
                identity,
                sort: sort.iter().map(|s| (s.to_string(), false)).collect(),
                span,
            });
        };
        add(
            self,
            "matches",
            matches,
            Some(pair_identity),
            &["tier_index"],
        );
        add(self, "unmatched_a", unmatched_a, rc_identity(&rc.a), &[]);
        add(self, "unmatched_b", unmatched_b, rc_identity(&rc.b), &[]);
        add(self, "candidates", candidates, None, &["tier_index"]);
        add(
            self,
            "ambiguous",
            ambiguous,
            None,
            &["tier_index", "ambiguity_id"],
        );
        add(self, "summary", summary, None, &["position"]);
        true
    }
}

fn rc_identity(side: &Side) -> Option<Vec<String>> {
    match &side.identity {
        Identity::Declared(ids) => Some(ids.clone()),
        Identity::Synthetic => Some(vec![format!("{}_row", side.alias)]),
    }
}

/// Columns that tell rows of each side apart; a side's classes are the rows equal on all of them
/// (see [`Policy::columns`]).
#[derive(Default)]
struct Policy {
    /// Read outside an aggregate somewhere in the policy, or group keys.
    rows: [Vec<String>; 2],
    /// Read inside aggregates (`sum(b.amount)`, `count(b.ref)`).
    aggregated: [Vec<String>; 2],
}

impl Policy {
    /// Record the columns `e` reads, split into bare and aggregated reads.
    fn collect(&mut self, e: &TExpr) {
        let mut refs: Vec<(u8, &str)> = Vec::new();
        let mut in_aggs: Vec<(u8, &str)> = Vec::new();
        e.walk(&mut |x| match &x.kind {
            TExprKind::Column { slot, name } => refs.push((*slot, name.as_str())),
            TExprKind::Agg { arg: Some(a), .. } => a.walk(&mut |y| {
                if let TExprKind::Column { slot, name } = &y.kind {
                    in_aggs.push((*slot, name.as_str()));
                }
            }),
            _ => {}
        });
        // aggregates do not nest, so each occurrence inside one is removed once
        for r in &in_aggs {
            if let Some(i) = refs.iter().position(|x| x == r) {
                refs.swap_remove(i);
            }
        }
        let add = |cols: &mut Vec<String>, name: &str| {
            if !name.starts_with("__magi_agg_") && !cols.iter().any(|c| c == name) {
                cols.push(name.to_string());
            }
        };
        for (slot, name) in refs {
            add(&mut self.rows[usize::from(slot)], name);
        }
        for (slot, name) in in_aggs {
            add(&mut self.aggregated[usize::from(slot)], name);
        }
    }

    /// The policy columns of side `slot`: every column read outside an aggregate, plus, with
    /// `consume none`, the columns read only inside aggregates.
    ///
    /// A column read only inside aggregates matters to groups and subsets, never to single rows,
    /// and counting it would split interchangeable rows (an identity column would make every
    /// row its own class). Leaving it out is safe as long as the identity-order pairing inside a
    /// class never changes what later tiers see. When a side is consumed it does not: a decided
    /// group of classes is matched whole (leftovers are exact duplicates), an ambiguity is
    /// withheld or left available whole, so the consumed side keeps the same rows whichever
    /// pairing was chosen, and the other side keeps all of its rows with no pair excluded. With
    /// `consume none` both partners stay and the matched pair itself is excluded from later
    /// tiers, so pairing a row that differs in such a column with one counterpart or another
    /// would change the outcome: there the column splits classes like any other.
    fn columns(&mut self, slot: usize, consume: Consume) -> Vec<String> {
        let mut cols = std::mem::take(&mut self.rows[slot]);
        if consume == Consume::None {
            for c in std::mem::take(&mut self.aggregated[slot]) {
                if !cols.contains(&c) {
                    cols.push(c);
                }
            }
        }
        cols
    }
}
