//! Lowering reconciliation policies to ordinary relational operators.
//!
//! Every tier runs over the rows earlier tiers left over:
//!
//! 1. **Units.** A row, or for a grouped (rollup) side one group of rows (a group of one row
//!    included; rows with a null group key are in no group). Rows that agree on every column the
//!    policy looks at outside aggregates form a *class*; members of a class are interchangeable
//!    as far as the policy can tell (a column read only inside aggregates concerns groups and
//!    subsets, never single rows; with `consume none` it counts too, since there the pairing
//!    decides which pairs later tiers exclude). With `consume none` a row that is already
//!    matched is a class of its own, since its earlier partners are excluded.
//! 2. **Candidates.** Unit pairs that share the blocking keys and satisfy every `require`, with
//!    their rank values and tier evidence. With `consume none` a pair that is already matched
//!    (for a group: any of its row pairs) is not a candidate again.
//! 3. **Selection** (`one_to_one`), in rounds until nothing changes:
//!    * pair classes that are each other's unique best candidate ("mutual best"); members are
//!      paired in identity order. If one class has more remaining members than the other, the
//!      leftovers are either exact duplicates (identical in every column but identity, reported
//!      as `duplicate` of the class's first member) or distinguishable rows, in which case
//!      choosing among them would be arbitrary and the classes are reported as ambiguous
//!      instead;
//!    * in the same step, *stars*: a class C whose best candidates are tied across a set T of
//!      classes of the other side, each of which has C as its unique best candidate. With as
//!      many remaining members in C as in T together, all are matched: C's members are
//!      interchangeable, so any assignment is equivalent, and C's members pair with T's in
//!      identity order (across the classes of T). If C has more members and they are exact
//!      duplicates of each other, or T has more and all its members are exact duplicates of each
//!      other, the leftovers of the larger side are `duplicate`s of its first member. Otherwise
//!      the star is not decided here. A leaf of a star has a unique best candidate and its
//!      centre several, so stars and mutual best pairs never share a class;
//!    * only when nothing was decided anywhere: a class whose best candidates are tied is
//!      reported as ambiguous together with those candidates (ambiguity is data).
//!
//! The same input therefore always yields the same result and no tie is broken by storage order.

use crate::ast::BinaryOp;
use crate::plan::logical::{JoinType, LogicalPlan};
use crate::reconcile::model::*;
use crate::semantic::hir::{AggFunc, Lit, OrderKey, TExpr, TExprKind, WinFunc};
use crate::semantic::types::{ColType, Type};

// Subset tiers (`subset b max_items 5`) are lowered separately but share the helpers below.
#[path = "subset.rs"]
pub mod subset;

/// One executable operation of a reconciliation program.
#[derive(Debug, Clone)]
pub enum Op {
    Create {
        table: String,
        plan: LogicalPlan,
    },
    CreateEmpty {
        table: String,
        columns: Vec<(String, String)>,
    },
    Insert {
        table: String,
        plan: LogicalPlan,
    },
}

#[derive(Debug, Clone)]
pub enum TierMode {
    /// `one_to_one`: repeat `round`; if `progress` has rows run `commit`, else run `tie`; if
    /// `tied` has rows run `tie_commit`, else the tier is finished.
    Rounds {
        round: Vec<Op>,
        progress: String,
        commit: Vec<Op>,
        tie: Vec<Op>,
        tied: String,
        tie_commit: Vec<Op>,
    },
    /// Other cardinalities select in one pass.
    Single(Vec<Op>),
    /// Subset tiers: check the budget, then enumerate and select in one pass.
    Subset(subset::Program),
}

#[derive(Debug, Clone)]
pub struct TierProgram {
    pub name: String,
    pub index: usize,
    pub description: String,
    pub prepare: Vec<Op>,
    pub mode: TierMode,
}

#[derive(Debug, Clone)]
pub struct IdentityCheck {
    pub side: String,
    pub columns: Vec<String>,
    /// One row: `rows`, `ids` (distinct identities), `null_ids`.
    pub plan: LogicalPlan,
}

