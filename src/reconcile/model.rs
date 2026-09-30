//! Reconciliation as a first-class HIR operation. The model is resolved and
//! typed here and lowered to ordinary relational operators by `reconcile::lower`.
//!
//! Expressions in tiers use slot 0 for the A side and slot 1 for the B side. On a grouped side
//! (rollup tiers) slot columns are the group keys plus `__magi_agg_N` columns computed per group.
//! On the subset side of a subset tier, rank keys, evidence, flags and subset-level requires read
//! only `__magi_sub_N` columns computed per subset.

use crate::ast::BinaryOp;
use crate::semantic::hir::{AggFunc, Column, TExpr, TExprKind};
use crate::semantic::types::{ColType, Type};
use crate::syntax::span::Span;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cardinality {
    OneToOne,
    /// One A row may match many B rows; each B row matches at most one A row.
    OneToMany,
    /// Many A rows may match one B row; each A row matches at most one B row.
    ManyToOne,
    ManyToMany,
}

impl Cardinality {
    pub fn name(self) -> &'static str {
        match self {
            Cardinality::OneToOne => "one_to_one",
            Cardinality::OneToMany => "one_to_many",
            Cardinality::ManyToOne => "many_to_one",
            Cardinality::ManyToMany => "many_to_many",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consume {
    Both,
    A,
    B,
    None,
}

impl Consume {
    pub fn a(self) -> bool {
        matches!(self, Consume::Both | Consume::A)
    }
    pub fn b(self) -> bool {
        matches!(self, Consume::Both | Consume::B)
    }
    pub fn name(self) -> &'static str {
        match self {
            Consume::Both => "both",
            Consume::A => "a",
            Consume::B => "b",
            Consume::None => "none",
        }
    }
}

/// What happens to rows involved in an ambiguity or left over as exact duplicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    /// Withheld from later tiers and reported.
    Hold,
    /// Reported, but still available to later tiers.
    Continue,
}

#[derive(Debug, Clone)]
pub enum Identity {
    /// Declared business identity columns (must be unique; checked at run time).
    Declared(Vec<String>),
    /// Row number over all columns: deterministic for identical input, not stable across refreshes.
    Synthetic,
}

