//! End-to-end tests of explicit cross joins (`join x on true`) and of source identity columns
//! being non-null downstream.

#[macro_use]
#[allow(dead_code)]
mod support;

use support::*;

#[test]
fn join_on_true_pairs_every_row_with_every_row_without_m121() {
    let fx = Fixture::new("datasets3");
    let out = fx.run_ok("cross.magi");
    out.assert_no_diagnostic("M121");
    assert_eq!(
        fx.csv("out/shares.csv").project(&["id", "amount", "total"]),
        lines![
            "L1,10.000000,7.750000",
            "L2,-4.500000,7.750000",
            "L3,2.250000,7.750000",
        ]
    );
}

#[test]
fn a_join_condition_that_never_mentions_the_joined_relation_still_warns() {
    let fx = Fixture::new("datasets3");
    fx.check("no_mention.magi")
        .assert_code(0)
        .assert_diagnostic("M121");
}

#[test]
fn source_identity_columns_are_non_null_downstream() {
    let fx = Fixture::new("datasets3");
    let schema = fx.magi(&["schema", "identity.magi", "a"]);
    schema.assert_code(0);
    let id_line = schema
        .stdout
        .lines()
        .find(|l| l.trim_start().starts_with("id "))
        .expect("id column listed");
    assert!(
        id_line.contains("string") && !id_line.contains("string?"),
        "identity column should be non-null: {id_line}"
    );
    let amount_line = schema
        .stdout
        .lines()
        .find(|l| l.trim_start().starts_with("amount "))
        .expect("amount column listed");
    assert!(amount_line.contains("string?"), "{amount_line}");
    // a reconciliation identity taken from a source identity needs no null warning
    fx.check("identity.magi")
        .assert_code(0)
        .assert_no_diagnostic("M206");
}
