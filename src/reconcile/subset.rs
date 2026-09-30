//! Lowering of subset tiers (`subset b max_items 5`): a unit of the other side (a
//! remaining row, or a group when that side is grouped, as in rollup tiers) matches a
//! combination of up to `max_items` remaining rows of the subset side.
//!
//! 1. **Members.** (unit, row) pairs that share the blocking keys and pass every member-level
//!    `require` (one without aggregates over the subset side). With `consume none` a pair that is
//!    already matched (for a group: any of its rows with that row) is no member.
//! 2. **Budget.** Without a sum target (below), before enumerating the run counts Σ over units
//!    of Σ_{k=1..max_items} C(n, k) for a unit with n members (an upper bound) and stops (M315)
//!    when that exceeds `max_subsets`. With a subset-level `require sum(x) == t` over an exact
//!    `x` ([`Subset::sum_target`]), subsets are built level by level (size 1, 2, ...) and a
//!    partial subset of k members is extended only while at most `max_items - k` members after
//!    its last one can still bring its sum to `t`; before each level the run adds the (subset,
//!    member) pairs that level examines to a total and stops (M315) as soon as the total exceeds
//!    `max_subsets`.
//! 3. **Candidates.** Non-empty subsets of one unit's members with at most `max_items` rows that
//!    pass every subset-level `require`, with its aggregates computed over the subset's rows.
//!    A partial subset pruned by the sum target is never a candidate, nor is any subset it
//!    would have grown into: each of those sums differs from `t`.
//!    Rows that are exact duplicates (equal in every column but identity, and on the identity
//!    columns the tier reads other than by `count` of a column or `count_distinct` of a
//!    single-column identity) are *copies* of one class: subsets that differ only by exchanging
//!    copies are one candidate, and only its representative is built, which uses the first
//!    copies (rows not matched by an earlier tier first, then identity order). With `consume
//!    none` a row matched before is a class of its own. Members are numbered within their unit
//!    with copies next to each other, and a subset lists them in increasing order, so every
//!    combination is built once. A candidate's id (the subset side's `a_group`/`b_group`)
//!    numbers candidates in a fixed order of unit and members; pruning keeps that order and
//!    drops no candidate, so ids are the same with or without it.
//! 4. **Selection.** `rank by` orders a unit's candidates; its best are those ranked first (all
//!    of them without `rank by`). Per class, units whose best subsets use it take consecutive
//!    copies in unit order (as many as their most demanding best subset needs). A class whose
//!    units need more copies than there are is contested. A unit with exactly one best subset
//!    and no contested class matches that subset with its own copies; every other unit is
//!    ambiguous (one ambiguity per unit: its rows × the rows of each of its best subsets, with
//!    every copy of a contested class). A row that is only in lower-ranked candidates of other
//!    units does not conflict.
//! 5. **Duplicates.** Copies of a class used by a matched unit that no unit's share includes and
//!    that no earlier tier matched are duplicates of a matched copy (held with `duplicates
//!    hold`).
//!
//! Nothing depends on storage order: members, subsets, shares and ids are ordered by column
//! values and identity; which copy a unit gets is a choice among interchangeable rows only.

use super::*;
use crate::syntax::span::Span;

/// Run-time part of a subset tier.
#[derive(Debug, Clone)]
pub struct Program {
    pub max_items: u32,
    pub max_subsets: u64,
    /// The `subset` clause, for the budget error.
    pub span: Span,
    /// One row per member count: `n` (members of a unit) and `units` (units with that many), for
    /// the log and the upfront bound without pruning.
    pub budget: LogicalPlan,
    /// With a sum target: prune by it and count examined subsets level by level.
    pub pruning: Option<Pruning>,
    /// `Op::Create` of the subsets with 1..=max_items members, in order.
    pub levels: Vec<Op>,
    /// Union of the levels, per-subset aggregates and selection, run after the levels.
    pub ops: Vec<Op>,
}

/// Enumeration pruned by a subset-level `sum(x) == t` (see [`Subset::sum_target`]).
#[derive(Debug, Clone)]
pub struct Pruning {
    /// The require the sum target comes from, for plan, SQL and log text.
    pub require: String,
    /// `fanout[k - 1]`: the subsets level k examines (one row, column `fanout`).
    pub fanout: Vec<LogicalPlan>,
}

/// Non-empty subsets of at most `max_items` of `n` members: Σ_{k=1..max_items} C(n, k),
/// saturating at `u64::MAX`.
pub fn subsets(n: u64, max_items: u32) -> u64 {
    let mut total = 0u64;
    // C(n, k - 1)
    let mut c: u128 = 1;
    for k in 1..=u64::from(max_items).min(n) {
        // C(n, k) = C(n, k - 1) · (n - k + 1) / k, exact in this order
        c = match c.checked_mul(u128::from(n - k + 1)) {
            Some(x) => x / u128::from(k),
            None => return u64::MAX,
        };
        total = total.saturating_add(u64::try_from(c).unwrap_or(u64::MAX));
        if total == u64::MAX {
            break;
        }
    }
    total
}