#[derive(Debug, Clone)]
pub struct Side {
    pub relation: String,
    pub alias: String,
    pub columns: Vec<Column>,
    pub identity: Identity,
    /// Columns the policy looks at anywhere outside aggregates. Rows equal on all of them are
    /// interchangeable.
    pub policy_columns: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct KeyPair {
    pub a: TExpr,
    pub b: TExpr,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct RankKey {
    pub expr: TExpr,
    pub desc: bool,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct Group {
    pub keys: Vec<String>,
    /// Aggregates over the grouped side's rows: (expression over the side's row columns, name).
    pub aggs: Vec<(TExpr, String)>,
}

/// Default `max_subsets` of a subset tier.
pub const DEFAULT_MAX_SUBSETS: u64 = 2_000_000;
/// Largest `max_items` of a subset tier.
pub const MAX_SUBSET_ITEMS: u32 = 16;

/// Bounded subset matching (`subset b max_items 5`): a unit of the other side matches a
/// combination of at most `max_items` remaining rows of this side.
#[derive(Debug, Clone)]
pub struct Subset {
    /// The side whose rows are combined: 0 = A, 1 = B.
    pub slot: u8,
    pub max_items: u32,
    /// The run stops when the tier would have more subsets than this (without a sum target,
    /// before enumerating) or has examined more (with one, while enumerating).
    pub max_subsets: u64,
    /// Requires with aggregates over the subset side, checked per subset. `Tier::requires`
    /// holds the member-level ones (checked per unit and row of this side).
    pub requires: Vec<Named>,
    /// Aggregates over a subset's rows: (expression over the side's row columns, name).
    pub aggs: Vec<(TExpr, String)>,
    /// The `subset` clause.
    pub span: Span,
}

/// A subset-level `require` conjunct `sum(x) == t` (either order): `x` is exact (int or decimal)
/// and `t` reads no column of the subset side. Enumeration skips partial subsets that can no
/// longer reach `t`.
#[derive(Debug, Clone)]
pub struct SumTarget {
    /// The summed expression, over the subset side's row columns in slot 0 (as in `aggs`).
    pub arg: TExpr,
    /// The target, over the other side's columns in their own slot (as in the require).
    pub target: TExpr,
    /// Index into `requires` of the require it comes from.
    pub require: usize,
}

impl Subset {
    /// The first subset-level `require` conjunct that bounds a subset's exact sum (see
    /// [`SumTarget`]). Float sums are left out: a partial sum rounds differently from the final
    /// `SUM` and could prune a real candidate.
    pub fn sum_target(&self) -> Option<SumTarget> {
        fn conjuncts<'a>(e: &'a TExpr, out: &mut Vec<&'a TExpr>) {
            match &e.kind {
                TExprKind::Binary {
                    op: BinaryOp::And,
                    left,
                    right,
                } => {
                    conjuncts(left, out);
                    conjuncts(right, out);
                }
                _ => out.push(e),
            }
        }
        let exact = |t: &TExpr| matches!(t.ty.ty, Type::Int | Type::Decimal(..));
        for (i, r) in self.requires.iter().enumerate() {
            let mut cs = Vec::new();
            conjuncts(&r.expr, &mut cs);
            for c in cs {
                let TExprKind::Binary {
                    op: BinaryOp::Eq,
                    left,
                    right,
                } = &c.kind
                else {
                    continue;
                };
                for (s, t) in [(left, right), (right, left)] {
                    let TExprKind::Column { slot, name } = &s.kind else {
                        continue;
                    };
                    if *slot != self.slot || !t.columns_of(self.slot).is_empty() || !exact(t) {
                        continue;
                    }
                    let arg = self.aggs.iter().find_map(|(e, n)| match &e.kind {
                        TExprKind::Agg {
                            func: AggFunc::Sum,
                            arg: Some(a),
                        } if n == name && exact(a) => Some(a),
                        _ => None,
                    });
                    if let Some(a) = arg {
                        return Some(SumTarget {
                            arg: (**a).clone(),
                            target: (**t).clone(),
                            require: i,
                        });
                    }
                }
            }
        }
        None
    }
}

#[derive(Debug, Clone)]
pub struct Named {
    pub name: String,
    pub expr: TExpr,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct Tier {
    pub name: String,
    pub block: Vec<KeyPair>,
    pub requires: Vec<Named>,
    pub rank: Vec<RankKey>,
    pub group_a: Option<Group>,
    pub group_b: Option<Group>,
    pub subset: Option<Subset>,
    pub evidence: Vec<Named>,
    /// Flags raised for matches of this tier; `None` expression = always.
    pub flags: Vec<(String, Option<TExpr>, String)>,
    pub span: Span,
}

impl Tier {
    pub fn is_rollup(&self) -> bool {
        self.group_a.is_some() || self.group_b.is_some()
    }
    /// One-line policy summary used in `reason` and `magi plan`.
    pub fn description(&self) -> String {
        let mut parts = Vec::new();
        if let Some(g) = &self.group_a {
            parts.push(format!("group a by {}", g.keys.join(", ")));
        }
        if let Some(g) = &self.group_b {
            parts.push(format!("group b by {}", g.keys.join(", ")));
        }
        if let Some(s) = &self.subset {
            let side = if s.slot == 0 { "a" } else { "b" };
            parts.push(format!("subset {side} max_items {}", s.max_items));
        }
        if self.block.is_empty() {
            parts.push("no blocking".to_string());
        } else {
            parts.push(format!(
                "block by {}",
                self.block
                    .iter()
                    .map(|k| k.text.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        parts.extend(self.requires.iter().map(|r| r.text.clone()));
        if let Some(s) = &self.subset {
            parts.extend(s.requires.iter().map(|r| r.text.clone()));
        }
        if !self.rank.is_empty() {
            parts.push(format!(
                "rank by {}",
                self.rank
                    .iter()
                    .map(|r| format!("{} {}", r.text, if r.desc { "desc" } else { "asc" }))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        format!("{}: {}", self.name, parts.join("; "))
    }
}

/// A tier-level evidence or flag column in `matches`, unified across tiers.
#[derive(Debug, Clone)]
pub struct TierColumn {
    pub name: String,
    pub ty: ColType,
    pub is_flag: bool,
}

#[derive(Debug, Clone)]
pub struct Reconcile {
    pub name: String,
    pub a: Side,
    pub b: Side,
    pub cardinality: Cardinality,
    pub consume: Consume,
    pub ambiguity: Hold,
    pub duplicates: Hold,
    /// Row-pair evidence computed for every match / candidate / ambiguity row.
    pub evidence: Vec<Named>,
    /// Row-pair flags.
    pub flags: Vec<Named>,
    pub tiers: Vec<Tier>,
    pub tier_columns: Vec<TierColumn>,
    pub span: Span,
}

pub const PARTS: &[&str] = &[
    "matches",
    "unmatched_a",
    "unmatched_b",
    "candidates",
    "ambiguous",
    "summary",
];
