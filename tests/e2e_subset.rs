//! Subset tiers (`subset b max_items N`): a unit of one side matches a bounded combination of
//! rows of the other side.

#[macro_use]
#[allow(dead_code)]
mod support;

use std::collections::{BTreeMap, BTreeSet};
use support::*;

fn sorted(mut xs: Vec<String>) -> Vec<String> {
    xs.sort();
    xs
}

/// Summary rows as `step,pairs,a_rows,b_rows`.
#[track_caller]
fn summary(fx: &Fixture, prefix: &str) -> Vec<String> {
    fx.csv(&format!("out/{prefix}_summary.csv"))
        .project(&["step", "pairs", "a_rows", "b_rows"])
}

/// Every input row of `side` has exactly one final status (matched, ambiguous or unmatched), the
/// summary reports the same counts, and a matched row belongs to one match unit: one subset of
/// one counterpart (its group, else its row). For single-tier reconciliations.
#[track_caller]
fn assert_accounting(fx: &Fixture, prefix: &str, side: &str, all: &[&str]) {
    let other = if side == "a" { "b" } else { "a" };
    let matches = fx.csv(&format!("out/{prefix}_matches.csv"));
    let (id, other_id, other_group) = (
        matches.col(&format!("{side}_id")),
        matches.col(&format!("{other}_id")),
        matches.col(&format!("{other}_group")),
    );
    let mut units: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for r in &matches.rows {
        let unit = if r[other_group].is_empty() {
            r[other_id].clone()
        } else {
            format!("g{}", r[other_group])
        };
        units.entry(r[id].clone()).or_default().insert(unit);
    }
    for (row, u) in &units {
        assert_eq!(u.len(), 1, "{prefix}: {side} row {row} is matched to {u:?}");
    }
    let matched: BTreeSet<String> = units.into_keys().collect();
    let in_ambiguity: BTreeSet<String> = fx
        .csv(&format!("out/{prefix}_ambiguous.csv"))
        .column(&format!("{side}_id"))
        .into_iter()
        .collect();
    assert!(
        matched.is_disjoint(&in_ambiguity),
        "{prefix}: {side} rows both matched and ambiguous"
    );
    let unmatched = fx
        .csv(&format!("out/{prefix}_unmatched_{side}.csv"))
        .column("id");
    let mut seen: Vec<String> = matched.iter().chain(&in_ambiguity).cloned().collect();
    seen.extend(unmatched.iter().cloned());
    assert_eq!(
        sorted(seen),
        sorted(all.iter().map(|s| s.to_string()).collect()),
        "{prefix}: each {side} row exactly once"
    );

    let col = if side == "a" { 2 } else { 3 };
    let steps: Vec<Vec<String>> = summary(fx, prefix)
        .into_iter()
        .map(|r| r.split(',').map(str::to_string).collect())
        .collect();
    let rows = |pred: &dyn Fn(&str) -> bool| -> usize {
        steps
            .iter()
            .filter(|r| pred(&r[0]))
            .map(|r| r[col].parse::<usize>().unwrap())
            .sum()
    };
    let tier = |s: &str| !["ambiguous", "duplicate", "unmatched", "total"].contains(&s);
    assert_eq!(rows(&tier), matched.len(), "{prefix}: matched {side} rows");
    assert_eq!(
        rows(&|s| s == "ambiguous"),
        in_ambiguity.len(),
        "{prefix}: ambiguous {side} rows"
    );
    assert_eq!(
        rows(&|s| s == "unmatched" || s == "duplicate"),
        unmatched.len(),
        "{prefix}: unmatched {side} rows"
    );
    assert_eq!(
        rows(&|s| s == "total"),
        all.len(),
        "{prefix}: total {side} rows"
    );
}

// ---------------------------------------------------------------------------------------------
// selection

#[test]
fn payout_matches_its_unique_subset_of_sales_lines() {
    let fx = Fixture::new("subset");
    fx.run_ok("payout.magi");
    // b7 alone has a1's amount but is dated outside the member window; a2 has no candidate
    assert_eq!(
        sorted(
            fx.csv("out/pay_matches.csv")
                .project(&["a_id", "b_id", "a_group", "b_group", "lines"])
        ),
        lines![
            "a1,b1,,1,3",
            "a1,b3,,1,3",
            "a1,b5,,1,3",
            "a3,b8,,2,2",
            "a3,b9,,2,2"
        ]
    );
    assert_eq!(
        summary(&fx, "pay"),
        lines![
            "payout,5,2,5",
            "ambiguous,0,0,0",
            "duplicate,0,0,0",
            "unmatched,0,1,5",
            "total,0,3,10"
        ]
    );
    assert_accounting(&fx, "pay", "a", &["a1", "a2", "a3"]);
    assert_accounting(
        &fx,
        "pay",
        "b",
        &["b1", "b2", "b3", "b4", "b5", "b6", "b7", "b8", "b9", "b10"],
    );
}

