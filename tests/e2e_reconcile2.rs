#[macro_use]
#[allow(dead_code)]
mod support;

use std::collections::BTreeSet;
use support::*;

fn set(xs: Vec<String>) -> BTreeSet<String> {
    xs.into_iter().collect()
}

/// Summary row `step` as `pairs,a_rows,b_rows`.
#[track_caller]
fn summary_row(fx: &Fixture, prefix: &str, step: &str) -> String {
    let summary = fx.csv(&format!("out/{prefix}_summary.csv"));
    let rows: Vec<String> = summary
        .project(&["step", "pairs", "a_rows", "b_rows"])
        .into_iter()
        .filter_map(|r| r.strip_prefix(&format!("{step},")).map(str::to_string))
        .collect();
    assert_eq!(rows.len(), 1, "one summary row `{step}` in {prefix}");
    rows[0].clone()
}

/// Every input row of one side has exactly one final status (matched, ambiguous, duplicate or
/// unmatched), and the summary reports the same counts. For single-tier reconciliations.
#[track_caller]
fn assert_partition(fx: &Fixture, prefix: &str, side: &str, all: &[&str]) {
    let id = format!("{side}_id");
    let matched = set(fx.csv(&format!("out/{prefix}_matches.csv")).column(&id));
    let in_ambiguity = set(fx.csv(&format!("out/{prefix}_ambiguous.csv")).column(&id));
    let ambiguous: BTreeSet<String> = in_ambiguity.difference(&matched).cloned().collect();
    let unmatched = fx.csv(&format!("out/{prefix}_unmatched_{side}.csv"));
    let listed = |status: &str| -> Vec<String> {
        unmatched
            .project(&["match_status", "id"])
            .into_iter()
            .filter_map(|r| r.strip_prefix(&format!("{status},")).map(str::to_string))
            .collect()
    };
    let (dups, rest) = (listed("duplicate"), listed("unmatched"));
    let mut seen: Vec<String> = matched.iter().chain(&ambiguous).cloned().collect();
    seen.extend(dups.iter().cloned());
    seen.extend(rest.iter().cloned());
    seen.sort();
    let mut expected: Vec<String> = all.iter().map(|s| s.to_string()).collect();
    expected.sort();
    assert_eq!(
        seen, expected,
        "{prefix} side {side}: each row exactly once"
    );

    let col = if side == "a" { 2 } else { 3 };
    let summary = fx
        .csv(&format!("out/{prefix}_summary.csv"))
        .project(&["step", "pairs", "a_rows", "b_rows"]);
    let rows = |step: &str| -> usize {
        summary
            .iter()
            .map(|r| r.split(',').collect::<Vec<_>>())
            .filter(|r| match step {
                "tiers" => !["ambiguous", "duplicate", "unmatched", "total"].contains(&r[0]),
                _ => r[0] == step,
            })
            .map(|r| r[col].parse::<usize>().unwrap())
            .sum()
    };
    assert_eq!(
        rows("ambiguous"),
        ambiguous.len(),
        "{prefix}: ambiguous {side}_rows"
    );
    assert_eq!(
        rows("duplicate"),
        dups.len(),
        "{prefix}: duplicate {side}_rows"
    );
    assert_eq!(
        rows("unmatched"),
        rest.len(),
        "{prefix}: unmatched {side}_rows"
    );
    assert_eq!(
        rows("tiers"),
        matched.len(),
        "{prefix}: matched {side}_rows"
    );
    assert_eq!(rows("total"), all.len(), "{prefix}: total {side}_rows");
}

// ---------------------------------------------------------------------------------------------
// user columns named like MAGI's internal ones

#[test]
fn user_columns_named_unit_cls_exact_are_compared_as_data() {
    let fx = Fixture::new("reconcile2");
    fx.run_ok("names.magi");
    // only b2 has the same unit, cls, exact and amount as a1
    assert_eq!(
        fx.csv("out/names_matches.csv")
            .project(&["a_id", "b_id", "a_unit", "b_unit", "b_cls", "b_exact"]),
        lines!["a1,b2,lb,lb,x,e1"]
    );
    assert_eq!(
        fx.csv("out/names_unmatched_b.csv")
            .project(&["id", "unit", "cls", "exact"]),
        lines!["b1,kg,x,e1", "b3,lb,z,e1", "b4,lb,x,e2"]
    );
}

#[test]
fn rollup_group_key_named_unit_groups_by_the_users_column() {
    let fx = Fixture::new("reconcile2");
    fx.run_ok("names.magi");
    assert_eq!(
        fx.csv("out/groups_matches.csv")
            .project(&["a_id", "b_id", "a_unit", "b_unit"]),
        lines!["a3,b1,kg,kg", "a1,b2,lb,lb", "a2,b2,lb,lb"]
    );
}