#[derive(Debug, Clone)]
pub struct ReconcileProgram {
    pub name: String,
    pub setup: Vec<Op>,
    pub identity_checks: Vec<IdentityCheck>,
    pub tiers: Vec<TierProgram>,
    pub outputs: Vec<Op>,
    /// One row with boolean `ok`: every input row is accounted for exactly once.
    pub invariant: LogicalPlan,
    /// Internal tables to drop afterwards.
    pub scratch: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// expression helpers (internal expressions are untyped)

const U: ColType = ColType::nullable(Type::Unknown);

// Internal column names; the `__magi` prefix is reserved, so they cannot collide with user columns.
const ID: &str = "__magi_id";
const CLS: &str = "__magi_cls";
const EXACT: &str = "__magi_exact";
/// A tier's unit: a row (its `ID`) or, on a grouped side, a group of rows.
const UNIT: &str = "__magi_unit";
const DUP_OF: &str = "__magi_dup_of";
/// Side A columns carried through a join to side B.
const A_PREFIX: &str = "__magi_a_";
const A_ID: &str = "__magi_a___magi_id";

fn t(kind: TExprKind) -> TExpr {
    TExpr::new(kind, U)
}
fn c(n: &str) -> TExpr {
    TExpr::col(0, n, U)
}
fn c1(n: &str) -> TExpr {
    TExpr::col(1, n, U)
}
fn bin(op: BinaryOp, a: TExpr, b: TExpr) -> TExpr {
    t(TExprKind::Binary {
        op,
        left: Box::new(a),
        right: Box::new(b),
    })
}
fn eq(a: TExpr, b: TExpr) -> TExpr {
    bin(BinaryOp::Eq, a, b)
}
fn and(a: TExpr, b: TExpr) -> TExpr {
    bin(BinaryOp::And, a, b)
}
fn or(a: TExpr, b: TExpr) -> TExpr {
    bin(BinaryOp::Or, a, b)
}
fn and_all(xs: Vec<TExpr>) -> Option<TExpr> {
    xs.into_iter().reduce(and)
}
fn int(v: i64) -> TExpr {
    TExpr::lit(Lit::Int(v))
}
fn s(v: &str) -> TExpr {
    TExpr::lit(Lit::Str(v.to_string()))
}
fn null() -> TExpr {
    TExpr::lit(Lit::Null)
}
fn not_null(e: TExpr) -> TExpr {
    t(TExprKind::IsNull {
        expr: Box::new(e),
        negated: true,
    })
}
fn agg(f: AggFunc, arg: Option<TExpr>) -> TExpr {
    t(TExprKind::Agg {
        func: f,
        arg: arg.map(Box::new),
    })
}
fn win(
    f: WinFunc,
    arg: Option<TExpr>,
    partition: Vec<TExpr>,
    order: Vec<OrderKey>,
    filter: Option<TExpr>,
) -> TExpr {
    t(TExprKind::Window {
        func: f,
        arg: arg.map(Box::new),
        partition,
        order,
        filter: filter.map(Box::new),
    })
}
fn asc(e: TExpr) -> OrderKey {
    OrderKey {
        expr: e,
        desc: false,
    }
}
fn call(f: &'static str, args: Vec<TExpr>) -> TExpr {
    t(TExprKind::Call { func: f, args })
}
fn native(name: &str, args: Vec<TExpr>) -> TExpr {
    t(TExprKind::Native {
        name: name.to_string(),
        args,
    })
}
fn to_text(e: TExpr) -> TExpr {
    call("to_string", vec![e])
}
fn case(arms: Vec<(TExpr, TExpr)>, otherwise: Option<TExpr>) -> TExpr {
    t(TExprKind::Case {
        arms,
        otherwise: otherwise.map(Box::new),
    })
}
fn keep(names: &[&str]) -> Vec<(TExpr, String)> {
    names.iter().map(|n| (c(n), n.to_string())).collect()
}
fn keep1(names: &[&str]) -> Vec<(TExpr, String)> {
    names.iter().map(|n| (c1(n), n.to_string())).collect()
}
fn named(e: TExpr, n: &str) -> (TExpr, String) {
    (e, n.to_string())
}
fn scan(n: &str) -> LogicalPlan {
    LogicalPlan::scan(n)
}
fn cross(l: LogicalPlan, r: LogicalPlan, output: Vec<(TExpr, String)>) -> LogicalPlan {
    l.join(r, JoinType::Inner, None, output)
}

/// Move slot-0 / slot-1 column references to new slots with a name prefix.
fn reslot(e: &TExpr, a: (u8, &str), b: (u8, &str)) -> TExpr {
    e.clone().map_columns(&mut |slot, name, ty| {
        let (ns, prefix) = if slot == 0 { a } else { b };
        TExpr::col(ns, format!("{prefix}{name}"), ty)
    })
}

fn side_identity_cols(side: &Side) -> Vec<String> {
    match &side.identity {
        Identity::Declared(ids) => ids.clone(),
        Identity::Synthetic => Vec::new(),
    }
}

// ---------------------------------------------------------------------------------------------

pub struct Names {
    pub prefix: String,
}

impl Names {
    fn t(&self, s: &str) -> String {
        format!("{}{s}", self.prefix)
    }
}

pub fn lower(rc: &Reconcile) -> ReconcileProgram {
    let n = Names {
        prefix: format!("__magi_rc_{}_", rc.name),
    };
    let (ta, tb) = (n.t("a"), n.t("b"));
    let (links, held_a, held_b, cands) = (n.t("links"), n.t("held_a"), n.t("held_b"), n.t("cands"));
    let mut scratch = vec![
        ta.clone(),
        tb.clone(),
        links.clone(),
        held_a.clone(),
        held_b.clone(),
        cands.clone(),
    ];
    let mut setup = Vec::new();
    let mut identity_checks = Vec::new();

    // ---- setup: stage both sides with identity, class and exact-duplicate keys
    for (side, table) in [(&rc.a, &ta), (&rc.b, &tb)] {
        let all: Vec<OrderKey> = side.columns.iter().map(|col| asc(c(&col.name))).collect();
        let ids = side_identity_cols(side);
        let id = match &side.identity {
            Identity::Declared(cols) => win(
                WinFunc::DenseRank,
                None,
                Vec::new(),
                cols.iter().map(|x| asc(c(x))).collect(),
                None,
            ),
            Identity::Synthetic => win(WinFunc::RowNumber, None, Vec::new(), all.clone(), None),
        };
        let cls = if side.policy_columns.is_empty() {
            int(1)
        } else {
            win(
                WinFunc::DenseRank,
                None,
                Vec::new(),
                side.policy_columns.iter().map(|x| asc(c(x))).collect(),
                None,
            )
        };
        let non_id: Vec<OrderKey> = side
            .columns
            .iter()
            .filter(|col| !ids.contains(&col.name))
            .map(|col| asc(c(&col.name)))
            .collect();
        let exact = if non_id.is_empty() {
            int(1)
        } else {
            win(WinFunc::DenseRank, None, Vec::new(), non_id, None)
        };
        let plan =
            scan(&side.relation).window(vec![named(id, ID), named(cls, CLS), named(exact, EXACT)]);
        setup.push(Op::Create {
            table: table.clone(),
            plan,
        });
        if let Identity::Declared(cols) = &side.identity {
            let any_null = cols
                .iter()
                .map(|x| {
                    t(TExprKind::IsNull {
                        expr: Box::new(c(x)),
                        negated: false,
                    })
                })
                .reduce(or)
                .unwrap();
            let plan = scan(table).aggregate(
                Vec::new(),
                vec![
                    named(agg(AggFunc::Count, None), "rows"),
                    named(agg(AggFunc::CountDistinct, Some(c(ID))), "ids"),
                    named(
                        agg(
                            AggFunc::Sum,
                            Some(case(vec![(any_null, int(1))], Some(int(0)))),
                        ),
                        "null_ids",
                    ),
                ],
            );
            identity_checks.push(IdentityCheck {
                side: side.alias.clone(),
                columns: cols.clone(),
                plan,
            });
        }
    }
    let mut link_cols: Vec<(String, String)> = vec![
        ("a_id".into(), "BIGINT".into()),
        ("b_id".into(), "BIGINT".into()),
        ("tier_index".into(), "INTEGER".into()),
        ("a_group".into(), "BIGINT".into()),
        ("b_group".into(), "BIGINT".into()),
        ("kind".into(), "VARCHAR".into()),
        ("amb_group".into(), "VARCHAR".into()),
        ("cand_a".into(), "BIGINT".into()),
        ("cand_b".into(), "BIGINT".into()),
        ("candidate_rank".into(), "BIGINT".into()),
        ("rank_values".into(), "VARCHAR".into()),
    ];
    for tc in &rc.tier_columns {
        link_cols.push((format!("tc_{}", tc.name), tc.ty.ty.duckdb_name()));
    }
    let cand_cols: Vec<(String, String)> = link_cols
        .iter()
        .filter(|(n, _)| !matches!(n.as_str(), "kind" | "amb_group" | "cand_a" | "cand_b"))
        .cloned()
        .collect();
    setup.push(Op::CreateEmpty {
        table: links.clone(),
        columns: link_cols,
    });
    setup.push(Op::CreateEmpty {
        table: cands.clone(),
        columns: cand_cols,
    });
    for d in [n.t("dups_a"), n.t("dups_b")] {
        scratch.push(d.clone());
        setup.push(Op::CreateEmpty {
            table: d,
            columns: vec![
                ("id".into(), "BIGINT".into()),
                ("dup_of".into(), "BIGINT".into()),
                ("tier_index".into(), "INTEGER".into()),
            ],
        });
    }
    for h in [&held_a, &held_b] {
        setup.push(Op::CreateEmpty {
            table: h.clone(),
            columns: vec![
                ("id".into(), "BIGINT".into()),
                ("status".into(), "VARCHAR".into()),
                ("dup_of".into(), "BIGINT".into()),
                ("tier_index".into(), "INTEGER".into()),
            ],
        });
    }

    let mut tiers = Vec::new();
    for (i, tier) in rc.tiers.iter().enumerate() {
        tiers.push(lower_tier(rc, tier, i + 1, &n, &mut scratch));
    }
    let outputs = outputs(rc, &n);
    let invariant = invariant(rc, &n);
    ReconcileProgram {
        name: rc.name.clone(),
        setup,
        identity_checks,
        tiers,
        outputs,
        invariant,
        scratch,
    }
}

/// Rows still available to a tier.
fn remaining(table: &str, held: &str, links: &str, consumed: bool, id_col: &str) -> LogicalPlan {
    let mut p = scan(table).anti(scan(held), eq(c(ID), c1("id")));
    if consumed {
        p = p.anti(
            scan(links).filter(eq(c("kind"), s("match"))),
            eq(c(ID), c1(id_col)),
        );
    }
    p
}

fn lower_tier(
    rc: &Reconcile,
    tier: &Tier,
    index: usize,
    n: &Names,
    scratch: &mut Vec<String>,
) -> TierProgram {
    if let Some(sub) = &tier.subset {
        return subset::lower_tier(rc, tier, sub, index, n, scratch);
    }
    let p = |x: &str| n.t(&format!("t{index}_{x}"));
    let (ua, ub, ma, mb, cand) = (p("ua"), p("ub"), p("ma"), p("mb"), p("cand"));
    let (links, held_a, held_b, cands) = (n.t("links"), n.t("held_a"), n.t("held_b"), n.t("cands"));
    let mut prepare = Vec::new();
    let grouped = [tier.group_a.as_ref(), tier.group_b.as_ref()];
    // Rows stay available to later tiers only on unconsumed sides, so an already matched pair
    // can come back only with `consume none`; it is never matched again.
    let exclude_pairs = !rc.consume.a() && !rc.consume.b();
    let prior_matches = || scan(&links).filter(eq(c("kind"), s("match")));

    for (slot, side, table, held, id_col, units, members) in [
        (0usize, &rc.a, n.t("a"), &held_a, "a_id", &ua, &ma),
        (1usize, &rc.b, n.t("b"), &held_b, "b_id", &ub, &mb),
    ] {
        let consumed = if slot == 0 {
            rc.consume.a()
        } else {
            rc.consume.b()
        };
        let rem = remaining(&table, held, &links, consumed, id_col);
        match grouped[slot] {
            None => {
                let mut exprs = vec![
                    named(c(ID), UNIT),
                    named(c(CLS), CLS),
                    named(c(EXACT), EXACT),
                ];
                exprs.extend(
                    side.columns
                        .iter()
                        .map(|col| named(c(&col.name), &col.name)),
                );
                let plan = if exclude_pairs {
                    // A row that is already matched cannot be paired with its earlier partners,
                    // so it is no longer interchangeable with otherwise equal rows: it gets a
                    // class of its own (negative, so it never meets a shared class).
                    let mut own = exprs.clone();
                    own[1] = named(bin(BinaryOp::Sub, int(0), c(ID)), CLS);
                    LogicalPlan::Union {
                        inputs: vec![
                            rem.clone()
                                .anti(prior_matches(), eq(c(ID), c1(id_col)))
                                .project(exprs),
                            rem.semi(prior_matches(), eq(c(ID), c1(id_col)))
                                .project(own),
                        ],
                    }
                } else {
                    rem.project(exprs)
                };
                prepare.push(Op::Create {
                    table: units.clone(),
                    plan,
                });
            }
            Some(g) => {
                // every group of remaining rows is a unit, a group of one row included; rows
                // whose group key is null belong to no group
                let keys_not_null = and_all(g.keys.iter().map(|k| not_null(c(k))).collect());
                let base = match keys_not_null {
                    Some(f) => rem.filter(f),
                    None => rem,
                };
                let groups = base
                    .clone()
                    .aggregate(
                        g.keys.iter().map(|k| named(c(k), k)).collect(),
                        g.aggs.clone(),
                    )
                    .window(vec![named(
                        win(
                            WinFunc::DenseRank,
                            None,
                            Vec::new(),
                            g.keys.iter().map(|k| asc(c(k))).collect(),
                            None,
                        ),
                        UNIT,
                    )]);
                let mut exprs = vec![
                    named(c(UNIT), UNIT),
                    named(c(UNIT), CLS),
                    named(c(UNIT), EXACT),
                ];
                exprs.extend(g.keys.iter().map(|k| named(c(k), k)));
                exprs.extend(g.aggs.iter().map(|(_, a)| named(c(a), a)));
                prepare.push(Op::Create {
                    table: units.clone(),
                    plan: groups.project(exprs),
                });
                let on = and_all(g.keys.iter().map(|k| eq(c(k), c1(k))).collect()).unwrap();
                let member_plan = base.join(
                    scan(units),
                    JoinType::Inner,
                    Some(on),
                    vec![named(c1(UNIT), UNIT), named(c(ID), ID)],
                );
                prepare.push(Op::Create {
                    table: members.clone(),
                    plan: member_plan,
                });
                scratch.push(members.clone());
            }
        }
        scratch.push(units.clone());
    }

    // ---- candidates
    let mut conds: Vec<TExpr> = tier
        .block
        .iter()
        .map(|k| eq(k.a.clone(), k.b.clone()))
        .collect();
    conds.extend(tier.requires.iter().map(|r| r.expr.clone()));
    let on = and_all(conds);
    let mut out = vec![
        named(c(UNIT), "a_unit"),
        named(c1(UNIT), "b_unit"),
        named(c(CLS), "a_cls"),
        named(c1(CLS), "b_cls"),
        named(c(EXACT), "a_exact"),
        named(c1(EXACT), "b_exact"),
    ];
    for (k, r) in tier.rank.iter().enumerate() {
        out.push(named(r.expr.clone(), &format!("r_{}", k + 1)));
    }
    for tc in &rc.tier_columns {
        let e = tier
            .evidence
            .iter()
            .find(|x| x.name == tc.name)
            .map(|x| x.expr.clone())
            .or_else(|| {
                tier.flags
                    .iter()
                    .find(|(f, ..)| f == &tc.name)
                    .map(|(_, w, _)| w.clone().unwrap_or_else(|| TExpr::lit(Lit::Bool(true))))
            })
            .unwrap_or_else(null);
        out.push(named(e, &format!("tc_{}", tc.name)));
    }
    // position of each candidate among its A unit's candidates, and the rank values as text
    let order: Vec<OrderKey> = tier
        .rank
        .iter()
        .enumerate()
        .map(|(k, r)| OrderKey {
            expr: c(&format!("r_{}", k + 1)),
            desc: r.desc,
        })
        .collect();
    let rank_values = if tier.rank.is_empty() {
        TExpr::new(
            TExprKind::TryCast {
                expr: Box::new(null()),
                ty: Type::String,
            },
            U,
        )
    } else {
        native(
            "concat_ws",
            std::iter::once(s(", "))
                .chain(tier.rank.iter().enumerate().map(|(k, r)| {
                    call(
                        "concat",
                        vec![
                            s(&format!("{}=", r.text)),
                            to_text(c(&format!("r_{}", k + 1))),
                        ],
                    )
                }))
                .collect(),
        )
    };
    let mut pairs = scan(&ua).join(scan(&ub), JoinType::Inner, on, out);
    if exclude_pairs {
        // earlier matches as unit pairs of this tier: a group candidate is excluded when any of
        // its row pairs is already matched
        let mut prior = prior_matches().project(keep(&["a_id", "b_id"]));
        if grouped[0].is_some() {
            prior = prior.join(
                scan(&ma),
                JoinType::Inner,
                Some(eq(c("a_id"), c1(ID))),
                vec![named(c1(UNIT), "a_id"), named(c("b_id"), "b_id")],
            );
        }
        if grouped[1].is_some() {
            prior = prior.join(
                scan(&mb),
                JoinType::Inner,
                Some(eq(c("b_id"), c1(ID))),
                vec![named(c("a_id"), "a_id"), named(c1(UNIT), "b_id")],
            );
        }
        pairs = pairs.anti(
            prior,
            and(eq(c("a_unit"), c1("a_id")), eq(c("b_unit"), c1("b_id"))),
        );
    }
    let cand_plan = pairs.window(vec![
        named(
            win(WinFunc::Count, None, vec![c("a_unit")], Vec::new(), None),
            "cand_a",
        ),
        named(
            win(WinFunc::Count, None, vec![c("b_unit")], Vec::new(), None),
            "cand_b",
        ),
        named(
            win(WinFunc::Rank, None, vec![c("a_unit")], order, None),
            "candidate_rank",
        ),
        named(rank_values, "rank_values"),
    ]);
    prepare.push(Op::Create {
        table: cand.clone(),
        plan: cand_plan,
    });
    scratch.push(cand.clone());
    let tc_names: Vec<String> = rc
        .tier_columns
        .iter()
        .map(|tc| format!("tc_{}", tc.name))
        .collect();
    let tc_refs: Vec<&str> = tc_names.iter().map(String::as_str).collect();
    // member-level candidate edges, for `.candidates`
    let mut cand_keep: Vec<&str> = vec!["a_unit", "b_unit", "candidate_rank", "rank_values"];
    cand_keep.extend(tc_refs.iter());
    let expanded = expand(scan(&cand), &cand_keep, grouped, &ma, &mb);
    let mut cand_out = vec![
        named(c("a_id"), "a_id"),
        named(c("b_id"), "b_id"),
        named(int(index as i64), "tier_index"),
        named(c("a_group"), "a_group"),
        named(c("b_group"), "b_group"),
    ];
    cand_out.extend(keep(&["candidate_rank", "rank_values"]));
    cand_out.extend(keep(&tc_refs));
    prepare.push(Op::Insert {
        table: cands.clone(),
        plan: expanded.project(cand_out),
    });

    let rank_order = |col_prefix: &str| -> Vec<OrderKey> {
        tier.rank
            .iter()
            .enumerate()
            .map(|(k, r)| OrderKey {
                expr: c(&format!("{col_prefix}{}", k + 1)),
                desc: r.desc,
            })
            .collect()
    };
    let r_names: Vec<String> = (1..=tier.rank.len()).map(|k| format!("r_{k}")).collect();
    let r_refs: Vec<&str> = r_names.iter().map(String::as_str).collect();

    // links insertion for selected unit pairs (plan with a_unit, b_unit)
    let link_insert = |sel: LogicalPlan, kind: &str, amb_group: Option<TExpr>| -> Op {
        let mut keep_cols = vec![
            "a_unit",
            "b_unit",
            "cand_a",
            "cand_b",
            "candidate_rank",
            "rank_values",
        ];
        keep_cols.extend(tc_refs.iter());
        let with_cand = if kind == "match" {
            sel.join(
                scan(&cand),
                JoinType::Inner,
                Some(and(
                    eq(c("a_unit"), c1("a_unit")),
                    eq(c("b_unit"), c1("b_unit")),
                )),
                [keep(&["a_unit", "b_unit"]), keep1(&keep_cols[2..])].concat(),
            )
        } else {
            let mut kept = keep(&["a_unit", "b_unit", "amb_group"]);
            kept.extend(keep1(&keep_cols[2..]));
            sel.join(
                scan(&cand),
                JoinType::Inner,
                Some(and(
                    eq(c("a_unit"), c1("a_unit")),
                    eq(c("b_unit"), c1("b_unit")),
                )),
                kept,
            )
        };
        let mut ek = keep_cols.clone();
        if kind != "match" {
            ek.push("amb_group");
        }
        let expanded = expand(with_cand, &ek, grouped, &ma, &mb);
        let mut o = vec![
            named(c("a_id"), "a_id"),
            named(c("b_id"), "b_id"),
            named(int(index as i64), "tier_index"),
            named(c("a_group"), "a_group"),
            named(c("b_group"), "b_group"),
            named(s(kind), "kind"),
            named(
                if kind == "match" {
                    null()
                } else {
                    amb_group.unwrap_or_else(|| c("amb_group"))
                },
                "amb_group",
            ),
            named(c("cand_a"), "cand_a"),
            named(c("cand_b"), "cand_b"),
            named(c("candidate_rank"), "candidate_rank"),
            named(c("rank_values"), "rank_values"),
        ];
        o.extend(keep(&tc_refs));
        Op::Insert {
            table: links.clone(),
            plan: expanded.project(o),
        }
    };
    // hold member rows of units
    let hold = |units_plan: LogicalPlan, slot: usize, status: &str, dup_of: Option<&str>| -> Op {
        let (members, held) = if slot == 0 {
            (&ma, &held_a)
        } else {
            (&mb, &held_b)
        };
        let rows = match grouped[slot] {
            None => units_plan.project(vec![
                named(c("unit"), "id"),
                named(dup_of.map(c).unwrap_or_else(null), "dup_of"),
            ]),
            Some(_) => units_plan.join(
                scan(members),
                JoinType::Inner,
                Some(eq(c("unit"), c1(UNIT))),
                vec![named(c1(ID), "id"), named(null(), "dup_of")],
            ),
        };
        Op::Insert {
            table: held.clone(),
            plan: rows.project(vec![
                named(c("id"), "id"),
                named(s(status), "status"),
                named(c("dup_of"), "dup_of"),
                named(int(index as i64), "tier_index"),
            ]),
        }
    };

    let mode = match rc.cardinality {
        Cardinality::OneToOne => {
            let (done_a, done_b) = (p("done_a"), p("done_b"));
            let (ce, best, mutual, rma, rmb, pairs, sel, tied, tied_units_a, tied_units_b) = (
                p("ce"),
                p("best"),
                p("mutual"),
                p("rma"),
                p("rmb"),
                p("pairs"),
                p("sel"),
                p("tied"),
                p("tua"),
                p("tub"),
            );
            scratch.extend(
                [
                    &done_a,
                    &done_b,
                    &ce,
                    &best,
                    &mutual,
                    &rma,
                    &rmb,
                    &pairs,
                    &sel,
                    &tied,
                    &tied_units_a,
                    &tied_units_b,
                ]
                .map(|x| x.to_string()),
            );
            prepare.push(Op::CreateEmpty {
                table: done_a.clone(),
                columns: vec![("unit".into(), "BIGINT".into())],
            });
            prepare.push(Op::CreateEmpty {
                table: done_b.clone(),
                columns: vec![("unit".into(), "BIGINT".into())],
            });

            let active = |plan: LogicalPlan| {
                plan.anti(scan(&done_a), eq(c("a_unit"), c1("unit")))
                    .anti(scan(&done_b), eq(c("b_unit"), c1("unit")))
            };
            let mut round = Vec::new();
            let mut ce_cols = vec!["a_cls", "b_cls"];
            ce_cols.extend(r_refs.iter());
            round.push(Op::Create {
                table: ce.clone(),
                plan: active(scan(&cand)).project(keep(&ce_cols)).distinct(),
            });
            let best_plan = scan(&ce)
                .window(vec![
                    named(
                        win(
                            WinFunc::Rank,
                            None,
                            vec![c("a_cls")],
                            rank_order("r_"),
                            None,
                        ),
                        "ra",
                    ),
                    named(
                        win(
                            WinFunc::Rank,
                            None,
                            vec![c("b_cls")],
                            rank_order("r_"),
                            None,
                        ),
                        "rb",
                    ),
                ])
                .window(vec![
                    named(
                        win(
                            WinFunc::Count,
                            None,
                            vec![c("a_cls")],
                            Vec::new(),
                            Some(eq(c("ra"), int(1))),
                        ),
                        "na_best",
                    ),
                    named(
                        win(
                            WinFunc::Count,
                            None,
                            vec![c("b_cls")],
                            Vec::new(),
                            Some(eq(c("rb"), int(1))),
                        ),
                        "nb_best",
                    ),
                ]);
            round.push(Op::Create {
                table: best.clone(),
                plan: best_plan,
            });
            let unique_best = and_all(vec![
                eq(c("ra"), int(1)),
                eq(c("rb"), int(1)),
                eq(c("na_best"), int(1)),
                eq(c("nb_best"), int(1)),
            ])
            .unwrap();
            let not = |e: TExpr| {
                t(TExprKind::Unary {
                    op: crate::ast::UnaryOp::Not,
                    expr: Box::new(e),
                })
            };
            // Decision groups: a mutual best pair of classes, or a *star*: a class (the centre)
            // whose best candidates are tied across several classes of the other side (the
            // leaves), each of which has the centre as its unique best candidate. `grp` names a
            // group by one of its classes: 2 * the A class of a pair or of a star centred on
            // side A, 2 * the B class + 1 of a star centred on side B. A class is in at most one
            // group: a leaf has a unique best candidate, a centre several.
            let pair_edges = scan(&best).filter(unique_best).project(vec![
                named(c("a_cls"), "a_cls"),
                named(c("b_cls"), "b_cls"),
                named(bin(BinaryOp::Mul, int(2), c("a_cls")), "grp"),
                named(TExpr::lit(Lit::Bool(false)), "star"),
            ]);
            // stars centred on side A (slot 0) or side B (slot 1)
            let star_edges = |slot: i64| {
                let (centre, r_self, n_self, r_other, n_other) = if slot == 0 {
                    ("a_cls", "ra", "na_best", "rb", "nb_best")
                } else {
                    ("b_cls", "rb", "nb_best", "ra", "na_best")
                };
                let leaf = and(eq(c(r_other), int(1)), eq(c(n_other), int(1)));
                scan(&best)
                    .filter(and(
                        eq(c(r_self), int(1)),
                        bin(BinaryOp::Gt, c(n_self), int(1)),
                    ))
                    .window(vec![named(
                        win(
                            WinFunc::Count,
                            None,
                            vec![c(centre)],
                            Vec::new(),
                            Some(leaf),
                        ),
                        "leaves",
                    )])
                    .filter(eq(c("leaves"), c(n_self)))
                    .project(vec![
                        named(c("a_cls"), "a_cls"),
                        named(c("b_cls"), "b_cls"),
                        named(
                            bin(
                                BinaryOp::Add,
                                bin(BinaryOp::Mul, int(2), c(centre)),
                                int(slot),
                            ),
                            "grp",
                        ),
                        named(TExpr::lit(Lit::Bool(true)), "star"),
                    ])
            };
            round.push(Op::Create {
                table: mutual.clone(),
                plan: LogicalPlan::Union {
                    inputs: vec![pair_edges, star_edges(0), star_edges(1)],
                },
            });
            // Remaining members of each group, numbered in identity order within the group.
            // Rows an earlier tier matched (still available on a side that is not consumed) come
            // last: they pair only when no unmatched member is left, so an unmatched exact
            // duplicate is never reported as surplus next to them, and they are never leftovers
            // themselves (see the commit).
            for (slot, units, done, cls_col, id_col, target) in [
                (0usize, &ua, &done_a, "a_cls", "a_id", &rma),
                (1usize, &ub, &done_b, "b_cls", "b_id", &rmb),
            ] {
                let classes = scan(&mutual)
                    .project(vec![named(c(cls_col), "cls"), named(c("grp"), "grp")])
                    .distinct();
                // internal tables below carry no user columns and use plain names
                let members = scan(units).anti(scan(done), eq(c(UNIT), c1("unit"))).join(
                    classes,
                    JoinType::Inner,
                    Some(eq(c(CLS), c1("cls"))),
                    vec![
                        named(c(UNIT), "unit"),
                        named(c(CLS), "cls"),
                        named(c(EXACT), "exact"),
                        named(c1("grp"), "grp"),
                    ],
                );
                // a group unit is never an exact duplicate of another (its exact key is its own)
                let members = if grouped[slot].is_some() {
                    members.project(vec![
                        named(c("unit"), "unit"),
                        named(c("cls"), "cls"),
                        named(c("exact"), "exact"),
                        named(c("grp"), "grp"),
                        named(TExpr::lit(Lit::Bool(false)), "seen"),
                    ])
                } else {
                    members.join(
                        prior_matches()
                            .project(vec![named(c(id_col), "id")])
                            .distinct(),
                        JoinType::Left,
                        Some(eq(c("unit"), c1("id"))),
                        vec![
                            named(c("unit"), "unit"),
                            named(c("cls"), "cls"),
                            named(c("exact"), "exact"),
                            named(c("grp"), "grp"),
                            named(not_null(c1("id")), "seen"),
                        ],
                    )
                };
                let plan = members.window(vec![named(
                    win(
                        WinFunc::RowNumber,
                        None,
                        vec![c("grp")],
                        vec![asc(c("seen")), asc(c("unit"))],
                        None,
                    ),
                    "rn",
                )]);
                round.push(Op::Create {
                    table: target.clone(),
                    plan,
                });
            }
            // per group and side: remaining members, whether they are all exact duplicates of
            // each other, and the first of them
            let side_summary = |t: &str, side: &str| {
                scan(t).aggregate(
                    vec![named(c("grp"), "grp")],
                    vec![
                        named(agg(AggFunc::Count, None), &format!("n{side}")),
                        named(
                            eq(
                                agg(AggFunc::Min, Some(c("exact"))),
                                agg(AggFunc::Max, Some(c("exact"))),
                            ),
                            &format!("{side}_exact"),
                        ),
                        // the member paired first; leftovers are duplicates of it
                        named(
                            agg(
                                AggFunc::Min,
                                Some(case(vec![(eq(c("rn"), int(1)), c("unit"))], None)),
                            ),
                            &format!("{side}_first"),
                        ),
                    ],
                )
            };
            // A group is decided when both sides have as many members, or the larger side's
            // members are exact duplicates. An undecided pair is ambiguous at once; an undecided
            // star is left to the tie step.
            let pairs_plan = scan(&mutual)
                .project(keep(&["grp", "star"]))
                .distinct()
                .join(
                    side_summary(&rma, "a"),
                    JoinType::Inner,
                    Some(eq(c("grp"), c1("grp"))),
                    [keep(&["grp", "star"]), keep1(&["na", "a_exact", "a_first"])].concat(),
                )
                .join(
                    side_summary(&rmb, "b"),
                    JoinType::Inner,
                    Some(eq(c("grp"), c1("grp"))),
                    [
                        keep(&["grp", "star", "na", "a_exact", "a_first"]),
                        keep1(&["nb", "b_exact", "b_first"]),
                    ]
                    .concat(),
                )
                .project(vec![
                    named(c("grp"), "grp"),
                    named(c("star"), "star"),
                    named(c("na"), "na"),
                    named(c("nb"), "nb"),
                    named(c("a_first"), "a_first"),
                    named(c("b_first"), "b_first"),
                    named(
                        or(
                            eq(c("na"), c("nb")),
                            or(
                                and(bin(BinaryOp::Gt, c("na"), c("nb")), c("a_exact")),
                                and(bin(BinaryOp::Gt, c("nb"), c("na")), c("b_exact")),
                            ),
                        ),
                        "ok",
                    ),
                ])
                .filter(or(c("ok"), not(c("star"))));
            round.push(Op::Create {
                table: pairs.clone(),
                plan: pairs_plan,
            });

            // commit: pair members in identity order, record duplicates and ambiguous class pairs
            let mut commit = Vec::new();
            let sel_plan = scan(&pairs)
                .filter(c("ok"))
                .join(
                    scan(&rma),
                    JoinType::Inner,
                    Some(eq(c("grp"), c1("grp"))),
                    vec![
                        named(c("grp"), "grp"),
                        named(c1("unit"), "a_unit"),
                        named(c1("rn"), "rn"),
                    ],
                )
                .join(
                    scan(&rmb),
                    JoinType::Inner,
                    Some(and(eq(c("grp"), c1("grp")), eq(c("rn"), c1("rn")))),
                    vec![named(c("a_unit"), "a_unit"), named(c1("unit"), "b_unit")],
                );
            commit.push(Op::Create {
                table: sel.clone(),
                plan: sel_plan,
            });
            commit.push(link_insert(scan(&sel), "match", None));
            // leftovers of the larger side, exact duplicates of its first member; a leftover an
            // earlier tier matched is matched, not surplus, and stays available
            for (slot, members, n_self, n_other, first) in [
                (0usize, &rma, "na", "nb", "a_first"),
                (1usize, &rmb, "nb", "na", "b_first"),
            ] {
                let leftovers = scan(members).filter(not(c("seen"))).join(
                    scan(&pairs).filter(and(c("ok"), bin(BinaryOp::Gt, c(n_self), c(n_other)))),
                    JoinType::Inner,
                    Some(and(
                        eq(c("grp"), c1("grp")),
                        bin(BinaryOp::Gt, c("rn"), c1(n_other)),
                    )),
                    vec![named(c("unit"), "unit"), named(c1(first), "first")],
                );
                // always recorded, so an exact duplicate that stays unmatched is reported as one
                let dups = n.t(if slot == 0 { "dups_a" } else { "dups_b" });
                commit.push(Op::Insert {
                    table: dups,
                    plan: leftovers.clone().project(vec![
                        named(c("unit"), "id"),
                        named(c("first"), "dup_of"),
                        named(int(index as i64), "tier_index"),
                    ]),
                });
                if rc.duplicates == Hold::Hold {
                    commit.push(hold(leftovers.clone(), slot, "duplicate", Some("first")));
                    let done = if slot == 0 { &done_a } else { &done_b };
                    commit.push(Op::Insert {
                        table: done.clone(),
                        plan: leftovers.project(keep(&["unit"])),
                    });
                }
            }
            // class pairs whose leftover choice would be arbitrary
            let amb_pairs = scan(&pairs).filter(not(c("ok")));
            let amb_edges = amb_pairs
                .clone()
                .join(
                    scan(&rma),
                    JoinType::Inner,
                    Some(eq(c("grp"), c1("grp"))),
                    vec![
                        named(c("grp"), "grp"),
                        named(c1("cls"), "a_cls"),
                        named(c1("unit"), "a_unit"),
                    ],
                )
                .join(
                    scan(&rmb),
                    JoinType::Inner,
                    Some(eq(c("grp"), c1("grp"))),
                    vec![
                        named(c("a_unit"), "a_unit"),
                        named(c1("unit"), "b_unit"),
                        named(
                            call(
                                "concat",
                                vec![s("p"), to_text(c("a_cls")), s("-"), to_text(c1("cls"))],
                            ),
                            "amb_group",
                        ),
                    ],
                );
            commit.push(link_insert(amb_edges, "ambiguous", None));
            for (slot, members, done) in [(0usize, &rma, &done_a), (1usize, &rmb, &done_b)] {
                let units = scan(members)
                    .semi(amb_pairs.clone(), eq(c("grp"), c1("grp")))
                    .project(keep(&["unit"]));
                if rc.ambiguity == Hold::Hold {
                    commit.push(hold(units.clone(), slot, "ambiguous", None));
                }
                commit.push(Op::Insert {
                    table: done.clone(),
                    plan: units,
                });
            }
            commit.push(Op::Insert {
                table: done_a.clone(),
                plan: scan(&sel).project(vec![named(c("a_unit"), "unit")]),
            });
            commit.push(Op::Insert {
                table: done_b.clone(),
                plan: scan(&sel).project(vec![named(c("b_unit"), "unit")]),
            });

            // tie step: classes whose best candidates are tied. An edge tied from both sides
            // belongs to both decisions, so it is recorded once per ambiguity group.
            let mut tie = Vec::new();
            let a_tied = and(eq(c("ra"), int(1)), bin(BinaryOp::Gt, c("na_best"), int(1)));
            let b_tied = and(eq(c("rb"), int(1)), bin(BinaryOp::Gt, c("nb_best"), int(1)));
            let side = |cond: TExpr, prefix: &str, cls: &str| {
                scan(&best).filter(cond).project(vec![
                    named(c("a_cls"), "a_cls"),
                    named(c("b_cls"), "b_cls"),
                    named(
                        call("concat", vec![s(prefix), to_text(c(cls))]),
                        "amb_group",
                    ),
                ])
            };
            let tied_plan = LogicalPlan::Union {
                inputs: vec![side(a_tied, "a", "a_cls"), side(b_tied, "b", "b_cls")],
            };
            tie.push(Op::Create {
                table: tied.clone(),
                plan: tied_plan,
            });
            let mut tie_commit = Vec::new();
            for (units, done, cls_col, target) in [
                (&ua, &done_a, "a_cls", &tied_units_a),
                (&ub, &done_b, "b_cls", &tied_units_b),
            ] {
                let plan = scan(units)
                    .anti(scan(done), eq(c(UNIT), c1("unit")))
                    .semi(scan(&tied), eq(c(CLS), c1(cls_col)))
                    .project(vec![named(c(UNIT), "unit"), named(c(CLS), "cls")]);
                tie_commit.push(Op::Create {
                    table: target.clone(),
                    plan,
                });
            }
            let tie_edges = scan(&tied)
                .join(
                    scan(&tied_units_a),
                    JoinType::Inner,
                    Some(eq(c("a_cls"), c1("cls"))),
                    vec![
                        named(c("b_cls"), "b_cls"),
                        named(c("amb_group"), "amb_group"),
                        named(c1("unit"), "a_unit"),
                    ],
                )
                .join(
                    scan(&tied_units_b),
                    JoinType::Inner,
                    Some(eq(c("b_cls"), c1("cls"))),
                    vec![
                        named(c("a_unit"), "a_unit"),
                        named(c1("unit"), "b_unit"),
                        named(c("amb_group"), "amb_group"),
                    ],
                )
                .semi(
                    scan(&cand),
                    and(eq(c("a_unit"), c1("a_unit")), eq(c("b_unit"), c1("b_unit"))),
                );
            tie_commit.push(link_insert(tie_edges, "ambiguous", None));
            for (slot, target, done) in [
                (0usize, &tied_units_a, &done_a),
                (1usize, &tied_units_b, &done_b),
            ] {
                if rc.ambiguity == Hold::Hold {
                    tie_commit.push(hold(scan(target), slot, "ambiguous", None));
                }
                tie_commit.push(Op::Insert {
                    table: done.clone(),
                    plan: scan(target).project(keep(&["unit"])),
                });
            }
            TierMode::Rounds {
                round,
                progress: pairs,
                commit,
                tie,
                tied,
                tie_commit,
            }
        }
        Cardinality::ManyToMany => TierMode::Single(vec![link_insert(
            scan(&cand).project(keep(&["a_unit", "b_unit"])),
            "match",
            None,
        )]),
        Cardinality::ManyToOne | Cardinality::OneToMany => {
            // the limited side picks its best counterpart; equally good distinguishable
            // counterparts make it ambiguous, exact duplicates resolve to the lowest identity
            let (chooser, other, other_exact) = if rc.cardinality == Cardinality::ManyToOne {
                ("a_unit", "b_unit", "b_exact")
            } else {
                ("b_unit", "a_unit", "a_exact")
            };
            let ranked = p("ranked");
            let decided = p("decided");
            scratch.extend([ranked.clone(), decided.clone()]);
            let mut ops = Vec::new();
            let ranked_plan = scan(&cand)
                .window(vec![named(
                    win(
                        WinFunc::Rank,
                        None,
                        vec![c(chooser)],
                        rank_order("r_"),
                        None,
                    ),
                    "rk",
                )])
                .filter(eq(c("rk"), int(1)));
            ops.push(Op::Create {
                table: ranked.clone(),
                plan: ranked_plan,
            });
            // A chooser picks the lowest of its equally good exact duplicates, preferring rows no
            // earlier tier matched: on a side that is not consumed an already matched row is
            // still available, but choosing it would report the unmatched copy as surplus.
            let other_id = if other == "a_unit" { "a_id" } else { "b_id" };
            let all = scan(&ranked).aggregate(
                vec![named(c(chooser), chooser)],
                vec![
                    named(agg(AggFunc::Min, Some(c(other))), other),
                    named(
                        eq(
                            agg(AggFunc::Min, Some(c(other_exact))),
                            agg(AggFunc::Max, Some(c(other_exact))),
                        ),
                        "same",
                    ),
                    named(agg(AggFunc::Count, None), "n"),
                ],
            );
            let free = scan(&ranked)
                .anti(prior_matches(), eq(c(other), c1(other_id)))
                .aggregate(
                    vec![named(c(chooser), chooser)],
                    vec![named(agg(AggFunc::Min, Some(c(other))), "free")],
                );
            let decided_plan = all.join(
                free,
                JoinType::Left,
                Some(eq(c(chooser), c1(chooser))),
                vec![
                    named(c(chooser), chooser),
                    named(call("coalesce", vec![c1("free"), c(other)]), other),
                    named(c("same"), "same"),
                    named(c("n"), "n"),
                ],
            );
            ops.push(Op::Create {
                table: decided.clone(),
                plan: decided_plan,
            });
            let picked = || scan(&decided).filter(or(eq(c("n"), int(1)), c("same")));
            ops.push(link_insert(
                picked().project(keep(&["a_unit", "b_unit"])),
                "match",
                None,
            ));
            // exact duplicates a chooser passed over are duplicates of the row it chose (the
            // lowest one, if several choosers passed over the same row), unless they are matched:
            // by another chooser here, or by an earlier tier on a side that is not consumed
            let losers = scan(&ranked)
                .join(
                    scan(&decided).filter(and(bin(BinaryOp::Gt, c("n"), int(1)), c("same"))),
                    JoinType::Inner,
                    Some(and(
                        eq(c(chooser), c1(chooser)),
                        bin(BinaryOp::NotEq, c(other), c1(other)),
                    )),
                    vec![named(c(other), "unit"), named(c1(other), "first")],
                )
                .anti(prior_matches(), eq(c("unit"), c1(other_id)))
                .aggregate(
                    vec![named(c("unit"), "unit")],
                    vec![named(agg(AggFunc::Min, Some(c("first"))), "first")],
                );
            let other_slot = if other == "a_unit" { 0 } else { 1 };
            ops.push(Op::Insert {
                table: n.t(if other_slot == 0 { "dups_a" } else { "dups_b" }),
                plan: losers.clone().project(vec![
                    named(c("unit"), "id"),
                    named(c("first"), "dup_of"),
                    named(int(index as i64), "tier_index"),
                ]),
            });
            if rc.duplicates == Hold::Hold {
                ops.push(hold(losers, other_slot, "duplicate", Some("first")));
            }
            let amb = scan(&ranked)
                .semi(
                    scan(&decided).filter(and(
                        bin(BinaryOp::Gt, c("n"), int(1)),
                        t(TExprKind::Unary {
                            op: crate::ast::UnaryOp::Not,
                            expr: Box::new(c("same")),
                        }),
                    )),
                    eq(c(chooser), c1(chooser)),
                )
                .project(vec![
                    named(c("a_unit"), "a_unit"),
                    named(c("b_unit"), "b_unit"),
                    named(
                        call("concat", vec![s(&chooser[..1]), to_text(c(chooser))]),
                        "amb_group",
                    ),
                ]);
            ops.push(link_insert(amb.clone(), "ambiguous", None));
            if rc.ambiguity == Hold::Hold {
                let slot = if chooser == "a_unit" { 0 } else { 1 };
                ops.push(hold(
                    amb.project(vec![named(c(chooser), "unit")]).distinct(),
                    slot,
                    "ambiguous",
                    None,
                ));
            }
            TierMode::Single(ops)
        }
    };
    TierProgram {
        name: tier.name.clone(),
        index,
        description: tier.description(),
        prepare,
        mode,
    }
}

/// Expand unit pairs to member rows: adds `a_id`, `a_group`, `b_id`, `b_group` and keeps `cols`.
fn expand(
    plan: LogicalPlan,
    cols: &[&str],
    grouped: [Option<&Group>; 2],
    ma: &str,
    mb: &str,
) -> LogicalPlan {
    let mut plan = plan;
    let mut carried: Vec<String> = cols.iter().map(|s| s.to_string()).collect();
    for (slot, members, unit, id, group) in [
        (0usize, ma, "a_unit", "a_id", "a_group"),
        (1usize, mb, "b_unit", "b_id", "b_group"),
    ] {
        let mut out: Vec<(TExpr, String)> = carried.iter().map(|x| (c(x), x.clone())).collect();
        if slot == 1 {
            out.extend(keep(&["a_id", "a_group"]));
        }
        plan = match grouped[slot] {
            None => {
                out.push(named(c(unit), id));
                out.push(named(null(), group));
                plan.project(out)
            }
            Some(_) => {
                out.push(named(c1(ID), id));
                out.push(named(c(unit), group));
                plan.join(
                    scan(members),
                    JoinType::Inner,
                    Some(eq(c(unit), c1(UNIT))),
                    out,
                )
            }
        };
        if slot == 0 {
            carried.retain(|x| x != "a_id" && x != "a_group");
        }
    }
    plan
}

fn tier_name_expr(rc: &Reconcile) -> TExpr {
    case(
        rc.tiers
            .iter()
            .enumerate()
            .map(|(i, t)| (eq(c("tier_index"), int(i as i64 + 1)), s(&t.name)))
            .collect(),
        None,
    )
}

fn reason_expr(rc: &Reconcile) -> TExpr {
    case(
        rc.tiers
            .iter()
            .enumerate()
            .map(|(i, t)| (eq(c("tier_index"), int(i as i64 + 1)), s(&t.description())))
            .collect(),
        None,
    )
}

/// Columns for side identities in outputs (slot = where the side's row columns are).
fn identity_out(side: &Side, slot: u8, prefix: &str) -> Vec<(TExpr, String)> {
    match &side.identity {
        Identity::Synthetic => vec![(
            TExpr::col(slot, format!("{prefix}{ID}"), U),
            format!("{}_row", side.alias),
        )],
        Identity::Declared(ids) => ids
            .iter()
            .map(|x| {
                (
                    TExpr::col(slot, format!("{prefix}{x}"), U),
                    format!("{}_{x}", side.alias),
                )
            })
            .collect(),
    }
}

fn prefixed_out(side: &Side, slot: u8, prefix: &str) -> Vec<(TExpr, String)> {
    let ids = side_identity_cols(side);
    side.columns
        .iter()
        .filter(|col| !ids.contains(&col.name))
        .map(|col| {
            (
                TExpr::col(slot, format!("{prefix}{}", col.name), U),
                format!("{}_{}", side.alias, col.name),
            )
        })
        .collect()
}

/// links-like plan (with `a_id`, `b_id` and other columns) joined to both sides. Side A columns
/// arrive as `__a_<col>` (slot 0 after the first join) and side B columns as slot 1.
fn with_rows(
    rc: &Reconcile,
    n: &Names,
    base: LogicalPlan,
    base_cols: &[&str],
    output: impl Fn(&Reconcile) -> Vec<(TExpr, String)>,
) -> LogicalPlan {
    let mut first: Vec<(TExpr, String)> = keep(base_cols);
    first.push(named(c1(ID), A_ID));
    first.extend(
        rc.a.columns
            .iter()
            .map(|col| (c1(&col.name), format!("{A_PREFIX}{}", col.name))),
    );
    base.join(
        scan(&n.t("a")),
        JoinType::Inner,
        Some(eq(c("a_id"), c1(ID))),
        first,
    )
    .join(
        scan(&n.t("b")),
        JoinType::Inner,
        Some(eq(c("b_id"), c1(ID))),
        output(rc),
    )
}

fn evidence_out(rc: &Reconcile) -> Vec<(TExpr, String)> {
    let mut out = Vec::new();
    for e in rc.evidence.iter().chain(rc.flags.iter()) {
        out.push((reslot(&e.expr, (0, A_PREFIX), (1, "")), e.name.clone()));
    }
    for tc in &rc.tier_columns {
        out.push((c(&format!("tc_{}", tc.name)), tc.name.clone()));
    }
    out
}

fn outputs(rc: &Reconcile, n: &Names) -> Vec<Op> {
    let links = n.t("links");
    let name = |part: &str| format!("{}.{part}", rc.name);
    let tc_cols: Vec<String> = rc
        .tier_columns
        .iter()
        .map(|tc| format!("tc_{}", tc.name))
        .collect();
    let mut base_cols: Vec<&str> = vec![
        "a_id",
        "b_id",
        "tier_index",
        "a_group",
        "b_group",
        "kind",
        "amb_group",
        "cand_a",
        "cand_b",
        "candidate_rank",
        "rank_values",
    ];
    base_cols.extend(tc_cols.iter().map(String::as_str));
    let mut ops = Vec::new();

    // matches
    let flag_names: Vec<(TExpr, String)> = rc
        .flags
        .iter()
        .map(|f| (reslot(&f.expr, (0, A_PREFIX), (1, "")), f.name.clone()))
        .chain(
            rc.tier_columns
                .iter()
                .filter(|tc| tc.is_flag)
                .map(|tc| (c(&format!("tc_{}", tc.name)), tc.name.clone())),
        )
        .collect();
    let flags_summary = if flag_names.is_empty() {
        s("")
    } else {
        native(
            "concat_ws",
            std::iter::once(s(", "))
                .chain(flag_names.iter().map(|(e, name)| {
                    case(
                        vec![(
                            call("coalesce", vec![e.clone(), TExpr::lit(Lit::Bool(false))]),
                            s(name),
                        )],
                        None,
                    )
                }))
                .collect(),
        )
    };
    let matches = with_rows(
        rc,
        n,
        scan(&links).filter(eq(c("kind"), s("match"))),
        &base_cols,
        |rc| {
            let mut o = vec![
                named(tier_name_expr(rc), "tier"),
                named(c("tier_index"), "tier_index"),
                named(c("a_group"), "a_group"),
                named(c("b_group"), "b_group"),
            ];
            o.extend(identity_out(&rc.a, 0, A_PREFIX));
            o.extend(identity_out(&rc.b, 1, ""));
            o.extend(evidence_out(rc));
            o.push(named(flags_summary.clone(), "flags"));
            o.push(named(c("cand_a"), "candidates_a"));
            o.push(named(c("cand_b"), "candidates_b"));
            o.push(named(c("candidate_rank"), "candidate_rank"));
            o.push(named(c("rank_values"), "rank_values"));
            o.push(named(reason_expr(rc), "reason"));
            o.extend(prefixed_out(&rc.a, 0, A_PREFIX));
            o.extend(prefixed_out(&rc.b, 1, ""));
            o
        },
    );
    let order: Vec<OrderKey> = vec![asc(c("tier_index"))];
    ops.push(Op::Create {
        table: name("matches"),
        plan: matches.sort(order),
    });

    // unmatched
    for (side, table, held, dups, id_col) in [
        (&rc.a, n.t("a"), n.t("held_a"), n.t("dups_a"), "a_id"),
        (&rc.b, n.t("b"), n.t("held_b"), n.t("dups_b"), "b_id"),
    ] {
        // neither matched nor ambiguous (in an ambiguity, held or not)
        let p = scan(&table).anti(scan(&links), eq(c(ID), c1(id_col))).anti(
            scan(&held).filter(eq(c("status"), s("ambiguous"))),
            eq(c(ID), c1("id")),
        );
        let mut first = vec![named(c(ID), ID)];
        first.extend(
            side.columns
                .iter()
                .map(|col| named(c(&col.name), &col.name)),
        );
        first.push(named(c1("dup_of"), DUP_OF));
        let dup_of = scan(&dups).aggregate(
            vec![named(c("id"), "id")],
            vec![named(agg(AggFunc::Min, Some(c("dup_of"))), "dup_of")],
        );
        let with_status = p.join(dup_of, JoinType::Left, Some(eq(c(ID), c1("id"))), first);
        // identity of the duplicated row, as text
        let dup_text = match &side.identity {
            Identity::Synthetic => to_text(c1(ID)),
            Identity::Declared(ids) => native(
                "concat_ws",
                std::iter::once(s("|"))
                    .chain(ids.iter().map(|x| to_text(c1(x))))
                    .collect(),
            ),
        };
        let mut out = vec![
            named(
                case(
                    vec![(not_null(c(DUP_OF)), s("duplicate"))],
                    Some(s("unmatched")),
                ),
                "match_status",
            ),
            named(
                case(vec![(not_null(c(DUP_OF)), dup_text)], None),
                "duplicate_of",
            ),
        ];
        if matches!(side.identity, Identity::Synthetic) {
            out.push(named(c(ID), &format!("{}_row", side.alias)));
        }
        out.extend(
            side.columns
                .iter()
                .map(|col| named(c(&col.name), &col.name)),
        );
        let plan = with_status.join(
            scan(&table),
            JoinType::Left,
            Some(eq(c(DUP_OF), c1(ID))),
            out,
        );
        let part = if side.alias == rc.a.alias {
            "unmatched_a"
        } else {
            "unmatched_b"
        };
        let ids: Vec<OrderKey> = match &side.identity {
            Identity::Declared(ids) => ids.iter().map(|x| asc(c(x))).collect(),
            Identity::Synthetic => vec![asc(c(&format!("{}_row", side.alias)))],
        };
        ops.push(Op::Create {
            table: name(part),
            plan: plan.sort(ids),
        });
    }

    // candidates
    let mut cand_base: Vec<&str> = vec![
        "a_id",
        "b_id",
        "tier_index",
        "a_group",
        "b_group",
        "candidate_rank",
        "rank_values",
    ];
    cand_base.extend(tc_cols.iter().map(String::as_str));
    let mut with_outcome = keep(&cand_base);
    with_outcome.push(named(
        call("coalesce", vec![c1("kind"), s("not selected")]),
        "outcome",
    ));
    // one outcome per pair (an edge tied from both sides is recorded in two ambiguity groups)
    let outcomes = scan(&links).aggregate(
        vec![
            named(c("a_id"), "a_id"),
            named(c("b_id"), "b_id"),
            named(c("tier_index"), "tier_index"),
        ],
        vec![named(agg(AggFunc::Min, Some(c("kind"))), "kind")],
    );
    let cands = scan(&n.t("cands")).join(
        outcomes,
        JoinType::Left,
        Some(
            and_all(vec![
                eq(c("a_id"), c1("a_id")),
                eq(c("b_id"), c1("b_id")),
                eq(c("tier_index"), c1("tier_index")),
            ])
            .unwrap(),
        ),
        with_outcome,
    );
    let mut cand_cols = cand_base.clone();
    cand_cols.push("outcome");
    let candidates = with_rows(rc, n, cands, &cand_cols, |rc| {
        let mut o = vec![
            named(tier_name_expr(rc), "tier"),
            named(c("tier_index"), "tier_index"),
            named(c("a_group"), "a_group"),
            named(c("b_group"), "b_group"),
        ];
        o.extend(identity_out(&rc.a, 0, A_PREFIX));
        o.extend(identity_out(&rc.b, 1, ""));
        o.push(named(
            case(
                vec![(eq(c("outcome"), s("match")), s("matched"))],
                Some(c("outcome")),
            ),
            "outcome",
        ));
        o.push(named(c("candidate_rank"), "candidate_rank"));
        o.push(named(c("rank_values"), "rank_values"));
        o.extend(evidence_out(rc));
        o
    });
    ops.push(Op::Create {
        table: name("candidates"),
        plan: candidates.sort(vec![asc(c("tier_index"))]),
    });

    // ambiguous
    let amb = with_rows(
        rc,
        n,
        scan(&links).filter(eq(c("kind"), s("ambiguous"))),
        &base_cols,
        |rc| {
            let mut o = vec![
                named(tier_name_expr(rc), "tier"),
                named(c("tier_index"), "tier_index"),
                named(
                    win(
                        WinFunc::DenseRank,
                        None,
                        Vec::new(),
                        vec![asc(c("tier_index")), asc(c("amb_group"))],
                        None,
                    ),
                    "ambiguity_id",
                ),
                named(c("a_group"), "a_group"),
                named(c("b_group"), "b_group"),
            ];
            o.extend(identity_out(&rc.a, 0, A_PREFIX));
            o.extend(identity_out(&rc.b, 1, ""));
            o.extend(evidence_out(rc));
            o.extend(prefixed_out(&rc.a, 0, A_PREFIX));
            o.extend(prefixed_out(&rc.b, 1, ""));
            o
        },
    );
    ops.push(Op::Create {
        table: name("ambiguous"),
        plan: amb.sort(vec![asc(c("tier_index")), asc(c("ambiguity_id"))]),
    });

    // summary
    let row = |step: TExpr, tier: TExpr, pairs: LogicalPlan| (step, tier, pairs);
    let counts = |p: LogicalPlan| {
        p.aggregate(
            Vec::new(),
            vec![
                named(agg(AggFunc::Count, None), "pairs"),
                named(agg(AggFunc::CountDistinct, Some(c("a_id"))), "a_rows"),
                named(agg(AggFunc::CountDistinct, Some(c("b_id"))), "b_rows"),
            ],
        )
    };
    let mut parts = Vec::new();
    for (i, tier) in rc.tiers.iter().enumerate() {
        let p = counts(scan(&links).filter(and(
            eq(c("kind"), s("match")),
            eq(c("tier_index"), int(i as i64 + 1)),
        )));
        parts.push(row(s(&tier.name), int(i as i64 + 1), p));
    }
    let count_rows = |p: LogicalPlan, col: &str| {
        p.aggregate(Vec::new(), vec![named(agg(AggFunc::Count, None), col)])
    };
    // distinct tied pairs (a pair tied from both sides is in two decisions), and the rows whose
    // final status is ambiguous
    let amb_pairs = count_rows(
        scan(&links)
            .filter(eq(c("kind"), s("ambiguous")))
            .project(keep(&["a_id", "b_id"]))
            .distinct(),
        "pairs",
    );
    let amb_rows = cross(
        count_rows(ambiguous_rows(&links, "a_id"), "a_rows"),
        count_rows(ambiguous_rows(&links, "b_id"), "b_rows"),
        vec![named(c("a_rows"), "a_rows"), named(c1("b_rows"), "b_rows")],
    );
    parts.push(row(
        s("ambiguous"),
        null(),
        cross(
            amb_pairs,
            amb_rows,
            vec![
                named(c("pairs"), "pairs"),
                named(c1("a_rows"), "a_rows"),
                named(c1("b_rows"), "b_rows"),
            ],
        ),
    ));
    let pair_of = |a: LogicalPlan, b: LogicalPlan| {
        cross(
            a,
            b,
            vec![
                named(int(0), "pairs"),
                named(c("a_rows"), "a_rows"),
                named(c1("b_rows"), "b_rows"),
            ],
        )
    };
    let by_status = |part: &str, status: &str, col: &str| {
        count_rows(
            scan(&name(part)).filter(eq(c("match_status"), s(status))),
            col,
        )
    };
    parts.push(row(
        s("duplicate"),
        null(),
        pair_of(
            by_status("unmatched_a", "duplicate", "a_rows"),
            by_status("unmatched_b", "duplicate", "b_rows"),
        ),
    ));
    let unmatched = |part: &str, col: &str| by_status(part, "unmatched", col);
    parts.push(row(
        s("unmatched"),
        null(),
        pair_of(
            unmatched("unmatched_a", "a_rows"),
            unmatched("unmatched_b", "b_rows"),
        ),
    ));
    parts.push(row(
        s("total"),
        null(),
        pair_of(
            count_rows(scan(&n.t("a")), "a_rows"),
            count_rows(scan(&n.t("b")), "b_rows"),
        ),
    ));
    // tiers first, then ambiguous, duplicate, unmatched, total
    let parts = parts
        .into_iter()
        .enumerate()
        .map(|(k, (step, tier, pairs))| {
            pairs.project(vec![
                named(int(k as i64 + 1), "position"),
                named(step, "step"),
                named(tier, "tier_index"),
                named(c("pairs"), "pairs"),
                named(c("a_rows"), "a_rows"),
                named(c("b_rows"), "b_rows"),
            ])
        })
        .collect();
    let summary = LogicalPlan::Union { inputs: parts };
    ops.push(Op::Create {
        table: name("summary"),
        plan: summary,
    });
    ops
}

/// Rows of one side (column `id`) whose final status is ambiguous: in an ambiguity, not matched.
fn ambiguous_rows(links: &str, id_col: &str) -> LogicalPlan {
    scan(links)
        .filter(eq(c("kind"), s("ambiguous")))
        .anti(
            scan(links).filter(eq(c("kind"), s("match"))),
            eq(c(id_col), c1(id_col)),
        )
        .project(vec![named(c(id_col), "id")])
        .distinct()
}

/// Single-row plans with one column `n` each (see `agg_count`), side by side in one row with
/// the given column names.
fn one_row(values: Vec<(LogicalPlan, &str)>) -> LogicalPlan {
    let mut values = values.into_iter();
    let (first_plan, first) = values.next().expect("at least one value");
    let mut plan = first_plan.project(vec![named(c("n"), first)]);
    let mut cols = vec![first];
    for (p, col) in values {
        let mut out = keep(&cols);
        out.push(named(c1("n"), col));
        plan = cross(plan, p, out);
        cols.push(col);
    }
    plan
}

/// For every policy, each input row has exactly one final status — matched, ambiguous,
/// duplicate or unmatched — is held at most once, and a row held as a duplicate is never matched
/// (a surplus copy, not a row some tier matched). With `one_to_one` and `consume both` a matched
/// row is moreover in exactly one match (one pair, or one rollup group) and never held.
fn invariant(rc: &Reconcile, n: &Names) -> LogicalPlan {
    let links = n.t("links");
    let exclusive = rc.cardinality == Cardinality::OneToOne && rc.consume == Consume::Both;
    let side = |table: &str, held: &str, id_col: &str, part: &str| {
        let matches = || scan(&links).filter(eq(c("kind"), s("match")));
        let count = |p: LogicalPlan| agg_count(p, None);
        let distinct = |p: LogicalPlan, col: &str| agg_count(p, Some(col));
        let status = |st: &str| {
            count(scan(&format!("{}.{part}", rc.name)).filter(eq(c("match_status"), s(st))))
        };
        let mut values = vec![
            (distinct(matches(), id_col), "m"),
            (count(ambiguous_rows(&links, id_col)), "a"),
            (status("duplicate"), "d"),
            (status("unmatched"), "u"),
            (count(scan(table)), "t"),
            (distinct(scan(held), "id"), "h"),
            (count(scan(held)), "hr"),
        ];
        let sum = [c("m"), c("a"), c("d"), c("u")]
            .into_iter()
            .reduce(|x, y| bin(BinaryOp::Add, x, y))
            .unwrap();
        // held as a surplus copy, yet matched (in an earlier tier or a later one)
        values.push((
            distinct(
                matches().semi(
                    scan(held).filter(eq(c("status"), s("duplicate"))),
                    eq(c(id_col), c1("id")),
                ),
                id_col,
            ),
            "dm",
        ));
        let mut ok = vec![eq(sum, c("t")), eq(c("h"), c("hr")), eq(c("dm"), int(0))];
        if exclusive {
            let (other_id, other_group) = if id_col == "a_id" {
                ("b_id", "b_group")
            } else {
                ("a_id", "a_group")
            };
            // matched and held at the same time
            values.push((
                distinct(matches().semi(scan(held), eq(c(id_col), c1("id"))), id_col),
                "mh",
            ));
            // matched to more than one counterpart (a pair, or a rollup group, counts once)
            let key = call(
                "concat",
                vec![
                    to_text(c("tier_index")),
                    s("-"),
                    to_text(call("coalesce", vec![c(other_group), c(other_id)])),
                ],
            );
            let multi = matches()
                .aggregate(
                    vec![named(c(id_col), id_col)],
                    vec![named(agg(AggFunc::CountDistinct, Some(key)), "k")],
                )
                .filter(bin(BinaryOp::Gt, c("k"), int(1)));
            values.push((count(multi), "mm"));
            ok.push(eq(c("mh"), int(0)));
            ok.push(eq(c("mm"), int(0)));
        }
        one_row(values).project(vec![named(and_all(ok).unwrap(), "ok")])
    };
    let a = side(&n.t("a"), &n.t("held_a"), "a_id", "unmatched_a");
    let b = side(&n.t("b"), &n.t("held_b"), "b_id", "unmatched_b");
    cross(a, b, vec![named(and(c("ok"), c1("ok")), "ok")])
}

/// One row, column `n`: the number of rows, or of distinct non-null values of `col`.
fn agg_count(p: LogicalPlan, col: Option<&str>) -> LogicalPlan {
    let f = match col {
        Some(col) => agg(AggFunc::CountDistinct, Some(c(col))),
        None => agg(AggFunc::Count, None),
    };
    p.aggregate(Vec::new(), vec![named(f, "n")])
}