/// The most members a single unit may have within `max_subsets`.
pub fn largest_unit(max_items: u32, max_subsets: u64) -> u64 {
    // `subsets(n, _) >= n`, so the answer is at most `max_subsets`
    let (mut lo, mut hi) = (0u64, max_subsets);
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if subsets(mid, max_items) <= max_subsets {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

/// The internal subset key while aggregates are computed next to the side's own columns.
const SUBSET: &str = "__magi_subset";
/// Rows of the subset side that are interchangeable in this tier share this key.
const EQUIV: &str = "__magi_equiv";
/// Rows of the subset side that an earlier tier matched.
const PRIOR: &str = "__magi_prior";

/// Identity columns of the subset side that the tier reads in a way that exchanging two rows
/// equal in every other column could change: blocking keys, member-level requires, and subset
/// aggregates. Counting an identity column gives the number of rows whichever rows they are
/// (identity columns are never null), and so does counting the distinct values of the whole
/// identity when it is a single column; any other use of an identity column (including the
/// distinct values of one column of a composite identity) makes rows that differ in it
/// distinguishable.
fn identity_reads(tier: &Tier, sub: &Subset, side: &Side) -> Vec<String> {
    let ids = side_identity_cols(side);
    let slot = sub.slot;
    let mut read: Vec<String> = Vec::new();
    let mut add = |cols: Vec<String>| {
        for col in cols {
            if ids.contains(&col) && !read.contains(&col) {
                read.push(col);
            }
        }
    };
    for k in &tier.block {
        add(if slot == 0 { &k.a } else { &k.b }.columns_of(slot));
    }
    for r in &tier.requires {
        add(r.expr.columns_of(slot));
    }
    for (e, _) in &sub.aggs {
        let invariant = match &e.kind {
            TExprKind::Agg { func, arg: Some(a) } => match (&a.kind, func) {
                (TExprKind::Column { .. }, AggFunc::Count) => true,
                (TExprKind::Column { name, .. }, AggFunc::CountDistinct) => {
                    ids.len() == 1 && ids[0] == *name
                }
                _ => false,
            },
            _ => false,
        };
        if !invariant {
            add(e.columns_of(0));
        }
    }
    read
}

fn is_null(e: TExpr) -> TExpr {
    t(TExprKind::IsNull {
        expr: Box::new(e),
        negated: false,
    })
}

fn not(e: TExpr) -> TExpr {
    t(TExprKind::Unary {
        op: crate::ast::UnaryOp::Not,
        expr: Box::new(e),
    })
}

/// `s1`, `s2`, ...: the members of a subset in member order (null past its size).
fn slot_col(j: usize) -> String {
    format!("s{j}")
}

pub(super) fn lower_tier(
    rc: &Reconcile,
    tier: &Tier,
    sub: &Subset,
    index: usize,
    n: &Names,
    scratch: &mut Vec<String>,
) -> TierProgram {
    let p = |x: &str| n.t(&format!("t{index}_{x}"));
    // subset side and other side
    let ss = usize::from(sub.slot);
    let os = 1 - ss;
    let k_max = sub.max_items as usize;
    let links = n.t("links");
    let held = [n.t("held_a"), n.t("held_b")];
    let sides = [&rc.a, &rc.b];
    let id_cols = ["a_id", "b_id"];
    let consumed = [rc.consume.a(), rc.consume.b()];
    let group = if os == 0 {
        tier.group_a.as_ref()
    } else {
        tier.group_b.as_ref()
    };
    let mut grouped: [Option<&Group>; 2] = [None, None];
    grouped[os] = group;
    let (ma, mb) = (p("ma"), p("mb"));
    let group_members = if os == 0 { &ma } else { &mb };
    let (units, rows, mem, subs, cand, cm, dec) = (
        p("units"),
        p("rows"),
        p("mem"),
        p("sub"),
        p("cand"),
        p("cm"),
        p("dec"),
    );
    let levels: Vec<String> = (1..=k_max).map(|k| p(&format!("l{k}"))).collect();
    let (dup, cmr, dem, alloc, cmp) = (p("dup"), p("cmr"), p("dem"), p("alloc"), p("cmp"));
    scratch.extend(
        [
            &units, &rows, &mem, &subs, &cand, &cm, &dec, &dup, &cmr, &dem, &alloc, &cmp,
        ]
        .map(|x| x.to_string()),
    );
    // with `consume none` a pair matched once is never matched again
    let exclude_pairs = !rc.consume.a() && !rc.consume.b();
    scratch.extend(levels.iter().cloned());
    let mut prepare = Vec::new();
    let mut ops = Vec::new();

    // ---- units of the other side (rows, or groups as in rollup tiers), rows of the subset side
    let remaining_rows = |side: usize| {
        remaining(
            &n.t(["a", "b"][side]),
            &held[side],
            &links,
            consumed[side],
            id_cols[side],
        )
    };
    match group {
        None => {
            let mut exprs = vec![named(c(ID), UNIT)];
            exprs.extend(
                sides[os]
                    .columns
                    .iter()
                    .map(|col| named(c(&col.name), &col.name)),
            );
            prepare.push(Op::Create {
                table: units.clone(),
                plan: remaining_rows(os).project(exprs),
            });
        }
        Some(g) => {
            // rows whose group key is null belong to no group
            let base = match and_all(g.keys.iter().map(|k| not_null(c(k))).collect()) {
                Some(f) => remaining_rows(os).filter(f),
                None => remaining_rows(os),
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
            prepare.push(Op::Create {
                table: units.clone(),
                plan: groups,
            });
            let on = and_all(g.keys.iter().map(|k| eq(c(k), c1(k))).collect());
            prepare.push(Op::Create {
                table: group_members.clone(),
                plan: base.join(
                    scan(&units),
                    JoinType::Inner,
                    on,
                    vec![named(c1(UNIT), UNIT), named(c(ID), ID)],
                ),
            });
            scratch.push(group_members.clone());
        }
    }
    // Rows interchangeable in this tier: exact duplicates (equal in every column but identity)
    // that are also equal on the identity columns the tier reads. `prior` marks rows an earlier
    // tier matched (possible when this side is not consumed); with `consume none` such a row is
    // a class of its own, since it is no member of the units it already matched.
    let matched_ids = scan(&links)
        .filter(eq(c("kind"), s("match")))
        .project(vec![named(c(id_cols[ss]), "mid")])
        .distinct();
    let mut with_prior = vec![
        named(c(ID), ID),
        named(c(EXACT), EXACT),
        named(not_null(c1("mid")), PRIOR),
    ];
    with_prior.extend(
        sides[ss]
            .columns
            .iter()
            .map(|col| named(c(&col.name), &col.name)),
    );
    let mut equiv_order = Vec::new();
    if exclude_pairs {
        equiv_order.push(asc(case(vec![(c(PRIOR), c(ID))], None)));
    }
    equiv_order.push(asc(c(EXACT)));
    equiv_order.extend(
        identity_reads(tier, sub, sides[ss])
            .iter()
            .map(|x| asc(c(x))),
    );
    let mut exprs = vec![
        named(c(ID), ID),
        named(c(PRIOR), PRIOR),
        named(
            win(WinFunc::DenseRank, None, Vec::new(), equiv_order, None),
            EQUIV,
        ),
    ];
    exprs.extend(
        sides[ss]
            .columns
            .iter()
            .map(|col| named(c(&col.name), &col.name)),
    );
    prepare.push(Op::Create {
        table: rows.clone(),
        plan: remaining_rows(ss)
            .join(
                matched_ids,
                JoinType::Left,
                Some(eq(c(ID), c1("mid"))),
                with_prior,
            )
            .project(exprs),
    });

    // ---- members: (unit, row) pairs passing the blocking keys and member-level requires; the
    // tier's expressions read side A in slot 0 and side B in slot 1
    let mut conds: Vec<TExpr> = tier
        .block
        .iter()
        .map(|k| eq(k.a.clone(), k.b.clone()))
        .collect();
    conds.extend(tier.requires.iter().map(|r| r.expr.clone()));
    let (left, right, unit_e, id_e, ex_e, prior_e) = if ss == 1 {
        (&units, &rows, c(UNIT), c1(ID), c1(EQUIV), c1(PRIOR))
    } else {
        (&rows, &units, c1(UNIT), c(ID), c(EQUIV), c(PRIOR))
    };
    let st = sub.sum_target();
    let mut member_out = vec![
        named(unit_e, "unit"),
        named(id_e, "id"),
        named(ex_e, "ex"),
        named(prior_e, "prior"),
    ];
    if let Some(st) = &st {
        // the summed value (0 when null, as `sum` skips it) and the target; the units are in
        // slot `os` here as in the require
        let x = st
            .arg
            .clone()
            .map_columns(&mut |_, name, ty| TExpr::col(ss as u8, name, ty));
        member_out.push(named(
            case(vec![(not_null(x.clone()), x)], Some(int(0))),
            "v",
        ));
        member_out.push(named(st.target.clone(), "tgt"));
    }
    let mut members = scan(left).join(scan(right), JoinType::Inner, and_all(conds), member_out);
    if exclude_pairs {
        // a pair matched once is never matched again (for a group: any of its rows)
        let matched = scan(&links).filter(eq(c("kind"), s("match")));
        let prior = match group {
            None => matched.project(vec![named(c(id_cols[os]), "o"), named(c(id_cols[ss]), "s")]),
            Some(_) => matched.join(
                scan(group_members),
                JoinType::Inner,
                Some(eq(c(id_cols[os]), c1(ID))),
                vec![named(c1(UNIT), "o"), named(c(id_cols[ss]), "s")],
            ),
        };
        members = members.anti(prior, and(eq(c("unit"), c1("o")), eq(c("id"), c1("s"))));
    }
    // Members are numbered per unit with interchangeable rows next to each other; `er` is a
    // member's position among the unit's members interchangeable with it (rows not matched
    // before first, then identity order).
    let mut mem_plan = members.window(vec![
        named(
            win(
                WinFunc::RowNumber,
                None,
                vec![c("unit")],
                vec![asc(c("ex")), asc(c("prior")), asc(c("id"))],
                None,
            ),
            "idx",
        ),
        named(
            win(
                WinFunc::RowNumber,
                None,
                vec![c("unit"), c("ex")],
                vec![asc(c("prior")), asc(c("id"))],
                None,
            ),
            "er",
        ),
    ]);
    if st.is_some() {
        // `pos{r}` (`neg{r}`), r = 1..max_items - 1: the largest (smallest) sum that at most r
        // members after a member can add, i.e. the sum of the r largest positive (most negative)
        // values after it, 0 when there are none. Such a set's first member j adds its own
        // value's positive part and at most r - 1 members after j, so `pos{r}` is the largest
        // `max(v_j, 0) + pos{r-1}_j` over the members j after it: a running maximum in reverse
        // member order (from each member to the unit's last; `idx` is unique within a unit) taken
        // from the next member. Each r reads the table of r - 1: DuckDB's planning time grows
        // exponentially with the depth of nested window steps.
        let part = |op: BinaryOp| case(vec![(bin(op, c("v"), int(0)), c("v"))], Some(int(0)));
        let reverse = vec![OrderKey {
            expr: c("idx"),
            desc: true,
        }];
        let mut cols = keep(&["unit", "id", "ex", "prior", "v", "tgt", "idx", "er"]);
        for r in 1..k_max {
            if r > 1 {
                let table = p(&format!("mem{}", r - 1));
                scratch.push(table.clone());
                prepare.push(Op::Create {
                    table: table.clone(),
                    plan: mem_plan,
                });
                mem_plan = scan(&table);
            }
            let from = |func: WinFunc, op: BinaryOp, prev: &str| {
                let step = if r == 1 {
                    part(op)
                } else {
                    bin(BinaryOp::Add, part(op), c(&format!("{prev}{}", r - 1)))
                };
                win(func, Some(step), vec![c("unit")], reverse.clone(), None)
            };
            let next = |name: &str| {
                let lead = win(
                    WinFunc::Lead,
                    Some(c(name)),
                    vec![c("unit")],
                    vec![asc(c("idx"))],
                    None,
                );
                native("coalesce", vec![lead, int(0)])
            };
            let (pos_from, neg_from) = (format!("pos_from{r}"), format!("neg_from{r}"));
            cols.extend([format!("pos{r}"), format!("neg{r}")].map(|x| named(c(&x), &x)));
            mem_plan = mem_plan
                .window(vec![
                    named(from(WinFunc::Max, BinaryOp::Gt, "pos"), &pos_from),
                    named(from(WinFunc::Min, BinaryOp::Lt, "neg"), &neg_from),
                ])
                .window(vec![
                    named(next(&pos_from), &format!("pos{r}")),
                    named(next(&neg_from), &format!("neg{r}")),
                ])
                .project(cols.clone());
        }
    }
    prepare.push(Op::Create {
        table: mem.clone(),
        plan: mem_plan,
    });
    let budget = scan(&mem)
        .aggregate(
            vec![named(c("unit"), "unit")],
            vec![named(agg(AggFunc::Count, None), "n")],
        )
        .aggregate(
            vec![named(c("n"), "n")],
            vec![named(agg(AggFunc::Count, None), "units")],
        );

    // ---- subsets of size k extend those of size k - 1 by a member with a higher number. Only
    // representatives are built: a member that is not the first of its interchangeable rows
    // joins only right after the previous one, so of subsets that differ by exchanging
    // interchangeable rows only the one using the first rows in identity order is enumerated.
    // With a sum target a subset of k members with running sum `p` (`ps`) whose last member is
    // `m` is kept only while the r = max_items - k members it may still gain, all after `m`, can
    // bring it to the target: `p` plus the smallest sum they can add (`neg{r}`) is at most the
    // target, and `p` plus the largest (`pos{r}`) at least the target; with no place left `p`
    // must be the target. A subset that reaches the target keeps each of its prefixes, whatever
    // the member order: the members it has after a prefix are no more than that prefix's places.
    let reachable = |p: TExpr, m: fn(&str) -> TExpr, k: usize| {
        let r = k_max - k;
        if r == 0 {
            return eq(p, m("tgt"));
        }
        and(
            bin(
                BinaryOp::Le,
                bin(BinaryOp::Add, p.clone(), m(&format!("neg{r}"))),
                m("tgt"),
            ),
            bin(
                BinaryOp::Le,
                m("tgt"),
                bin(BinaryOp::Add, p, m(&format!("pos{r}"))),
            ),
        )
    };
    let mut level_ops = Vec::new();
    let mut first = vec![
        named(c("unit"), "unit"),
        named(c("idx"), "last"),
        named(c("id"), &slot_col(1)),
    ];
    let mut first_filter = eq(c("er"), int(1));
    if st.is_some() {
        first_filter = and(first_filter, reachable(c("v"), c, 1));
        first.push(named(c("v"), "ps"));
    }
    level_ops.push(Op::Create {
        table: levels[0].clone(),
        plan: scan(&mem).filter(first_filter).project(first),
    });
    for k in 2..=k_max {
        let mut out = vec![named(c("unit"), "unit"), named(c1("idx"), "last")];
        out.extend((1..k).map(|j| named(c(&slot_col(j)), &slot_col(j))));
        out.push(named(c1("id"), &slot_col(k)));
        let mut on = vec![
            eq(c("unit"), c1("unit")),
            bin(BinaryOp::Gt, c1("idx"), c("last")),
            or(
                eq(c1("er"), int(1)),
                eq(c1("idx"), bin(BinaryOp::Add, c("last"), int(1))),
            ),
        ];
        if st.is_some() {
            let ps = bin(BinaryOp::Add, c("ps"), c1("v"));
            on.push(reachable(ps.clone(), c1, k));
            out.push(named(ps, "ps"));
        }
        level_ops.push(Op::Create {
            table: levels[k - 1].clone(),
            plan: scan(&levels[k - 2]).join(scan(&mem), JoinType::Inner, and_all(on), out),
        });
    }
    // (partial subset, member) pairs each level examines: every member for the first level,
    // then the members after each kept subset's last one
    let pruning = st.as_ref().map(|st| {
        let size = p("size");
        scratch.push(size.clone());
        prepare.push(Op::Create {
            table: size.clone(),
            plan: scan(&mem).aggregate(
                vec![named(c("unit"), "unit")],
                vec![named(agg(AggFunc::Count, None), "n")],
            ),
        });
        let mut fanout =
            vec![scan(&mem).aggregate(vec![], vec![named(agg(AggFunc::Count, None), "fanout")])];
        for level in &levels[..k_max - 1] {
            fanout.push(
                scan(level)
                    .join(
                        scan(&size),
                        JoinType::Inner,
                        Some(eq(c("unit"), c1("unit"))),
                        vec![named(bin(BinaryOp::Sub, c1("n"), c("last")), "f")],
                    )
                    .aggregate(
                        vec![],
                        vec![named(agg(AggFunc::Sum, Some(c("f"))), "fanout")],
                    ),
            );
        }
        Pruning {
            require: sub.requires[st.require].text.clone(),
            fanout,
        }
    });
    let all_subsets = LogicalPlan::Union {
        inputs: levels
            .iter()
            .enumerate()
            .map(|(i, level)| {
                let mut out = vec![named(c("unit"), "unit")];
                out.extend((1..=k_max).map(|j| {
                    named(
                        if j <= i + 1 { c(&slot_col(j)) } else { null() },
                        &slot_col(j),
                    )
                }));
                scan(level).project(out)
            })
            .collect(),
    };
    let mut order = vec![asc(c("unit"))];
    order.extend((1..=k_max).map(|j| asc(c(&slot_col(j)))));
    ops.push(Op::Create {
        table: subs.clone(),
        plan: all_subsets.window(vec![named(
            win(WinFunc::RowNumber, None, Vec::new(), order, None),
            "sub",
        )]),
    });

    // ---- per-subset aggregates over the subset's rows
    let mut subset_side = scan(&subs);
    if !sub.aggs.is_empty() {
        let exploded = LogicalPlan::Union {
            inputs: (1..=k_max)
                .map(|j| {
                    scan(&subs)
                        .filter(not_null(c(&slot_col(j))))
                        .project(vec![named(c("sub"), SUBSET), named(c(&slot_col(j)), "id")])
                })
                .collect(),
        };
        let mut out = vec![named(c(SUBSET), SUBSET)];
        out.extend(
            sides[ss]
                .columns
                .iter()
                .map(|col| named(c1(&col.name), &col.name)),
        );
        let per_subset = exploded
            .join(scan(&rows), JoinType::Inner, Some(eq(c("id"), c1(ID))), out)
            .aggregate(vec![named(c(SUBSET), SUBSET)], sub.aggs.clone());
        let mut out = vec![named(c("unit"), "unit"), named(c("sub"), "sub")];
        out.extend((1..=k_max).map(|j| named(c(&slot_col(j)), &slot_col(j))));
        out.extend(sub.aggs.iter().map(|(_, name)| named(c1(name), name)));
        subset_side = subset_side.join(
            per_subset,
            JoinType::Inner,
            Some(eq(c("sub"), c1(SUBSET))),
            out,
        );
    }

    // ---- candidates: subsets passing the subset-level requires, with rank values and evidence
    let side_col = |name: &str| if ss == 1 { c1(name) } else { c(name) };
    let mut conds = vec![if ss == 1 {
        eq(c(UNIT), c1("unit"))
    } else {
        eq(c("unit"), c1(UNIT))
    }];
    conds.extend(sub.requires.iter().map(|r| r.expr.clone()));
    let mut out = vec![
        named(if ss == 1 { c(UNIT) } else { c1(UNIT) }, "unit"),
        named(side_col("sub"), "sub"),
    ];
    out.extend((1..=k_max).map(|j| named(side_col(&slot_col(j)), &slot_col(j))));
    for (k, r) in tier.rank.iter().enumerate() {
        out.push(named(r.expr.clone(), &format!("r_{}", k + 1)));
    }
    let tc_names: Vec<String> = rc
        .tier_columns
        .iter()
        .map(|tc| format!("tc_{}", tc.name))
        .collect();
    let tc_refs: Vec<&str> = tc_names.iter().map(String::as_str).collect();
    out.extend(tier_columns(rc, tier));
    let (l, r) = if ss == 1 {
        (scan(&units), subset_side)
    } else {
        (subset_side, scan(&units))
    };
    let rank_order: Vec<OrderKey> = tier
        .rank
        .iter()
        .enumerate()
        .map(|(k, r)| OrderKey {
            expr: c(&format!("r_{}", k + 1)),
            desc: r.desc,
        })
        .collect();
    ops.push(Op::Create {
        table: cand.clone(),
        plan: l.join(r, JoinType::Inner, and_all(conds), out).window(vec![
            named(
                win(WinFunc::Count, None, vec![c("unit")], Vec::new(), None),
                "cand_n",
            ),
            named(
                win(WinFunc::Rank, None, vec![c("unit")], rank_order, None),
                "candidate_rank",
            ),
            named(rank_values(tier), "rank_values"),
            named(
                win(
                    WinFunc::RowNumber,
                    None,
                    Vec::new(),
                    vec![asc(c("sub"))],
                    None,
                ),
                "sid",
            ),
        ]),
    });

    // candidate members of the representatives: one row per (candidate subset, member), with the
    // member's class and position among the unit's interchangeable members
    let mut member_cols = vec!["unit", "sid", "cand_n", "candidate_rank", "rank_values"];
    member_cols.extend(tc_refs.iter());
    let mut raw_out = keep(&member_cols);
    raw_out.extend(keep1(&["ex", "er"]));
    let unit_and = |more: TExpr| and(eq(c("unit"), c1("unit")), more);
    ops.push(Op::Create {
        table: cmr.clone(),
        plan: LogicalPlan::Union {
            inputs: (1..=k_max)
                .map(|j| {
                    let mut out = keep(&member_cols);
                    out.push(named(c(&slot_col(j)), "id"));
                    scan(&cand).filter(not_null(c(&slot_col(j)))).project(out)
                })
                .collect(),
        }
        .join(
            scan(&mem),
            JoinType::Inner,
            Some(unit_and(eq(c("id"), c1("id")))),
            raw_out,
        ),
    });

    // Copies of interchangeable rows are shared out among the units whose best subsets use
    // them: per class, `d` is the most copies one of a unit's best subsets needs and `s` how many
    // the unit has as members (the same for every unit: copies differ only in columns the tier
    // does not read). Units take consecutive copies in unit order (`off` copies go to earlier
    // units); a class whose units together need more copies than there are is contested (`ok`
    // false), and every unit using it is ambiguous.
    ops.push(Op::Create {
        table: dem.clone(),
        plan: scan(&cmr)
            .filter(eq(c("candidate_rank"), int(1)))
            .aggregate(
                vec![
                    named(c("unit"), "unit"),
                    named(c("sid"), "sid"),
                    named(c("ex"), "ex"),
                ],
                vec![named(agg(AggFunc::Count, None), "n")],
            )
            .aggregate(
                vec![named(c("unit"), "unit"), named(c("ex"), "ex")],
                vec![named(agg(AggFunc::Max, Some(c("n"))), "d")],
            )
            .join(
                scan(&mem).aggregate(
                    vec![named(c("unit"), "unit"), named(c("ex"), "ex")],
                    vec![named(agg(AggFunc::Count, None), "s")],
                ),
                JoinType::Inner,
                Some(unit_and(eq(c("ex"), c1("ex")))),
                vec![
                    named(c("unit"), "unit"),
                    named(c("ex"), "ex"),
                    named(c("d"), "d"),
                    named(c1("s"), "s"),
                ],
            ),
    });
    let earlier = scan(&dem)
        .join(
            scan(&dem),
            JoinType::Inner,
            Some(and(
                eq(c("ex"), c1("ex")),
                bin(BinaryOp::Lt, c1("unit"), c("unit")),
            )),
            vec![
                named(c("unit"), "unit"),
                named(c("ex"), "ex"),
                named(c1("d"), "d"),
            ],
        )
        .aggregate(
            vec![named(c("unit"), "unit"), named(c("ex"), "ex")],
            vec![named(agg(AggFunc::Sum, Some(c("d"))), "off")],
        );
    let classes = scan(&dem).aggregate(
        vec![named(c("ex"), "ex")],
        vec![
            named(agg(AggFunc::Sum, Some(c("d"))), "total"),
            named(agg(AggFunc::Min, Some(c("s"))), "supply"),
        ],
    );
    ops.push(Op::Create {
        table: alloc.clone(),
        plan: scan(&dem)
            .join(
                earlier,
                JoinType::Left,
                Some(unit_and(eq(c("ex"), c1("ex")))),
                vec![
                    named(c("unit"), "unit"),
                    named(c("ex"), "ex"),
                    named(call("coalesce", vec![c1("off"), int(0)]), "off"),
                ],
            )
            .join(
                classes,
                JoinType::Inner,
                Some(eq(c("ex"), c1("ex"))),
                vec![
                    named(c("unit"), "unit"),
                    named(c("ex"), "ex"),
                    named(c("off"), "off"),
                    named(c1("total"), "total"),
                    named(bin(BinaryOp::Le, c1("total"), c1("supply")), "ok"),
                ],
            ),
    });
    // Members of best subsets move to the unit's share of copies (`pos`); in a contested class
    // (`spread`) the unit's decision involves every copy it could have used.
    let best_row = eq(c("candidate_rank"), int(1));
    let mut pos_out = keep(&member_cols);
    pos_out.extend(keep(&["ex", "er"]));
    pos_out.push(named(
        bin(
            BinaryOp::Add,
            c("er"),
            case(
                vec![(and(best_row.clone(), c1("ok")), c1("off"))],
                Some(int(0)),
            ),
        ),
        "pos",
    ));
    pos_out.push(named(
        call(
            "coalesce",
            vec![and(best_row, not(c1("ok"))), TExpr::lit(Lit::Bool(false))],
        ),
        "spread",
    ));
    ops.push(Op::Create {
        table: cmp.clone(),
        plan: scan(&cmr).join(
            scan(&alloc),
            JoinType::Left,
            Some(unit_and(eq(c("ex"), c1("ex")))),
            pos_out,
        ),
    });
    let mut cm_out = keep(&member_cols);
    cm_out.push(named(c1("id"), "id"));
    cm_out.push(named(c("ex"), "ex"));
    let placed = scan(&cmp).filter(not(c("spread"))).join(
        scan(&mem),
        JoinType::Inner,
        Some(unit_and(and(eq(c("ex"), c1("ex")), eq(c("pos"), c1("er"))))),
        cm_out.clone(),
    );
    // a copy an earlier tier matched is involved only if the representative uses it itself
    let spread = scan(&cmp)
        .filter(c("spread"))
        .join(
            scan(&mem),
            JoinType::Inner,
            Some(unit_and(and(
                eq(c("ex"), c1("ex")),
                or(eq(c("er"), c1("er")), not(c1("prior"))),
            ))),
            cm_out,
        )
        .distinct();
    ops.push(Op::Create {
        table: cm.clone(),
        plan: LogicalPlan::Union {
            inputs: vec![placed, spread],
        }
        .window(vec![named(
            win(WinFunc::Count, None, vec![c("id")], Vec::new(), None),
            "cand_s",
        )]),
    });

    // (unit, member) rows -> row pairs: `a_id`, `b_id`, the other side's group and the subset
    // id as groups, and the candidate's columns
    let mut carried = vec![
        "a_unit",
        "b_unit",
        "unit",
        "sid",
        "cand_a",
        "cand_b",
        "candidate_rank",
        "rank_values",
    ];
    carried.extend(tc_refs.iter());
    let edges = |plan: LogicalPlan| -> LogicalPlan {
        let (a_unit, b_unit, cand_a, cand_b) = if ss == 1 {
            ("unit", "id", "cand_n", "cand_s")
        } else {
            ("id", "unit", "cand_s", "cand_n")
        };
        let mut pre = vec![
            named(c(a_unit), "a_unit"),
            named(c(b_unit), "b_unit"),
            named(c(cand_a), "cand_a"),
            named(c(cand_b), "cand_b"),
        ];
        pre.extend(keep(&["unit", "sid", "candidate_rank", "rank_values"]));
        pre.extend(keep(&tc_refs));
        let expanded = expand(plan.project(pre), &carried, grouped, &ma, &mb);
        let (a_group, b_group) = if ss == 1 {
            (c("a_group"), c("sid"))
        } else {
            (c("sid"), c("b_group"))
        };
        let mut out = vec![
            named(c("a_id"), "a_id"),
            named(c("b_id"), "b_id"),
            named(a_group, "a_group"),
            named(b_group, "b_group"),
        ];
        out.extend(keep(&carried[2..]));
        expanded.project(out)
    };

    // `.candidates`: each member edge once, from its best-ranked candidate subset
    let first = scan(&cm)
        .window(vec![named(
            win(
                WinFunc::RowNumber,
                None,
                vec![c("unit"), c("id")],
                vec![asc(c("candidate_rank")), asc(c("sid"))],
                None,
            ),
            "rn",
        )])
        .filter(eq(c("rn"), int(1)));
    let mut cand_out = vec![
        named(c("a_id"), "a_id"),
        named(c("b_id"), "b_id"),
        named(int(index as i64), "tier_index"),
        named(c("a_group"), "a_group"),
        named(c("b_group"), "b_group"),
    ];
    cand_out.extend(keep(&["candidate_rank", "rank_values"]));
    cand_out.extend(keep(&tc_refs));
    ops.push(Op::Insert {
        table: n.t("cands"),
        plan: edges(first).project(cand_out),
    });

    // ---- selection: a unit matches its single best subset unless it uses a contested class
    let best = || scan(&cm).filter(eq(c("candidate_rank"), int(1)));
    let conflicted = scan(&alloc)
        .filter(not(c("ok")))
        .project(vec![named(c("unit"), "unit")])
        .distinct();
    let decision = scan(&cand)
        .filter(eq(c("candidate_rank"), int(1)))
        .aggregate(
            vec![named(c("unit"), "unit")],
            vec![named(agg(AggFunc::Count, None), "nbest")],
        )
        .join(
            conflicted,
            JoinType::Left,
            Some(eq(c("unit"), c1("unit"))),
            vec![
                named(c("unit"), "unit"),
                named(and(eq(c("nbest"), int(1)), is_null(c1("unit"))), "ok"),
            ],
        );
    ops.push(Op::Create {
        table: dec.clone(),
        plan: decision,
    });
    let decided = |ok: bool| {
        let f = if ok { c("ok") } else { not(c("ok")) };
        scan(&dec).filter(f).project(vec![named(c("unit"), "unit")])
    };
    for (ok, kind) in [(true, "match"), (false, "ambiguous")] {
        let chosen = best().semi(decided(ok), eq(c("unit"), c1("unit")));
        let mut o = vec![
            named(c("a_id"), "a_id"),
            named(c("b_id"), "b_id"),
            named(int(index as i64), "tier_index"),
            named(c("a_group"), "a_group"),
            named(c("b_group"), "b_group"),
            named(s(kind), "kind"),
            named(
                if ok {
                    null()
                } else {
                    call("concat", vec![s("s"), to_text(c("unit"))])
                },
                "amb_group",
            ),
        ];
        o.extend(keep(&["cand_a", "cand_b", "candidate_rank", "rank_values"]));
        o.extend(keep(&tc_refs));
        ops.push(Op::Insert {
            table: links.clone(),
            plan: edges(chosen).project(o),
        });
    }
    // Copies of a class that matched units use, left over by every unit's share (positions past
    // the class's total demand) and not matched by an earlier tier, are duplicates of a matched
    // copy.
    let matched_classes = best()
        .semi(decided(true), eq(c("unit"), c1("unit")))
        .aggregate(
            vec![named(c("ex"), "ex")],
            vec![named(agg(AggFunc::Min, Some(c("id"))), "dup_of")],
        )
        .join(
            scan(&alloc).aggregate(
                vec![named(c("ex"), "ex")],
                vec![named(agg(AggFunc::Max, Some(c("total"))), "total")],
            ),
            JoinType::Inner,
            Some(eq(c("ex"), c1("ex"))),
            vec![
                named(c("ex"), "ex"),
                named(c("dup_of"), "dup_of"),
                named(c1("total"), "total"),
            ],
        );
    let leftovers = scan(&mem)
        .filter(not(c("prior")))
        .join(
            matched_classes,
            JoinType::Inner,
            Some(and(
                eq(c("ex"), c1("ex")),
                bin(BinaryOp::Gt, c("er"), c1("total")),
            )),
            vec![
                named(c("unit"), "unit"),
                named(c("id"), "id"),
                named(c1("dup_of"), "dup_of"),
            ],
        )
        .anti(best(), eq(c("id"), c1("id")))
        .window(vec![named(
            win(
                WinFunc::RowNumber,
                None,
                vec![c("id")],
                vec![asc(c("unit"))],
                None,
            ),
            "rn",
        )])
        .filter(eq(c("rn"), int(1)));
    ops.push(Op::Create {
        table: dup.clone(),
        plan: leftovers,
    });
    ops.push(Op::Insert {
        table: n.t(if ss == 0 { "dups_a" } else { "dups_b" }),
        plan: scan(&dup).project(vec![
            named(c("id"), "id"),
            named(c("dup_of"), "dup_of"),
            named(int(index as i64), "tier_index"),
        ]),
    });
    if rc.duplicates == Hold::Hold {
        ops.push(Op::Insert {
            table: held[ss].clone(),
            plan: scan(&dup).project(vec![
                named(c("id"), "id"),
                named(s("duplicate"), "status"),
                named(c("dup_of"), "dup_of"),
                named(int(index as i64), "tier_index"),
            ]),
        });
    }
    if rc.ambiguity == Hold::Hold {
        // the ambiguous units' rows, and every row of their best subsets
        let other_rows = match group {
            None => decided(false).project(vec![named(c("unit"), "id")]),
            Some(_) => decided(false).join(
                scan(group_members),
                JoinType::Inner,
                Some(eq(c("unit"), c1(UNIT))),
                vec![named(c1(ID), "id")],
            ),
        };
        let subset_rows = best()
            .semi(decided(false), eq(c("unit"), c1("unit")))
            .project(vec![named(c("id"), "id")])
            .distinct();
        for (side, plan) in [(os, other_rows), (ss, subset_rows)] {
            ops.push(Op::Insert {
                table: held[side].clone(),
                plan: plan.project(vec![
                    named(c("id"), "id"),
                    named(s("ambiguous"), "status"),
                    named(null(), "dup_of"),
                    named(int(index as i64), "tier_index"),
                ]),
            });
        }
    }

    TierProgram {
        name: tier.name.clone(),
        index,
        description: tier.description(),
        prepare,
        mode: TierMode::Subset(Program {
            max_items: sub.max_items,
            max_subsets: sub.max_subsets,
            span: sub.span,
            budget,
            pruning,
            levels: level_ops,
            ops,
        }),
    }
}

/// Tier evidence and flag columns (`tc_<name>`) of this tier; null for other tiers' columns.
fn tier_columns(rc: &Reconcile, tier: &Tier) -> Vec<(TExpr, String)> {
    rc.tier_columns
        .iter()
        .map(|tc| {
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
            named(e, &format!("tc_{}", tc.name))
        })
        .collect()
}

/// The `rank by` values of a candidate in words (`r_1`, `r_2`, ... columns).
fn rank_values(tier: &Tier) -> TExpr {
    if tier.rank.is_empty() {
        return TExpr::new(
            TExprKind::TryCast {
                expr: Box::new(null()),
                ty: Type::String,
            },
            U,
        );
    }
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
}

#[cfg(test)]
mod tests {
    use super::{largest_unit, subsets};

    #[test]
    fn subset_counts_sum_binomials_up_to_max_items() {
        assert_eq!(subsets(0, 5), 0);
        // fewer members than max_items: every non-empty subset, 2^n - 1
        assert_eq!(subsets(3, 5), 7);
        assert_eq!(subsets(15, 8), 22_818);
        assert_eq!(
            subsets(64, 16),
            (1..=16).map(|k| binomial(64, k)).sum::<u64>()
        );
        assert_eq!(subsets(u64::MAX, 16), u64::MAX);
        assert_eq!(subsets(1 << 40, 16), u64::MAX);
    }

    #[test]
    fn largest_unit_is_the_last_member_count_within_budget() {
        let n = largest_unit(8, 2_000_000);
        assert!(subsets(n, 8) <= 2_000_000);
        assert!(subsets(n + 1, 8) > 2_000_000);
        assert_eq!(largest_unit(1, 10), 10);
        assert_eq!(largest_unit(16, u64::MAX), u64::MAX);
    }

    fn binomial(n: u64, k: u64) -> u64 {
        (1..=k).fold(1u128, |c, i| c * u128::from(n - i + 1) / u128::from(i)) as u64
    }
}