#[test]
fn two_equally_good_subsets_make_the_unit_ambiguous() {
    let fx = Fixture::new("subset");
    fx.run_ok("tie.magi");
    assert!(fx.csv("out/tie_matches.csv").rows.is_empty());
    // one decision: a1 against the rows of each tied subset (b1 + b2, and b3)
    assert_eq!(
        sorted(fx.csv("out/tie_ambiguous.csv").project(&[
            "ambiguity_id",
            "a_id",
            "b_id",
            "b_group"
        ])),
        lines!["1,a1,b1,1", "1,a1,b2,1", "1,a1,b3,2"]
    );
    assert_eq!(summary(&fx, "tie")[1], "ambiguous,3,1,3");
    assert_accounting(&fx, "tie", "a", &["a1"]);
    assert_accounting(&fx, "tie", "b", &["b1", "b2", "b3"]);
}

#[test]
fn rank_by_row_count_picks_the_smallest_subset() {
    let fx = Fixture::new("subset");
    fx.run_ok("tie.magi");
    assert_eq!(
        fx.csv("out/smallest_matches.csv").project(&[
            "a_id",
            "b_id",
            "candidate_rank",
            "candidates_a"
        ]),
        lines!["a1,b3,1,2"]
    );
    assert!(fx.csv("out/smallest_ambiguous.csv").rows.is_empty());
    assert_accounting(&fx, "smallest", "a", &["a1"]);
    assert_accounting(&fx, "smallest", "b", &["b1", "b2", "b3"]);
}

#[test]
fn units_whose_unique_subsets_share_a_row_are_both_ambiguous() {
    let fx = Fixture::new("subset");
    fx.run_ok("conflict.magi");
    assert!(fx.csv("out/clash_matches.csv").rows.is_empty());
    assert_eq!(
        sorted(
            fx.csv("out/clash_ambiguous.csv")
                .project(&["ambiguity_id", "a_id", "b_id"])
        ),
        lines!["1,a1,b1", "1,a1,b2", "2,a2,b1", "2,a2,b3"]
    );
    assert_eq!(summary(&fx, "clash")[1], "ambiguous,4,2,3");
    assert_accounting(&fx, "clash", "a", &["a1", "a2"]);
    assert_accounting(&fx, "clash", "b", &["b1", "b2", "b3"]);
}

#[test]
fn a_row_in_a_lower_ranked_subset_of_another_unit_is_no_conflict() {
    let fx = Fixture::new("subset");
    fx.run_ok("conflict.magi");
    assert_eq!(
        sorted(fx.csv("out/apart_matches.csv").project(&["a_id", "b_id"])),
        lines!["a1,b1", "a1,b2", "a3,b4"]
    );
    // every member edge of every candidate subset, once
    assert_eq!(
        sorted(fx.csv("out/apart_candidates.csv").project(&[
            "a_id",
            "b_id",
            "outcome",
            "candidate_rank"
        ])),
        lines![
            "a1,b1,matched,1",
            "a1,b2,matched,1",
            "a3,b2,not selected,2",
            "a3,b3,not selected,2",
            "a3,b4,matched,1"
        ]
    );
    assert_accounting(&fx, "apart", "a", &["a1", "a3"]);
    assert_accounting(&fx, "apart", "b", &["b1", "b2", "b3", "b4"]);
}

#[test]
fn grouped_entry_matches_a_subset_of_bank_lines() {
    let fx = Fixture::new("subset");
    fx.run_ok("deposit.magi");
    // E1 (g1 + g2) = d1 + d2, E2 (g3) = d3: every GL line of the entry pairs with every bank line
    // of the subset; a_group is the subset, b_group the entry
    assert_eq!(
        sorted(
            fx.csv("out/dep_matches.csv")
                .project(&["a_id", "b_id", "a_group", "b_group"])
        ),
        lines![
            "d1,g1,1,1",
            "d1,g2,1,1",
            "d2,g1,1,1",
            "d2,g2,1,1",
            "d3,g3,2,2"
        ]
    );
    assert_eq!(summary(&fx, "dep")[0], "deposits,5,3,3");
    assert_accounting(&fx, "dep", "a", &["d1", "d2", "d3", "d4"]);
    assert_accounting(&fx, "dep", "b", &["g1", "g2", "g3"]);
}

// ---------------------------------------------------------------------------------------------
// exact duplicates

