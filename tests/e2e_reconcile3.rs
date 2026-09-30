//! End-to-end tests of `one_to_one` selection with *stars* (a class whose best candidates are
//! tied across several classes of the other side, each of which has it as its unique best
//! candidate) and of `block by none` acknowledging M305.

#[macro_use]
#[allow(dead_code)]
mod support;

use std::collections::{BTreeMap, BTreeSet};
use support::*;

/// Identity of one row of `csv`: the `cols` values joined with `|`.
fn key(csv: &Csv, row: &[String], cols: &[String]) -> String {
    cols.iter()
        .map(|c| row[csv.col(c)].as_str())
        .collect::<Vec<_>>()
        .join("|")
}

/// Final status of every row of one side (`a` or `b`, identity columns `ids`) of the
/// single-tier reconciliation exported as `out/<rec>_*.csv`, by identity: `matched:<counterpart>`,
/// `ambiguous`, `duplicate:<duplicate_of>` or `unmatched`. Asserts that every row has exactly one
/// final status and that the summary counts the same rows per status.
#[track_caller]
fn statuses(
    fx: &Fixture,
    rec: &str,
    side: &str,
    ids: &[&str],
    other_ids: &[&str],
) -> BTreeMap<String, String> {
    let other = if side == "a" { "b" } else { "a" };
    let prefixed =
        |s: &str, ids: &[&str]| -> Vec<String> { ids.iter().map(|c| format!("{s}_{c}")).collect() };
    let (own, theirs) = (prefixed(side, ids), prefixed(other, other_ids));
    let mut seen: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let matches = fx.csv(&format!("out/{rec}_matches.csv"));
    for r in &matches.rows {
        seen.entry(key(&matches, r, &own))
            .or_default()
            .push(format!("matched:{}", key(&matches, r, &theirs)));
    }
    // in an ambiguity and not matched
    let ambiguous = fx.csv(&format!("out/{rec}_ambiguous.csv"));
    let tied: BTreeSet<String> = ambiguous
        .rows
        .iter()
        .map(|r| key(&ambiguous, r, &own))
        .collect();
    for k in tied {
        seen.entry(k).or_insert_with(|| vec!["ambiguous".into()]);
    }
    let rest = fx.csv(&format!("out/{rec}_unmatched_{side}.csv"));
    let own_plain: Vec<String> = ids.iter().map(|c| c.to_string()).collect();
    for r in &rest.rows {
        let status = match r[rest.col("match_status")].as_str() {
            "duplicate" => format!("duplicate:{}", r[rest.col("duplicate_of")]),
            s => s.to_string(),
        };
        seen.entry(key(&rest, r, &own_plain))
            .or_default()
            .push(status);
    }
    let mut out = BTreeMap::new();
    for (k, v) in seen {
        assert_eq!(v.len(), 1, "{rec}: {side} row {k} has statuses {v:?}");
        out.insert(k, v.into_iter().next().unwrap());
    }

    let summary = fx.csv(&format!("out/{rec}_summary.csv"));
    let col = summary.col(&format!("{side}_rows"));
    let step = summary.col("step");
    let rows = |pred: &dyn Fn(&str) -> bool| -> usize {
        summary
            .rows
            .iter()
            .filter(|r| pred(&r[step]))
            .map(|r| r[col].parse::<usize>().expect("row count"))
            .sum()
    };
    let with = |prefix: &str| out.values().filter(|s| s.starts_with(prefix)).count();
    let is_tier = |s: &str| !matches!(s, "ambiguous" | "duplicate" | "unmatched" | "total");
    assert_eq!(
        rows(&is_tier),
        with("matched:"),
        "{rec}: matched {side} rows"
    );
    for status in ["ambiguous", "duplicate", "unmatched"] {
        assert_eq!(
            rows(&|s| s == status),
            with(status),
            "{rec}: {status} {side} rows"
        );
    }
    assert_eq!(
        rows(&|s| s == "total"),
        out.len(),
        "{rec}: total {side} rows"
    );
    out
}

/// Statuses of the bank side (`id`) and the GL side (`ref`, `entry`) of a reconciliation with
/// the bank as side A.
#[track_caller]
fn bank_gl(fx: &Fixture, rec: &str) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    (
        statuses(fx, rec, "a", &["id"], &["ref", "entry"]),
        statuses(fx, rec, "b", &["ref", "entry"], &["id"]),
    )
}

