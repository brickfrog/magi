//! End-to-end tests of `magi snapshot` and `magi diff`: a snapshot holds every relation of a
//! run, and a diff reports which relations an edit to the policy or the inputs changed.

#[macro_use]
#[allow(dead_code)]
mod support;

use support::Fixture;

const SNAP: &str = "rec.snapshot.duckdb";

#[test]
fn a_snapshot_of_an_unchanged_program_shows_no_differences() {
    let fx = Fixture::new("snapshot");
    fx.magi(&["snapshot", "rec.magi"]).assert_code(0);
    assert!(fx.exists(SNAP));
    // a snapshot writes no export
    assert!(!fx.exists("out"));
    let out = fx.magi(&["diff", SNAP, "rec.magi"]);
    out.assert_code(0);
    assert!(out.stdout.contains("no differences"), "{}", out.stdout);
}

#[test]
fn an_edited_policy_reports_only_the_relations_it_changed() {
    let fx = Fixture::new("snapshot");
    fx.magi(&["snapshot", "rec.magi"]).assert_code(0);
    let out = fx.magi(&["diff", SNAP, "rec_late.magi"]);
    out.assert_code(1);
    assert!(
        out.stdout
            .contains("rec.matches: 1 row added, 0 removed (1 → 2 rows)"),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout
            .contains("rec.unmatched_a: 0 rows added, 1 removed (2 → 1 rows)"),
        "{}",
        out.stdout
    );
    // unchanged relations are not listed, and no data value is printed unless asked
    assert!(!out.stdout.contains("ledger:"), "{}", out.stdout);
    assert!(!out.stdout.contains("L2"), "{}", out.stdout);

    let out = fx.magi(&["diff", SNAP, "rec_late.magi", "--rows", "5"]);
    out.assert_code(1);
    assert!(out.stdout.contains("a_id=L2, b_ref=B2"), "{}", out.stdout);

    let out = fx.magi(&["diff", SNAP, "rec_late.magi", "--relation", "ledger"]);
    out.assert_code(0);
    let out = fx.magi(&["diff", SNAP, "rec_late.magi", "--relation", "nope"]);
    out.assert_code(2);
}

#[test]
fn changed_inputs_and_columns_are_reported() {
    let fx = Fixture::new("snapshot");
    fx.magi(&["snapshot", "rec.magi"]).assert_code(0);
    std::fs::write(
        fx.file("bank.csv"),
        "ref,amount,day\nB1,100.00,2026-03-02\nB2,250.00,2026-03-05\nB3,75.50,2026-03-04\n",
    )
    .unwrap();
    fx.magi(&["snapshot", "rec.magi", "--out", "new.duckdb"])
        .assert_code(0);
    let out = fx.magi(&["diff", SNAP, "new.duckdb"]);
    out.assert_code(1);
    assert!(
        out.stdout.contains("inputs: changed bank.csv"),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout
            .contains("bank: 1 row added, 0 removed (2 → 3 rows)"),
        "{}",
        out.stdout
    );

    let out = fx.magi(&[
        "diff",
        "new.duckdb",
        "rec_note.magi",
        "--relation",
        "ledger",
    ]);
    out.assert_code(1);
    assert!(out.stdout.contains("columns added: note"), "{}", out.stdout);
}

#[test]
fn a_run_that_cannot_happen_leaves_the_old_snapshot() {
    let fx = Fixture::new("snapshot");
    fx.magi(&["snapshot", "rec.magi"]).assert_code(0);
    let before = fx.bytes(SNAP);
    fx.magi(&["snapshot", "rec_missing.magi", "--out", SNAP])
        .assert_code(1);
    assert_eq!(fx.bytes(SNAP), before);
    // a file that is not a snapshot is refused
    fx.magi(&["diff", "bank.csv", SNAP]).assert_code(2);
}