/// `unmatched_b` rows as `id,match_status,duplicate_of`.
#[track_caller]
fn unmatched_b(fx: &Fixture, prefix: &str) -> Vec<String> {
    fx.csv(&format!("out/{prefix}_unmatched_b.csv")).project(&[
        "id",
        "match_status",
        "duplicate_of",
    ])
}

#[test]
fn a_duplicated_row_gives_one_candidate_and_the_copy_is_a_duplicate() {
    let fx = Fixture::new("subset");
    fx.run_ok("dup.magi");
    // a1 = 100 is b1 or its exact duplicate b2: one candidate, matched with the first
    assert_eq!(
        fx.csv("out/one_matches.csv")
            .project(&["a_id", "b_id", "candidates_a"]),
        lines!["a1,b1,1"]
    );
    assert_eq!(
        unmatched_b(&fx, "one"),
        lines!["b2,duplicate,b1", "b3,unmatched,"]
    );
    assert_accounting(&fx, "one", "a", &["a1"]);
    assert_accounting(&fx, "one", "b", &["b1", "b2", "b3"]);
}

#[test]
fn subset_with_a_duplicated_member_matches_its_first_copy() {
    let fx = Fixture::new("subset");
    fx.run_ok("dup.magi");
    // 300 = 100 + 200 where the 100 is posted twice (b1, b2)
    assert_eq!(
        sorted(
            fx.csv("out/two_matches.csv")
                .project(&["a_id", "b_id", "b_group"])
        ),
        lines!["a1,b1,1", "a1,b3,1"]
    );
    assert!(fx.csv("out/two_ambiguous.csv").rows.is_empty());
    assert_eq!(unmatched_b(&fx, "two"), lines!["b2,duplicate,b1"]);
    assert_eq!(
        summary(&fx, "two")[1..3],
        lines!["ambiguous,0,0,0", "duplicate,0,0,1"]
    );
    assert_accounting(&fx, "two", "a", &["a1"]);
    assert_accounting(&fx, "two", "b", &["b1", "b2", "b3"]);
}

#[test]
fn duplicates_continue_leaves_the_copy_to_later_tiers() {
    let fx = Fixture::new("subset");
    fx.run_ok("dup.magi");
    // held: b2 is a duplicate and a2 (100) finds nothing in tier `single`
    assert_eq!(unmatched_b(&fx, "hold"), lines!["b2,duplicate,b1"]);
    assert_eq!(
        fx.csv("out/hold_unmatched_a.csv")
            .project(&["id", "match_status"]),
        lines!["a2,unmatched"]
    );
    assert_accounting(&fx, "hold", "a", &["a1", "a2"]);
    assert_accounting(&fx, "hold", "b", &["b1", "b2", "b3"]);
    // continue: tier `single` matches a2 with b2
    assert_eq!(
        sorted(
            fx.csv("out/cont_matches.csv")
                .project(&["tier", "a_id", "b_id"])
        ),
        lines!["single,a2,b2", "sums,a1,b1", "sums,a1,b3"]
    );
    assert!(fx.csv("out/cont_unmatched_b.csv").rows.is_empty());
    assert_accounting(&fx, "cont", "a", &["a1", "a2"]);
    assert_accounting(&fx, "cont", "b", &["b1", "b2", "b3"]);
}

#[test]
fn copies_of_a_duplicated_row_are_shared_out_between_units() {
    let fx = Fixture::new("subset");
    fx.run_ok("dup.magi");
    // a1 = 300 is b1 + b3 and a2 = 100 is b1 or its copy b2. There are two copies for two
    // units, so the assignment is clean up to exchanging identical rows: a1 (first in unit
    // order) takes b1, a2 takes b2. Tier `single` then has no row left for x2b.
    assert_eq!(
        sorted(
            fx.csv("out/clash_matches.csv")
                .project(&["tier", "a_id", "b_id"])
        ),
        lines!["sums,a1,b1", "sums,a1,b3", "sums,a2,b2"]
    );
    assert!(fx.csv("out/clash_ambiguous.csv").rows.is_empty());
    assert!(fx.csv("out/clash_unmatched_b.csv").rows.is_empty());
    assert_accounting(&fx, "clash", "a", &["a1", "a2", "x2b"]);
    assert_accounting(&fx, "clash", "b", &["b1", "b2", "b3"]);
}

#[test]
fn more_claims_than_copies_hold_every_copy_as_ambiguous() {
    let fx = Fixture::new("subset");
    fx.run_ok("dup.magi");
    // a1, a2 and a3 each need a row of 100 and there are two copies: every unit involved is
    // ambiguous with both copies, so neither copy reaches tier `single` (x4)
    assert!(fx.csv("out/contest_matches.csv").rows.is_empty());
    assert_eq!(
        sorted(
            fx.csv("out/contest_ambiguous.csv")
                .project(&["a_id", "b_id"])
        ),
        lines![
            "a1,b1", "a1,b2", "a1,b3", "a2,b1", "a2,b2", "a3,b1", "a3,b2"
        ]
    );
    assert_eq!(
        fx.csv("out/contest_unmatched_a.csv")
            .project(&["id", "match_status"]),
        lines!["x4,unmatched"]
    );
    assert_accounting(&fx, "contest", "a", &["a1", "a2", "a3", "x4"]);
    assert_accounting(&fx, "contest", "b", &["b1", "b2", "b3"]);
}