fn map(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Summary row of the only tier as `pairs,a_rows,b_rows`.
#[track_caller]
fn tier_summary(fx: &Fixture, rec: &str) -> String {
    fx.csv(&format!("out/{rec}_summary.csv"))
        .project(&["step", "pairs", "a_rows", "b_rows"])
        .into_iter()
        .find_map(|r| r.strip_prefix("amount,").map(str::to_string))
        .expect("summary row of tier `amount`")
}

// ---------------------------------------------------------------------------------------------
// stars centred on side A

#[test]
fn star_with_as_many_rows_on_both_sides_matches_every_row() {
    let fx = Fixture::new("reconcile3");
    fx.run_ok("stars.magi");
    // two identical bank lines (one class) vs two receipts of different customers (two classes):
    // any pairing is equivalent, so the rows pair in identity order
    let (bank, gl) = bank_gl(&fx, "eq");
    assert_eq!(
        bank,
        map(&[("a11", "matched:R1|1"), ("a12", "matched:R2|1")])
    );
    assert_eq!(gl, map(&[("R1|1", "matched:a11"), ("R2|1", "matched:a12")]));
    assert_eq!(tier_summary(&fx, "eq"), "2,2,2");
}

#[test]
fn star_with_exact_duplicate_leaves_matches_once_and_reports_the_rest_as_duplicates() {
    let fx = Fixture::new("reconcile3");
    fx.run_ok("stars.magi");
    // one bank line vs one receipt posted twice: the postings differ only in their identity
    // (`entry`), which the policy looks at, so they are two classes of exact duplicates
    let (bank, gl) = bank_gl(&fx, "twice");
    assert_eq!(bank, map(&[("a21", "matched:R3|1")]));
    assert_eq!(
        gl,
        map(&[("R3|1", "matched:a21"), ("R3|2", "duplicate:R3|1")])
    );
    assert_eq!(tier_summary(&fx, "twice"), "1,1,1");
}

#[test]
fn star_centre_with_more_exact_duplicate_rows_matches_as_many_as_there_are_leaf_rows() {
    let fx = Fixture::new("reconcile3");
    fx.run_ok("stars.magi");
    let (bank, gl) = bank_gl(&fx, "three_exact");
    assert_eq!(
        bank,
        map(&[
            ("a51", "matched:R8|1"),
            ("a52", "matched:R9|1"),
            ("a53", "duplicate:a51"),
        ])
    );
    assert_eq!(gl, map(&[("R8|1", "matched:a51"), ("R9|1", "matched:a52")]));
}

#[test]
fn star_whose_larger_side_is_distinguishable_stays_ambiguous() {
    let fx = Fixture::new("reconcile3");
    fx.run_ok("stars.magi");
    // one generic bank line vs receipts of two different customers: choosing is arbitrary
    let (bank, gl) = bank_gl(&fx, "generic");
    assert_eq!(bank, map(&[("a31", "ambiguous")]));
    assert_eq!(gl, map(&[("R4|1", "ambiguous"), ("R5|1", "ambiguous")]));
    // three interchangeable bank lines that are not exact duplicates (their memos differ) vs two
    // receipts: which line stays unmatched would be arbitrary
    let (bank, gl) = bank_gl(&fx, "three");
    assert_eq!(
        bank,
        map(&[
            ("a41", "ambiguous"),
            ("a42", "ambiguous"),
            ("a43", "ambiguous")
        ])
    );
    assert_eq!(gl, map(&[("R6|1", "ambiguous"), ("R7|1", "ambiguous")]));
    assert_eq!(tier_summary(&fx, "three"), "0,0,0");
}

#[test]
fn candidate_whose_best_is_also_tied_is_no_leaf_of_a_star() {
    let fx = Fixture::new("reconcile3");
    fx.run_ok("stars.magi");
    // {a61, a62} tie between R10 and R11, but R10 ties between {a61, a62} and a63: no star, and
    // matching a61-R10, a62-R11 would have taken a63's only candidate
    let (bank, gl) = bank_gl(&fx, "shared_leaf");
    assert_eq!(
        bank,
        map(&[
            ("a61", "ambiguous"),
            ("a62", "ambiguous"),
            ("a63", "ambiguous")
        ])
    );
    assert_eq!(gl, map(&[("R10|1", "ambiguous"), ("R11|1", "ambiguous")]));
}

// ---------------------------------------------------------------------------------------------
// stars centred on side B

#[test]
fn star_centred_on_side_b_is_decided_like_one_centred_on_side_a() {
    let fx = Fixture::new("reconcile3");
    fx.run_ok("stars_b.magi");
    // two bank lines differing only in a column the policy ignores vs two receipts
    let gl = statuses(&fx, "eq_memo", "a", &["ref", "entry"], &["id"]);
    let bank = statuses(&fx, "eq_memo", "b", &["id"], &["ref", "entry"]);
    assert_eq!(
        gl,
        map(&[("R12|1", "matched:a71"), ("R13|1", "matched:a72")])
    );
    assert_eq!(
        bank,
        map(&[("a71", "matched:R12|1"), ("a72", "matched:R13|1")])
    );
    assert_eq!(tier_summary(&fx, "eq_memo"), "2,2,2");
    // the receipt posted twice is now side A: its second posting is a duplicate there
    let gl = statuses(&fx, "twice", "a", &["ref", "entry"], &["id"]);
    let bank = statuses(&fx, "twice", "b", &["id"], &["ref", "entry"]);
    assert_eq!(
        gl,
        map(&[("R3|1", "matched:a21"), ("R3|2", "duplicate:R3|1")])
    );
    assert_eq!(bank, map(&[("a21", "matched:R3|1")]));
}

// ---------------------------------------------------------------------------------------------
// classes: columns read only inside aggregates

#[test]
fn columns_read_only_inside_aggregates_do_not_split_classes() {
    let fx = Fixture::new("reconcile3");
    fx.run_ok("aggregate_only.magi");
    // an earlier rollup tier counts rows by their identity columns; the lines are still
    // interchangeable in the exact tier, so they pair instead of all tying
    let (bank, gl) = bank_gl(&fx, "agg");
    assert_eq!(
        bank,
        map(&[("a81", "matched:R14|1"), ("a82", "matched:R15|1")])
    );
    assert_eq!(
        gl,
        map(&[("R14|1", "matched:a81"), ("R15|1", "matched:a82")])
    );
    assert_eq!(tier_summary(&fx, "agg"), "2,2,2");
}

#[test]
fn with_consume_none_columns_read_only_inside_aggregates_split_classes() {
    let fx = Fixture::new("reconcile3");
    fx.run_ok("consume_none_agg.magi");
    // the A rows differ only in `fee`; pairing them with the B rows in identity order would decide
    // which pair the subset tier may not use again, so r1 and r2 (the fees swapped between the
    // ids) would differ. The rows are told apart instead and the tie is ambiguous in both.
    for rec in ["r1", "r2"] {
        let matches = fx.csv(&format!("out/{rec}_matches.csv"));
        assert_eq!(
            matches.rows,
            Vec::<Vec<String>>::new(),
            "{rec}: {matches:?}"
        );
        let mut tied = fx
            .csv(&format!("out/{rec}_ambiguous.csv"))
            .project(&["tier", "a_id", "b_id"]);
        tied.sort();
        tied.dedup();
        assert_eq!(
            tied,
            lines!["amount,1,1", "amount,1,2", "amount,2,1", "amount,2,2"],
            "{rec}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// duplicates hold|continue for many_to_one / one_to_many

#[test]
fn passed_over_exact_duplicate_is_held_unless_duplicates_continue() {
    let fx = Fixture::new("reconcile3");
    fx.run_ok("dup_hold.magi");
    let pairs = |rec: &str| {
        fx.csv(&format!("out/{rec}_matches.csv"))
            .project(&["tier", "a_id", "b_id"])
    };
    let rest = |rec: &str, side: &str| {
        fx.csv(&format!("out/{rec}_unmatched_{side}.csv"))
            .project(&["id", "match_status", "duplicate_of"])
    };
    // many_to_one: B row 2 is a duplicate of row 1 and withheld from tier `late`
    assert_eq!(pairs("m2o_hold"), lines!["early,1,1"]);
    assert_eq!(rest("m2o_hold", "b"), lines!["2,duplicate,1"]);
    assert_eq!(rest("m2o_hold", "a"), lines!["2,unmatched,"]);
    // one_to_many: the same with the chooser on side B
    assert_eq!(pairs("o2m_hold"), lines!["early,1,1"]);
    assert_eq!(rest("o2m_hold", "a"), lines!["2,duplicate,1"]);
    assert_eq!(rest("o2m_hold", "b"), lines!["2,unmatched,"]);
    // continue: the duplicate stays available and a later tier matches it
    assert_eq!(pairs("m2o_continue"), lines!["early,1,1", "late,2,2"]);
    assert_eq!(rest("m2o_continue", "b"), Vec::<String>::new());
}

#[test]
fn exact_duplicate_another_chooser_picked_is_not_held() {
    let fx = Fixture::new("reconcile3");
    fx.run_ok("dup_hold.magi");
    // A row 1 passes over B row 2, A row 3 picks it: B row 2 is matched, and with `consume a`
    // it stays available to tier `late`
    let mut m = fx
        .csv("out/picked_matches.csv")
        .project(&["tier", "a_id", "b_id"]);
    m.sort();
    assert_eq!(m, lines!["early,1,1", "early,3,2", "late,2,2"]);
    assert_eq!(
        fx.csv("out/picked_unmatched_b.csv").rows,
        Vec::<Vec<String>>::new()
    );
}

// ---------------------------------------------------------------------------------------------
// a row an earlier tier matched is never a surplus copy

/// Matches of `rec` in `rematch.magi` as sorted `tier,a_id,b_id`, after asserting that no row of
/// either side ends unmatched or duplicate.
#[track_caller]
fn rematch(fx: &Fixture, rec: &str) -> Vec<String> {
    for side in ["a", "b"] {
        let rest = fx.csv(&format!("out/{rec}_unmatched_{side}.csv"));
        assert_eq!(
            rest.project(&["id", "match_status"]),
            Vec::<String>::new(),
            "{rec}: side {side}"
        );
    }
    let mut m = fx
        .csv(&format!("out/{rec}_matches.csv"))
        .project(&["tier", "a_id", "b_id"]);
    m.sort();
    m
}

#[test]
fn row_matched_by_an_earlier_tier_is_not_withheld_as_a_duplicate() {
    let fx = Fixture::new("reconcile3");
    fx.run_ok("rematch.magi");
    // B2 is matched by `ref`; in `amount` a1 takes B1 and B2 must stay available (B is not
    // consumed), so `ref_again` matches a3 to it
    let expected = lines!["amount,a1,B1", "ref,a0,B2", "ref_again,a3,B2"];
    for rec in ["m2o_a_hi", "m2o_none_hi", "o2o_a_hi", "o2o_none_hi"] {
        assert_eq!(rematch(&fx, rec), expected, "{rec}");
    }
}

#[test]
fn unmatched_exact_duplicate_is_chosen_before_an_already_matched_one() {
    let fx = Fixture::new("reconcile3");
    fx.run_ok("rematch.magi");
    // B1 is matched by `ref`; in `amount` c1 takes the still unmatched B2 rather than reporting it
    // as a surplus copy of B1, so `ref_again` still matches c3 to B2
    let expected = lines!["amount,c1,B2", "ref,c0,B1", "ref_again,c3,B2"];
    for rec in ["m2o_a_lo", "o2o_a_lo"] {
        assert_eq!(rematch(&fx, rec), expected, "{rec}");
    }
}

// ---------------------------------------------------------------------------------------------
// M305: `block by none` is a deliberate choice

#[test]
fn block_by_none_acknowledges_comparing_every_pair() {
    let fx = Fixture::new("reconcile3");
    // in a tier, and for the whole reconciliation
    fx.check("block_none.magi")
        .assert_code(0)
        .assert_no_diagnostic("M305");
    // no `block` clause anywhere still warns
    fx.check("block_missing.magi")
        .assert_code(0)
        .assert_diagnostic("M305");
    // `block by none` without any `require` still accepts any pair: one warning for that only
    let out = fx.check("any_pair.magi");
    out.assert_code(0);
    let m305 = out.codes().iter().filter(|c| *c == "M305").count();
    assert_eq!(m305, 1, "{}", out.stderr);
}
