//! End-to-end tests: every test runs the real `magi` binary on a small fixture
//! from `tests/fixtures/` copied into a temporary directory and checks exit codes, diagnostics and
//! the exported relations.

#[macro_use]
mod support;

use support::Fixture;

/// Every row of either side ends up in exactly one summary step (holding policies, `consume both`,
/// `one_to_one`), and the exported relations agree: each identity is matched (in one pair, or one
/// rollup group), ambiguous or unmatched/duplicate — never two of these — and the counts equal
/// the summary's.
#[track_caller]
fn assert_accounting(fx: &Fixture) {
    use std::collections::{BTreeMap, BTreeSet};
    let summary = fx.csv("out/summary.csv");
    let steps = summary.column("step");
    let count = |side: &str, pred: &dyn Fn(&str) -> bool| -> i64 {
        steps
            .iter()
            .zip(summary.column(side))
            .filter(|(s, _)| pred(s))
            .map(|(_, v)| v.parse::<i64>().expect("row count"))
            .sum()
    };
    let matches = fx.csv("out/matches.csv");
    let ambiguous = fx
        .exists("out/ambiguous.csv")
        .then(|| fx.csv("out/ambiguous.csv"));
    for (side, slot) in [("a", 4), ("b", 5)] {
        let rows = format!("{side}_rows");
        let total = count(&rows, &|s| s == "total");
        let accounted = count(&rows, &|s| s != "total");
        assert_eq!(
            accounted, total,
            "{side}: summary steps != total in {summary:?}"
        );

        // matched: one identity belongs to one match unit (tier + counterpart pair or group)
        let mut units: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let other = if slot == 4 { 5 } else { 4 };
        let other_group = matches.col(if side == "a" { "b_group" } else { "a_group" });
        for r in &matches.rows {
            let unit = if r[other_group].is_empty() {
                r[other].clone()
            } else {
                format!("g{}", r[other_group])
            };
            units
                .entry(r[slot].clone())
                .or_default()
                .insert(format!("{}:{unit}", r[matches.col("tier_index")]));
        }
        for (id, u) in &units {
            assert_eq!(
                u.len(),
                1,
                "{side} row {id} is matched more than once: {u:?}"
            );
        }
        let matched: BTreeSet<String> = units.into_keys().collect();
        let held: BTreeSet<String> = ambiguous
            .as_ref()
            .map(|a| a.rows.iter().map(|r| r[slot + 1].clone()).collect())
            .unwrap_or_default();
        let is_tier = |s: &str| !matches!(s, "ambiguous" | "duplicate" | "unmatched" | "total");
        assert_eq!(
            matched.len() as i64,
            count(&rows, &is_tier),
            "{side}: matched rows vs summary"
        );
        assert_eq!(
            held.len() as i64,
            count(&rows, &|s| s == "ambiguous"),
            "{side}: ambiguous rows vs summary"
        );
        let both: Vec<_> = matched.intersection(&held).collect();
        assert!(
            both.is_empty(),
            "{side} rows {both:?} are both matched and ambiguous"
        );
        let path = format!("out/unmatched_{side}.csv");
        if !fx.exists(&path) {
            continue;
        }
        let rest = fx.csv(&path);
        let rest_ids: BTreeSet<String> = rest.rows.iter().map(|r| r[2].clone()).collect();
        assert_eq!(
            rest_ids.len(),
            rest.rows.len(),
            "{side}: a row is listed twice as unmatched"
        );
        for (x, what) in [(&matched, "matched"), (&held, "ambiguous")] {
            let both: Vec<_> = x.intersection(&rest_ids).collect();
            assert!(
                both.is_empty(),
                "{side} rows {both:?} are both {what} and unmatched"
            );
        }
        assert_eq!(
            rest_ids.len() as i64,
            count(&rows, &|s| s == "duplicate" || s == "unmatched"),
            "{side}: unmatched rows vs summary"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// null handling

#[test]
fn null_comparisons_filter_and_join_like_sql() {
    let fx = Fixture::new("nulls");
    fx.run_ok("nulls.magi");
    // `amount > 5` is not true for a missing amount
    assert_eq!(fx.read("out/big.csv"), "id,amount\na1,10\na2,20\na4,7\n");
    assert_eq!(fx.read("out/no_amount.csv"), "id\na3\n");
    // `org != "ORG-1"` does not keep the null org either
    assert_eq!(fx.read("out/not_org1.csv"), "id,org\na3,ORG-2\na4,ORG-3\n");
    // null keys never join, even to another null
    assert_eq!(fx.read("out/inner_joined.csv"), "a_id,b_id\na1,b1\na3,b3\n");
    assert_eq!(
        fx.read("out/left_joined.csv"),
        "a_id,b_id\na1,b1\na2,\na3,b3\na4,\n"
    );
    assert_eq!(
        fx.read("out/filled.csv"),
        "id,amount_or_zero\na1,10\na2,20\na3,0\na4,7\n"
    );
}

#[test]
fn not_null_checks_report_the_rows_with_nulls() {
    let fx = Fixture::new("nulls");
    let out = fx.run_ok("nulls.magi");
    assert_eq!(
        out.codes().iter().filter(|c| *c == "M404").count(),
        2,
        "{}",
        out.stderr
    );
    // each failure names its row in the source file (data rows counted from 1)
    assert_eq!(
        fx.read("out/failures.csv"),
        "check,severity,source_row,id,org,amount\namount not null,warn,3,a3,ORG-2,\norg present,warn,2,a2,,20\n"
    );
    assert_eq!(
        fx.read("out/checks.csv"),
        "check,severity,status,failing_rows,total_rows,measured\namount not null,warn,fail,1,4,\norg present,warn,fail,1,4,\n"
    );
}

#[test]
fn reconcile_null_blocking_keys_never_match() {
    let fx = Fixture::new("nulls");
    fx.run_ok("nulls.magi");
    // a2 and b2 both have a null org and the same amount; the unconditional second tier still
    // does not pair them
    assert_eq!(
        fx.csv("out/matches.csv").project(&["tier", "a_id", "b_id"]),
        lines!["exact,a1,b1", "any_amount,a3,b3"]
    );
    let candidates = fx.csv("out/candidates.csv").project(&["a_id", "b_id"]);
    assert!(
        !candidates
            .iter()
            .any(|c| c.contains("a2") || c.contains("b2")),
        "{candidates:?}"
    );
    assert_eq!(
        fx.csv("out/unmatched_a.csv")
            .project(&["match_status", "id"]),
        lines!["unmatched,a2", "unmatched,a4"]
    );
    assert_eq!(
        fx.csv("out/unmatched_b.csv")
            .project(&["match_status", "id"]),
        lines!["unmatched,b2"]
    );
}

// ---------------------------------------------------------------------------------------------
// date boundaries

#[test]
fn parse_date_tries_each_format_and_invalid_dates_become_null() {
    let fx = Fixture::new("dates");
    fx.run_ok("dates.magi");
    assert_eq!(
        fx.read("out/parsed.csv"),
        "id,raw,day\n\
         1,2024-02-29,2024-02-29\n\
         2,03/15/2024,2024-03-15\n\
         3,02/30/2024,\n\
         4,13/01/2024,\n\
         5,,\n\
         6,2023-02-29,\n"
    );
}

#[test]
fn declared_date_column_rejects_invalid_calendar_dates() {
    let fx = Fixture::new("dates");
    let out = fx.run_ok("dates.magi");
    out.assert_diagnostic("M208");
    assert!(
        out.stderr_flat()
            .contains("3 value(s) in column `raw` are not valid date and became null"),
        "{}",
        out.stderr
    );
    assert_eq!(
        fx.csv("out/declared.csv").project(&["id", "raw"]),
        lines!["1,2024-02-29", "2,2024-03-15", "3,", "4,", "5,", "6,"]
    );
    // the blank value is simply missing, not a reject
    assert_eq!(
        fx.read("out/rejects.csv"),
        "row,column,expected,value\n3,raw,date,02/30/2024\n4,raw,date,13/01/2024\n6,raw,date,2023-02-29\n"
    );
}

#[test]
fn reject_warning_names_the_first_source_rows() {
    let fx = Fixture::new("dates");
    let out = fx.run_ok("dates.magi");
    assert!(
        out.stderr_flat().contains("first at source row(s) 3, 4, 6"),
        "{}",
        out.stderr
    );
}

#[test]
fn day_arithmetic_across_month_year_and_leap_day() {
    let fx = Fixture::new("dates");
    fx.run_ok("dates.magi");
    assert_eq!(
        fx.read("out/spans.csv"),
        "id,start,finish,gap,signed,next_day,before_leap\n\
         1,2024-01-31,2024-03-01,30,30,2024-02-01,true\n\
         2,2023-12-31,2024-01-01,1,1,2024-01-01,true\n\
         3,2023-02-28,2023-03-01,1,1,2023-03-01,true\n\
         4,2024-02-28,2024-03-01,2,2,2024-02-29,true\n\
         5,2025-12-31,2024-12-31,365,-365,2026-01-01,false\n\
         6,2024-03-01,2024-02-28,2,-2,2024-03-02,false\n"
    );
}

#[test]
fn invalid_date_literal_is_a_compile_error() {
    let fx = Fixture::new("errors");
    let out = fx.check("invalid_date.magi");
    out.assert_code(1).assert_diagnostic("M108");
    assert!(
        out.stderr.contains("`2024-02-30` is not a valid date"),
        "{}",
        out.stderr
    );
    // `run` refuses too, before touching any data
    let run = fx.run("invalid_date.magi", &[]);
    run.assert_code(1).assert_diagnostic("M108");
    assert!(!fx.exists("out"));
}

// ---------------------------------------------------------------------------------------------
// duplicate rows

#[test]
fn exact_duplicate_leftover_is_reported_as_duplicate_of_the_first() {
    let fx = Fixture::new("duplicates");
    fx.run_ok("duplicates.magi");
    assert_eq!(
        fx.csv("out/matches.csv").project(&["tier", "a_id", "b_id"]),
        lines!["exact,a1,b1", "exact,a3,b2", "exact,a4,b4"]
    );
    assert_eq!(
        fx.read("out/unmatched_a.csv"),
        "match_status,duplicate_of,id,k,amt,note\nduplicate,a1,a2,X,10,same\n"
    );
    assert_eq!(
        fx.read("out/unmatched_b.csv"),
        "match_status,duplicate_of,id,k,amt\nduplicate,b4,b5,Z,3\nunmatched,,b3,X,11\n"
    );
    let summary = fx.csv("out/summary.csv");
    assert_eq!(
        summary.project(&["step", "a_rows", "b_rows"])[3],
        "duplicate,1,1"
    );
    assert_accounting(&fx);
}

#[test]
fn duplicates_continue_leaves_the_leftover_to_later_tiers() {
    let fx = Fixture::new("duplicates");
    fx.run_ok("duplicates_continue.magi");
    // a2 is not held: tier `near` pairs it with b3
    assert_eq!(
        fx.csv("out/matches.csv").project(&["tier", "a_id", "b_id"]),
        lines!["exact,a1,b1", "exact,a3,b2", "exact,a4,b4", "near,a2,b3"]
    );
    assert_eq!(fx.csv("out/unmatched_a.csv").rows.len(), 0);
}

#[test]
fn duplicates_continue_still_reports_an_unused_duplicate() {
    let fx = Fixture::new("duplicates");
    fx.run_ok("duplicates_continue.magi");
    assert_eq!(
        fx.csv("out/unmatched_b.csv")
            .project(&["match_status", "duplicate_of", "id"]),
        lines!["duplicate,b4,b5"]
    );
}

// ---------------------------------------------------------------------------------------------
// ambiguous matching

#[test]
fn one_a_with_two_distinguishable_equal_b_is_ambiguous_and_held() {
    let fx = Fixture::new("ambiguous");
    fx.run_ok("ambiguous.magi");
    let ambiguous = fx.csv("out/ambiguous.csv");
    assert_eq!(
        ambiguous.project(&["tier", "ambiguity_id", "a_id", "b_id"]),
        lines!["exact,1,a1,b1", "exact,1,a1,b2"]
    );
    let matched = fx.csv("out/matches.csv").project(&["a_id", "b_id"]);
    assert!(
        !matched
            .iter()
            .any(|m| m.contains("a1") || m.contains("b1") || m.contains("b2")),
        "{matched:?}"
    );
    // held: the later tier `by_memo` (a1.note == b1.memo) does not get to pair them
    assert!(!fx.read("out/candidates.csv").contains("by_memo"));
    // held rows are not reported as unmatched
    assert_eq!(fx.csv("out/unmatched_a.csv").rows.len(), 0);
    assert_eq!(fx.csv("out/unmatched_b.csv").project(&["id"]), lines!["b6"]);
    assert_accounting(&fx);
}

#[test]
fn policy_equal_rows_pair_in_identity_order() {
    let fx = Fixture::new("ambiguous");
    fx.run_ok("ambiguous.magi");
    let matches = fx.csv("out/matches.csv");
    let y: Vec<String> = matches
        .project(&["a_id", "b_id", "a_k"])
        .into_iter()
        .filter(|r| r.ends_with(",Y"))
        .collect();
    assert_eq!(y, lines!["a3,b3,Y", "a4,b4,Y"]);
    assert_eq!(
        fx.csv("out/unranked_matches.csv")
            .project(&["a_id", "b_id"]),
        lines!["a3,b3", "a4,b4"]
    );
}

#[test]
fn rank_by_resolves_a_tie() {
    let fx = Fixture::new("ambiguous");
    fx.run_ok("ambiguous.magi");
    // ranked by description similarity, a5 ("red box") pairs with b5 ("red box")
    assert!(
        fx.csv("out/matches.csv")
            .project(&["a_id", "b_id"])
            .contains(&"a5,b5".to_string())
    );
    assert_eq!(
        fx.csv("out/unmatched_b.csv")
            .project(&["match_status", "id"]),
        lines!["unmatched,b6"]
    );
    // without ranking b5 and b6 are equally good
    assert_eq!(
        fx.csv("out/unranked_ambiguous.csv")
            .project(&["ambiguity_id", "a_id", "b_id"]),
        lines!["1,a1,b1", "1,a1,b2", "2,a5,b5", "2,a5,b6"]
    );
}

#[test]
fn ambiguity_continue_lets_a_later_tier_match() {
    let fx = Fixture::new("ambiguous");
    fx.run_ok("ambiguous_continue.magi");
    // still reported ...
    assert_eq!(
        fx.csv("out/ambiguous.csv")
            .project(&["tier", "a_id", "b_id"]),
        lines!["exact,a1,b1", "exact,a1,b2"]
    );
    // ... but available: the memo tier resolves it
    assert_eq!(
        fx.csv("out/matches.csv").project(&["tier", "a_id", "b_id"]),
        lines!["exact,a3,b3", "exact,a4,b4", "exact,a5,b5", "by_memo,a1,b1"]
    );
    // b2 was in the exact-tier ambiguity and is not matched: its final status is ambiguous,
    // so it is not listed as unmatched
    assert_eq!(fx.csv("out/unmatched_b.csv").project(&["id"]), lines!["b6"]);
}

// ---------------------------------------------------------------------------------------------
// one-to-one consumption

#[test]
fn row_matched_in_an_earlier_tier_is_not_reused() {
    let fx = Fixture::new("consumption");
    fx.run_ok("consumption.magi");
    // b1 went to a1 in tier 1, so a4 (nearest to b1) must take b2 in tier 2
    assert_eq!(
        fx.csv("out/both_matches.csv")
            .project(&["tier", "a_id", "b_id"]),
        lines!["exact,a1,b1", "near,a2,b4", "near,a3,b3", "near,a4,b2"]
    );
    assert_eq!(fx.csv("out/both_unmatched_a.csv").rows.len(), 0);
    assert_eq!(fx.csv("out/both_unmatched_b.csv").rows.len(), 0);
}

#[test]
fn mutual_best_pairs_first_then_the_rest_in_later_rounds() {
    let fx = Fixture::new("consumption");
    fx.run_ok("consumption.magi");
    // a2's best is b3 (|100-102| = 2), but b3 prefers a3 (|103-102| = 1): a3-b3, then a2-b4
    let y: Vec<String> = fx
        .csv("out/both_matches.csv")
        .project(&["a_id", "b_id", "a_k"])
        .into_iter()
        .filter(|r| r.ends_with(",Y"))
        .collect();
    assert_eq!(y, lines!["a2,b4,Y", "a3,b3,Y"]);
}

#[test]
fn consume_policy_decides_which_side_later_tiers_may_reuse() {
    let fx = Fixture::new("consumption");
    fx.run_ok("consumption.magi");
    let x = |part: &str| -> Vec<String> {
        fx.csv(&format!("out/{part}_matches.csv"))
            .project(&["tier", "a_id", "b_id", "a_k"])
            .into_iter()
            .filter(|r| r.ends_with(",X"))
            .collect()
    };
    // consume none: tier 2 sees every row again, but the tier-1 pair a1-b1 is not matched twice,
    // so a4 (nearest to b1) takes b1 and a1 takes b2
    assert_eq!(
        x("none"),
        lines!["exact,a1,b1,X", "near,a1,b2,X", "near,a4,b1,X"]
    );
    // consume a: a1 is used up, b1 is not: a4 takes b1, b2 stays unmatched
    assert_eq!(x("only_a"), lines!["exact,a1,b1,X", "near,a4,b1,X"]);
    assert_eq!(
        fx.csv("out/only_a_unmatched_b.csv").project(&["id"]),
        lines!["b2"]
    );
    // consume b: b1 is used up, a1 is not: a1 (closer than a4) takes b2, a4 stays unmatched
    assert_eq!(x("only_b"), lines!["exact,a1,b1,X", "near,a1,b2,X"]);
    assert_eq!(
        fx.csv("out/only_b_unmatched_a.csv").project(&["id"]),
        lines!["a4"]
    );
}

#[test]
fn many_to_one_lets_several_a_rows_share_one_b_row() {
    let fx = Fixture::new("consumption");
    fx.run_ok("consumption.magi");
    assert_eq!(
        fx.csv("out/many_matches.csv").project(&["a_id", "b_id"]),
        lines!["a1,b1", "a5,b5", "a6,b5", "a7,b6"]
    );
    assert_eq!(
        fx.csv("out/many_unmatched_b.csv").project(&["id"]),
        lines!["b2", "b3", "b4"]
    );
}

// ---------------------------------------------------------------------------------------------
// rollups

#[test]
fn many_a_to_one_b_matches_the_group_sum() {
    let fx = Fixture::new("rollups");
    fx.run_ok("rollups.magi");
    let matches = fx.csv("out/matches.csv");
    let rollup: Vec<String> = matches
        .project(&["tier", "a_group", "a_id", "b_id", "a_total"])
        .into_iter()
        .filter(|r| r.starts_with("rollup,"))
        .collect();
    assert_eq!(
        rollup,
        lines![
            "rollup,1,a1,b1,6.75",
            "rollup,1,a2,b1,6.75",
            "rollup,1,a3,b1,6.75",
            "rollup,3,a4,b2,5.00"
        ]
    );
}

#[test]
fn many_a_to_many_b_matches_group_sums_on_both_sides() {
    let fx = Fixture::new("rollups");
    fx.run_ok("rollups.magi");
    let many: Vec<String> = fx
        .csv("out/matches.csv")
        .project(&["tier", "a_id", "b_id", "a_total", "b_total"])
        .into_iter()
        .filter(|r| r.starts_with("rollup_many,"))
        .collect();
    assert_eq!(
        many,
        lines![
            "rollup_many,a8,b4,5.00,5.00",
            "rollup_many,a8,b5,5.00,5.00",
            "rollup_many,a9,b4,5.00,5.00",
            "rollup_many,a9,b5,5.00,5.00"
        ]
    );
}

#[test]
fn rollups_never_search_subsets() {
    let fx = Fixture::new("rollups");
    fx.run_ok("rollups.magi");
    // a4 = b2 is a group of one row and matches; a5 + a6 = b3 but the group is a5..a7;
    // a10 has no counterpart
    assert_eq!(
        fx.csv("out/unmatched_a.csv").project(&["id"]),
        lines!["a10", "a5", "a6", "a7"]
    );
    assert_eq!(fx.csv("out/unmatched_b.csv").project(&["id"]), lines!["b3"]);
    assert_accounting(&fx);
}

// ---------------------------------------------------------------------------------------------
// category mappings

#[test]
fn normalize_with_inline_arms_keeps_unmapped_values() {
    let fx = Fixture::new("mappings");
    fx.run_ok("mappings.magi");
    assert_eq!(
        fx.read("out/inline.csv"),
        "id,category\n1,Firearms\n2,Firearms\n3,Firearms\n4,Narcotic\n5,Vehicle\n6,\n"
    );
}

#[test]
fn named_mapping_via_normalize_with_and_map() {
    let fx = Fixture::new("mappings");
    fx.run_ok("mappings.magi");
    // `codes` has no `otherwise`: unmapped (x) and missing codes become null
    assert_eq!(
        fx.read("out/named.csv"),
        "id,category,colour\n1,Firearms,green\n2,Firearms,red\n3,Firearms,\n4,Drugs,green\n5,Vehicle,\n6,,red\n"
    );
    assert_eq!(
        fx.read("out/per_category.csv"),
        "category,n\nDrugs,1\nFirearms,3\nVehicle,1\n,1\n"
    );
}

#[test]
fn value_mapped_twice_is_an_error() {
    let fx = Fixture::new("mappings");
    let out = fx.check("duplicate_pattern.magi");
    out.assert_code(1).assert_diagnostic("M109");
    assert!(
        out.stderr.contains("value is mapped twice"),
        "{}",
        out.stderr
    );
}

// ---------------------------------------------------------------------------------------------
// Excel inference failures

#[test]
fn check_warns_about_mixed_type_columns_and_misplaced_headers() {
    let fx = Fixture::new("excel");
    let out = fx.check("inference.magi");
    out.assert_code(0).assert_diagnostic("M201");
    let err = out.stderr_flat();
    assert!(
        err.contains(
            "column amount: 2 numeric cells and 2 text cells (first: B3, B5); inferred string"
        ),
        "{}",
        out.stderr
    );
    assert!(
        err.contains(
            "header row 1 has 1 non-empty cell but row 3 has 3; the table probably starts at row 3"
        ),
        "{}",
        out.stderr
    );
    // inferred as text, every value survives unchanged
    fx.run_ok("inference.magi");
    assert_eq!(
        fx.csv("out/mixed.csv").column("amount"),
        lines!["10.5", "n/a", "7", "12,5"]
    );
    // the title row became the header; the table itself was lost
    assert_eq!(
        fx.read("out/report.csv"),
        "Quarterly shipments report,column_2,column_3\n"
    );
}

#[test]
fn declared_excel_schema_turns_bad_cells_into_rejects() {
    let fx = Fixture::new("excel");
    let out = fx.run_ok("declared.magi");
    out.assert_diagnostic("M208");
    assert_eq!(
        fx.read("out/mixed.csv"),
        "id,amount,note\n1,10.50,ok\n2,,text in a number column\n3,7.00,ok\n4,,decimal comma\n"
    );
    // rows are worksheet rows
    assert_eq!(
        fx.read("out/rejects.csv"),
        "row,column,expected,value\n3,amount,\"decimal(10,2)\",n/a\n5,amount,\"decimal(10,2)\",\"12,5\"\n"
    );
    assert_eq!(
        fx.read("out/report.csv"),
        "id,org,amount\n1,ORG-1,10\n2,ORG-2,20\n3,ORG-3,30\n"
    );
}

// ---------------------------------------------------------------------------------------------
// decimal precision

#[test]
fn decimal_sums_are_exact() {
    let fx = Fixture::new("decimals");
    fx.run_ok("decimals.magi");
    assert_eq!(
        fx.read("out/totals.csv"),
        "total,float_total,total_is_03,float_total_is_03\n0.30,0.30000000000000004,true,false\n"
    );
}

#[test]
fn grams_times_0_001_equals_the_decimal_kilograms() {
    let fx = Fixture::new("decimals");
    fx.run_ok("decimals.magi");
    // 9 g * 0.001 == 0.009 kg exactly; the same in binary floats is not equal
    assert_eq!(
        fx.csv("out/converted.csv")
            .project(&["id", "grams_as_kg", "same_kg", "float_same_kg"]),
        lines![
            "1,1.100,true,true",
            "2,12.500,true,true",
            "3,0.009,true,false"
        ]
    );
}

#[test]
fn too_many_decimal_places_are_rounded_with_a_warning() {
    let fx = Fixture::new("decimals");
    let out = fx.run_ok("decimals.magi");
    out.assert_diagnostic("M209");
    assert!(
        out.stderr_flat().contains(
            "1 value(s) in column `amount` have more than 2 decimal places and were rounded"
        ),
        "{}",
        out.stderr
    );
    // 1.005 read from text (not through a binary float) rounds half away from zero
    assert_eq!(
        fx.csv("out/converted.csv").column("amount"),
        lines!["0.10", "0.20", "1.01"]
    );
}

// ---------------------------------------------------------------------------------------------
// SQL source extraction

#[test]
fn duckdb_file_source_reads_tables_and_queries() {
    let fx = Fixture::new("sql_source");
    fx.run_ok("make_db.magi");
    assert!(fx.exists("warehouse.duckdb"));
    let check = fx.check("sql_source.magi");
    check.assert_code(0);
    fx.run_ok("sql_source.magi");
    assert_eq!(
        fx.read("out/orders.csv"),
        "id,customer,amount,day\n1,acme,10.50,2024-01-05\n2,globex,20.00,2024-01-06\n3,acme,5.25,2024-02-01\n"
    );
    assert_eq!(fx.read("out/acme.csv"), "id,amount\n1,10.50\n3,5.25\n");
    assert_eq!(
        fx.read("out/per_customer.csv"),
        "customer,total\nacme,15.75\nglobex,20.00\n"
    );
    assert_eq!(
        fx.read("out/january.csv"),
        "id,customer,amount\n1,acme,10.50\n2,globex,20.00\n"
    );
}

#[test]
#[ignore = "needs MAGI_TEST_SQLITE_ODBC_DRIVER"]
fn odbc_sqlite_source_extracts_typed_rows() {
    let driver = std::env::var("MAGI_TEST_SQLITE_ODBC_DRIVER").expect(
        "MAGI_TEST_SQLITE_ODBC_DRIVER must name the SQLite ODBC driver (scripts/build-sqlite-odbc.sh)",
    );
    let fx = Fixture::new("odbc");
    let db = fx.file("shop.db");
    let script = "import sqlite3, sys\n\
        con = sqlite3.connect(sys.argv[1])\n\
        con.executescript('''\n\
        CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT, amount NUMERIC, sold_on TEXT);\n\
        INSERT INTO items VALUES (1, 'bolt', 10.5, '2024-01-05'), (2, 'nut', 2.25, '2024-02-29'),\n\
            (3, NULL, NULL, 'not a date'), (4, 'washer', 12, '2024-03-01');\n\
        ''')\n\
        con.commit()\n";
    let status = std::process::Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(&db)
        .status()
        .expect("python3 is needed to create the SQLite database");
    assert!(status.success());
    let conn = format!("Driver={driver};Database={};", db.display());
    let env = [("MAGI_TEST_ODBC", conn.as_str())];

    let check = fx.magi_env(&["check", "--sources", "odbc.magi"], &env);
    check.assert_code(0).assert_no_diagnostic("M200");

    let out = fx.magi_env(&["run", "--quiet", "odbc.magi"], &env);
    out.assert_code(0).assert_diagnostic("M208");
    assert_eq!(
        fx.read("out/items.csv"),
        "id,name,amount,sold_on\n1,bolt,10.50,2024-01-05\n2,nut,2.25,2024-02-29\n3,,,\n4,washer,12.00,2024-03-01\n"
    );
    assert_eq!(
        fx.read("out/rejects.csv"),
        "row,column,expected,value\n3,sold_on,date,not a date\n"
    );
    assert_eq!(
        fx.read("out/big.csv"),
        "id,name,amount,sold_on\n1,bolt,10.50,2024-01-05\n4,washer,12.00,2024-03-01\n"
    );
    assert_eq!(
        fx.read("out/untyped.csv"),
        "name,n\nbolt,1\nnut,1\nwasher,1\n"
    );
    // the connection string never shows up in what MAGI prints
    assert!(!out.stderr.contains(&conn) && !out.stdout.contains(&conn));
}

// ---------------------------------------------------------------------------------------------
// validation severities and exit codes

#[test]
fn failed_require_stops_the_run_and_writes_nothing() {
    let fx = Fixture::new("validation");
    let out = fx.run("require.magi", &[]);
    out.assert_code(1)
        .assert_diagnostic("M402")
        .assert_diagnostic("M405");
    assert_eq!(
        out.codes().iter().filter(|c| *c == "M402").count(),
        4,
        "{}",
        out.stderr
    );
    assert!(
        !fx.exists("out"),
        "no output may be written after a failed require"
    );
}

#[test]
fn keep_going_writes_outputs_but_still_fails() {
    let fx = Fixture::new("validation");
    let out = fx.run("require.magi", &["--keep-going"]);
    out.assert_code(1)
        .assert_diagnostic("M402")
        .assert_no_diagnostic("M405");
    assert_eq!(
        fx.read("out/positive.csv"),
        "id,customer,amount\n1,acme,10\n3,initech,4\n3,,7\n"
    );
    // `measured` is the compared aggregate of an aggregate check
    assert_eq!(
        fx.read("out/checks.csv"),
        "check,severity,status,failing_rows,total_rows,measured\n\
         amounts are positive,require,fail,1,4,\n\
         at least ten orders,require,fail,,4,4\n\
         customer not null,require,fail,1,4,\n\
         sum(amount) > 0,warn,pass,,4,16\n\
         unique(id),require,fail,2,4,\n"
    );
}

#[test]
fn failed_expect_exits_1_with_outputs_written() {
    let fx = Fixture::new("validation");
    let out = fx.run("expect.magi", &[]);
    out.assert_code(1)
        .assert_diagnostic("M403")
        .assert_no_diagnostic("M405");
    assert_eq!(
        fx.read("out/positive.csv"),
        "id,customer,amount\n1,acme,10\n3,initech,4\n3,,7\n"
    );
    // the two rows with id 3 are told apart by their source row
    assert_eq!(
        fx.read("out/failures.csv"),
        "check,severity,source_row,id,customer,amount\n\
         amounts are positive,expect,2,2,globex,-5\n\
         customer not null,expect,3,3,,7\n\
         unique(id),expect,3,3,,7\n\
         unique(id),expect,4,3,initech,4\n"
    );
}

#[test]
fn failed_warn_exits_0() {
    let fx = Fixture::new("validation");
    let out = fx.run("warn.magi", &[]);
    out.assert_code(0)
        .assert_diagnostic("M404")
        .assert_no_diagnostic("M403")
        .assert_no_diagnostic("M402");
    assert!(
        out.stderr
            .contains("warn `at least ten orders` on `orders`: fails for the relation"),
        "{}",
        out.stderr
    );
    assert_eq!(
        fx.csv("out/checks.csv").project(&["check", "status"]),
        lines![
            "amounts are positive,fail",
            "at least ten orders,fail",
            "customer not null,fail",
            "sum(amount) > 0,pass",
            "unique(id),fail"
        ]
    );
}

// ---------------------------------------------------------------------------------------------
// exports: ordering and reproducibility

#[test]
fn exports_are_ordered_by_sort_keys_then_all_columns() {
    let fx = Fixture::new("pipeline");
    fx.run_ok("pipeline.magi");
    // `sort region desc`; ties (north, south) ordered by product, then amount
    assert_eq!(
        fx.read("out/by_region.csv"),
        "region,product,amount\nsouth,gadget,4.00\nsouth,widget,12.50\nnorth,gadget,7.25\nnorth,widget,3.00\nnorth,widget,12.50\n"
    );
    // unsorted relation: all columns, in column order
    assert_eq!(
        fx.read("out/unsorted.csv"),
        "product,id\ngadget,3\ngadget,5\nwidget,1\nwidget,2\nwidget,7\n"
    );
    assert_eq!(
        fx.read("out/totals.csv"),
        "region,orders,total\nnorth,3,22.75\nsouth,2,16.50\n"
    );
}

#[test]
fn every_export_format_reads_back_to_the_same_rows() {
    let fx = Fixture::new("outputs");
    fx.run_ok("write.magi");
    let expected = "region,orders,total,first_day\n\
                    east,1,,2026-01-07\n\
                    north,2,30.75,2026-01-05\n\
                    south,2,10.10,2026-01-05\n";
    assert_eq!(fx.read("out/totals.csv"), expected);
    assert_eq!(
        fx.read("out/totals.jsonl"),
        "{\"region\":\"east\",\"orders\":1,\"total\":null,\"first_day\":\"2026-01-07\"}\n\
         {\"region\":\"north\",\"orders\":2,\"total\":30.75,\"first_day\":\"2026-01-05\"}\n\
         {\"region\":\"south\",\"orders\":2,\"total\":10.10,\"first_day\":\"2026-01-05\"}\n"
    );
    assert!(
        fx.read("out/totals.json")
            .starts_with("[\n\t{\"region\":\"east\"")
    );
    // Parquet and XLSX files are read back by MAGI itself as sources
    fx.run_ok("read.magi");
    assert_eq!(fx.read("back/from_parquet.csv"), expected);
    assert_eq!(fx.read("back/from_xlsx.csv"), expected);
}

#[test]
fn a_failed_run_changes_no_output_file() {
    let fx = Fixture::new("outputs");
    std::fs::create_dir_all(fx.file("out")).unwrap();
    std::fs::write(fx.file("out/first.csv"), "stale\n").unwrap();
    let out = fx.run("partial.magi", &[]);
    out.assert_code(1).assert_diagnostic("M505");
    // the first export succeeded on its own, but the run failed: the old file stays
    assert_eq!(fx.read("out/first.csv"), "stale\n");
    assert!(!fx.exists("out/first.csv.magi-tmp"));
}

#[test]
fn duckdb_errors_do_not_print_data_values() {
    let fx = Fixture::new("errors");
    for program in ["overflow.magi", "unparsable.magi"] {
        let out = fx.run(program, &[]);
        out.assert_code(1).assert_diagnostic("M400");
        let text = out.stderr_flat();
        for value in ["9223372036854775807", "S3CR3T-CODE"] {
            assert!(!text.contains(value), "{program}: `{value}` in\n{text}");
        }
        assert!(
            text.contains("(details withheld: they quote data)"),
            "{text}"
        );
    }
}

#[test]
fn a_tie_on_either_side_keeps_every_tied_pair_in_that_sides_group() {
    let fx = Fixture::new("ambiguous");
    fx.run_ok("tied.magi");
    // A1 fits both B rows; B102 fits both A rows: each decision lists all of its pairs
    assert_eq!(
        fx.csv("out/ambiguous.csv")
            .project(&["ambiguity_id", "a_id", "b_id"]),
        lines!["1,1,101", "1,1,102", "2,1,102", "2,2,102"]
    );
    assert_eq!(fx.csv("out/matches.csv").rows.len(), 0);
    assert_accounting(&fx);
}

#[test]
fn duckdb_functions_are_checked_against_duckdbs_catalog() {
    let fx = Fixture::new("errors");
    let out = fx.check("natives.magi");
    out.assert_code(1)
        .assert_diagnostic("M107")
        .assert_diagnostic("M123")
        .assert_diagnostic("M115");
    let text = out.stderr_flat();
    assert!(
        text.contains("DuckDB has no function `jaro_winkler`"),
        "{text}"
    );
    assert!(
        text.contains("did you mean `duckdb.jaro_winkler_similarity`?"),
        "{text}"
    );
    assert!(
        text.contains("`duckdb.random` returns different values on every run"),
        "{text}"
    );
}

#[test]
fn two_runs_write_byte_identical_files() {
    let first = Fixture::new("pipeline");
    let second = Fixture::new("pipeline");
    first.run_ok("pipeline.magi");
    second.run_ok("pipeline.magi");
    for file in [
        "out/by_region.csv",
        "out/unsorted.csv",
        "out/totals.csv",
        "out/matches.csv",
        "out/summary.csv",
        "out/totals.parquet",
        "out/report.xlsx",
    ] {
        assert!(
            first.bytes(file) == second.bytes(file),
            "{file} differs between two runs"
        );
    }
}

#[test]
fn reconcile_summary_accounts_for_every_row() {
    for (case, program) in [
        ("pipeline", "pipeline.magi"),
        ("duplicates", "duplicates.magi"),
        ("ambiguous", "ambiguous.magi"),
        ("rollups", "rollups.magi"),
    ] {
        let fx = Fixture::new(case);
        fx.run_ok(program);
        let summary = fx.csv("out/summary.csv");
        assert_accounting(&fx);
        if case == "pipeline" {
            assert_eq!(
                summary.project(&[
                    "position",
                    "step",
                    "tier_index",
                    "pairs",
                    "a_rows",
                    "b_rows"
                ]),
                lines![
                    "1,same_day,1,2,2,2",
                    "2,any_day,2,1,1,1",
                    "3,ambiguous,,0,0,0",
                    "4,duplicate,,0,0,0",
                    "5,unmatched,,0,2,1",
                    "6,total,,0,5,4"
                ]
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// compile-time errors

#[test]
fn limit_without_sort_is_rejected() {
    let fx = Fixture::new("errors");
    fx.check("limit_without_sort.magi")
        .assert_code(1)
        .assert_diagnostic("M118");
}

#[test]
fn union_with_different_columns_is_rejected() {
    let fx = Fixture::new("errors");
    let out = fx.check("union_mismatch.magi");
    out.assert_code(1).assert_diagnostic("M119");
    assert!(
        out.stderr_flat()
            .contains("missing on the right: amount; only on the right: total"),
        "{}",
        out.stderr
    );
}

#[test]
fn import_cycle_is_rejected() {
    let fx = Fixture::new("errors");
    fx.check("cycle_a.magi")
        .assert_code(1)
        .assert_diagnostic("M004");
}

#[test]
fn unknown_column_suggests_the_closest_name() {
    let fx = Fixture::new("errors");
    let out = fx.check("unknown_column.magi");
    out.assert_code(1).assert_diagnostic("M012");
    assert!(
        out.stderr.contains("did you mean `amount`?"),
        "{}",
        out.stderr
    );
}

#[test]
fn column_on_both_sides_of_a_join_is_ambiguous() {
    let fx = Fixture::new("errors");
    let out = fx.check("ambiguous_column.magi");
    out.assert_code(1).assert_diagnostic("M011");
    assert!(
        out.stderr.contains("qualify it: `a.org` or `b.org`"),
        "{}",
        out.stderr
    );
}

// ---------------------------------------------------------------------------------------------
// lineage

#[test]
fn trace_follows_a_column_back_to_its_source() {
    let fx = Fixture::new("pipeline");
    let out = fx.magi(&["trace", "pipeline.magi", "books.matches.s_net"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "books.matches.s_net  (decimal(11,3)?)\n\
         └─ priced.net\n   \
            └─ priced: net = amount * 0.8\n      \
               └─ sales.amount  (CSV sales.csv)\n"
    );
}

// ---------------------------------------------------------------------------------------------
// snapshots: diagnostics, plan, SQL, explain

fn check_stderr(case: &str, program: &str) -> String {
    let fx = Fixture::new(case);
    let out = fx.check(program);
    assert_eq!(out.code, 1, "{}", out.stderr);
    fx.normalize(&out.stderr)
}

#[test]
fn snapshot_diagnostic_unknown_column() {
    insta::assert_snapshot!(
        "diagnostic_unknown_column",
        check_stderr("errors", "unknown_column.magi")
    );
}

#[test]
fn snapshot_diagnostic_ambiguous_column() {
    insta::assert_snapshot!(
        "diagnostic_ambiguous_column",
        check_stderr("errors", "ambiguous_column.magi")
    );
}

#[test]
fn snapshot_diagnostic_union_mismatch() {
    insta::assert_snapshot!(
        "diagnostic_union_mismatch",
        check_stderr("errors", "union_mismatch.magi")
    );
}

#[test]
fn snapshot_diagnostic_import_cycle() {
    insta::assert_snapshot!(
        "diagnostic_import_cycle",
        check_stderr("errors", "cycle_a.magi")
    );
}

#[test]
fn snapshot_plan() {
    let fx = Fixture::new("pipeline");
    let out = fx.magi(&["plan", "pipeline.magi"]);
    out.assert_code(0);
    insta::assert_snapshot!("plan_pipeline", fx.normalize(&out.stdout));
}

#[test]
fn snapshot_sql_dataset() {
    let fx = Fixture::new("pipeline");
    let out = fx.magi(&["sql", "pipeline.magi", "totals"]);
    out.assert_code(0);
    insta::assert_snapshot!("sql_dataset_totals", fx.normalize(&out.stdout));
}

#[test]
fn snapshot_sql_reconcile() {
    let fx = Fixture::new("pipeline");
    let out = fx.magi(&["sql", "small_reconcile.magi", "pair"]);
    out.assert_code(0);
    insta::assert_snapshot!("sql_reconcile_pair", fx.normalize(&out.stdout));
}

#[test]
fn snapshot_explain_dataset() {
    let fx = Fixture::new("pipeline");
    let out = fx.magi(&["explain", "pipeline.magi", "totals"]);
    out.assert_code(0);
    insta::assert_snapshot!("explain_totals", fx.normalize(&out.stdout));
}