#[test]
fn a_copy_matched_by_an_earlier_tier_is_no_duplicate() {
    let fx = Fixture::new("subset");
    fx.run_ok("prior.magi");
    assert_eq!(
        sorted(
            fx.csv("out/prior_matches.csv")
                .project(&["tier", "a_id", "b_id"])
        ),
        lines!["again,a3,B2", "byref,a0,B2", "sums,a1,B1"]
    );
    assert!(fx.csv("out/prior_unmatched_b.csv").rows.is_empty());
}

#[test]
fn distinct_values_of_part_of_a_composite_identity_tell_rows_apart() {
    let fx = Fixture::new("subset");
    fx.run_ok("composite.magi");
    assert_eq!(
        sorted(
            fx.csv("out/comp_matches.csv")
                .project(&["a_id", "b_ref", "b_entry"])
        ),
        lines!["a1,R1,2", "a1,R2,1"]
    );
}

// ---------------------------------------------------------------------------------------------
// budget

#[test]
fn subset_budget_counts_every_subset_before_enumerating() {
    // C(5,1) + C(5,2) = 15 subsets: a budget of 15 runs, 14 stops the run with no output
    let fx = Fixture::new("subset");
    fx.run_ok("budget_ok.magi");
    assert!(fx.exists("out/budget_summary.csv"));

    let fx = Fixture::new("subset");
    let out = fx.run("budget_over.magi", &[]);
    out.assert_code(1).assert_diagnostic("M315");
    assert!(!fx.exists("out/budget_summary.csv"));
}

#[test]
fn pruned_enumeration_runs_where_the_upfront_count_would_stop() {
    // 15 subsets upfront exceed 14, but pruned by the sum target at most 5 + 4 + 3 are examined
    let fx = Fixture::new("subset");
    fx.run_ok("prune_ok.magi");
    assert_eq!(
        sorted(fx.csv("out/prune_matches.csv").project(&["a_id", "b_id"])),
        lines!["a1,b1", "a1,b2"]
    );
}

#[test]
fn pruned_enumeration_stops_when_examined_subsets_exceed_the_budget() {
    // the 5 subsets of one row already exceed 4
    let fx = Fixture::new("subset");
    let out = fx.run("prune_over.magi", &[]);
    out.assert_code(1).assert_diagnostic("M315");
    assert!(!fx.exists("out/prune_matches.csv"));
}

#[test]
fn pruning_by_sum_keeps_every_candidate_with_mixed_signs_and_nulls() {
    let fx = Fixture::new("subset");
    fx.run_ok("signs.magi");
    let pairs = |f: &str| sorted(fx.csv(&format!("out/{f}.csv")).project(&["a_id", "b_id"]));
    let expected = lines!["p1,f1", "p1,s1", "p1,s2", "p2,s3", "p2,s4"];
    assert_eq!(pairs("pruned_matches"), expected);
    assert_eq!(pairs("plain_matches"), expected);
    // the null member can join any subset, so every unit's best subsets tie
    let ambiguous = pairs("pruned_null_ambiguous");
    assert!(!ambiguous.is_empty());
    assert_eq!(ambiguous, pairs("plain_null_ambiguous"));
}

// ---------------------------------------------------------------------------------------------
// check

#[test]
fn max_items_must_be_between_1_and_16() {
    let fx = Fixture::new("subset");
    fx.check("items_ok.magi").assert_code(0);
    let out = fx.check("items_bad.magi");
    out.assert_code(1);
    assert_eq!(out.codes(), ["M313", "M313"], "{}", out.stderr);
}

#[test]
fn subset_side_cannot_be_grouped_and_shape_must_agree() {
    let fx = Fixture::new("subset");
    fx.check("grouped.magi")
        .assert_code(1)
        .assert_diagnostic("M313");
    fx.check("shape.magi")
        .assert_code(1)
        .assert_diagnostic("M313");
}

#[test]
fn subset_side_columns_need_an_aggregate_where_a_value_per_subset_is_needed() {
    let fx = Fixture::new("subset");
    fx.check("bare.magi")
        .assert_code(1)
        .assert_diagnostic("M314");
    fx.check("bare_rank.magi")
        .assert_code(1)
        .assert_diagnostic("M314");
}