// ---------------------------------------------------------------------------------------------
// consume none: a pair matched once is never matched again

#[test]
fn consume_none_pairs_interchangeable_rows_with_their_other_partners() {
    let fx = Fixture::new("reconcile2");
    fx.run_ok("reuse.magi");
    // equal rows a1/a2 and b1/b2 pair in identity order in `exact`; `near` must not pick the
    // already matched pairs again, and must not lose the two remaining ones
    let mut m = fx
        .csv("out/same_matches.csv")
        .project(&["tier", "a_id", "b_id"]);
    m.sort();
    assert_eq!(
        m,
        lines!["exact,a1,b1", "exact,a2,b2", "near,a1,b2", "near,a2,b1"]
    );
}

#[test]
fn consume_none_rollup_group_containing_a_matched_pair_is_no_candidate() {
    let fx = Fixture::new("reconcile2");
    fx.run_ok("reuse.magi");
    assert_eq!(
        fx.csv("out/roll_matches.csv")
            .project(&["tier", "a_id", "b_id"]),
        lines!["exact,a1,b1"]
    );
    assert_eq!(
        fx.csv("out/roll_candidates.csv")
            .project(&["tier", "a_id", "b_id"]),
        lines!["exact,a1,b1"]
    );
}

// ---------------------------------------------------------------------------------------------
// final row status is a partition

#[test]
fn many_to_one_tied_rows_are_ambiguous_not_unmatched() {
    let fx = Fixture::new("reconcile2");
    fx.run_ok("status.magi");
    assert_eq!(
        fx.csv("out/mo_ambiguous.csv").project(&["a_id", "b_id"]),
        lines!["1,101", "1,102"]
    );
    assert_eq!(
        fx.csv("out/mo_matches.csv").project(&["a_id", "b_id"]),
        lines!["2,103"]
    );
    assert!(fx.csv("out/mo_unmatched_b.csv").rows.is_empty());
    assert_partition(&fx, "mo", "a", &["1", "2"]);
    assert_partition(&fx, "mo", "b", &["101", "102", "103"]);
    assert_eq!(summary_row(&fx, "mo", "ambiguous"), "2,1,2");
}

#[test]
fn one_to_many_rows_in_an_ambiguity_are_not_listed_unmatched() {
    let fx = Fixture::new("reconcile2");
    fx.run_ok("status.magi");
    assert_eq!(
        fx.csv("out/om_matches.csv").project(&["a_id", "b_id"]),
        lines!["3,103"]
    );
    assert!(fx.csv("out/om_unmatched_a.csv").rows.is_empty());
    assert_partition(&fx, "om", "a", &["1", "2", "3", "4"]);
    assert_partition(&fx, "om", "b", &["101", "102", "103"]);
}

#[test]
fn ambiguity_continue_rows_are_ambiguous_once_and_summary_adds_up() {
    let fx = Fixture::new("reconcile2");
    fx.run_ok("status.magi");
    assert!(fx.csv("out/pc_matches.csv").rows.is_empty());
    assert!(fx.csv("out/pc_unmatched_a.csv").rows.is_empty());
    assert!(fx.csv("out/pc_unmatched_b.csv").rows.is_empty());
    assert_partition(&fx, "pc", "a", &["1", "2"]);
    assert_partition(&fx, "pc", "b", &["101", "102"]);
}

#[test]
fn summary_counts_an_edge_tied_from_both_sides_as_one_pair() {
    let fx = Fixture::new("reconcile2");
    fx.run_ok("status.magi");
    // 1-102 is in A 1's decision and in B 102's decision: two rows, one pair
    let amb = fx.csv("out/p_ambiguous.csv").project(&["a_id", "b_id"]);
    assert_eq!(amb.len(), 4);
    assert_eq!(set(amb).len(), 3);
    assert_eq!(summary_row(&fx, "p", "ambiguous"), "3,2,2");
    assert_partition(&fx, "p", "a", &["1", "2"]);
    assert_partition(&fx, "p", "b", &["101", "102"]);
}

// ---------------------------------------------------------------------------------------------
// rollup groups

#[test]
fn rollup_group_of_one_row_is_a_group_and_null_keys_are_not_grouped() {
    let fx = Fixture::new("reconcile2");
    fx.run_ok("single.magi");
    assert_eq!(
        fx.csv("out/single_matches.csv")
            .project(&["a_group", "a_id", "b_id"]),
        lines!["1,1,101", "2,2,102", "2,3,102"]
    );
    assert_eq!(
        fx.csv("out/single_unmatched_a.csv").project(&["id"]),
        lines!["4"]
    );
    assert_eq!(
        fx.csv("out/single_unmatched_b.csv").project(&["id"]),
        lines!["103"]
    );
}
