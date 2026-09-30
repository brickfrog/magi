//! End-to-end tests for names and name resolution: DuckDB's case-insensitive names, the
//! reserved `__magi` prefix, `.` in declared names, join output names and join key types, and
//! the `duckdb.*` escape hatch (function kinds and run-dependent functions).

#[macro_use]
#[allow(dead_code)]
mod support;

use support::Fixture;

// ---------------------------------------------------------------------------------------------
// Case-insensitive names

#[test]
fn relation_names_that_differ_only_in_case_are_one_name() {
    let fx = Fixture::new("names2");
    let out = fx.check("relcase.magi");
    out.assert_code(1)
        .assert_diagnostic("M003")
        // the second spelling is rejected once, not reported again at every use
        .assert_no_diagnostic("M002");
    assert!(
        out.stderr_flat()
            .contains("names are case-insensitive: `Sales` and `sales` are the same name"),
        "{}",
        out.stderr
    );
    assert!(!fx.exists("out/total.csv"));
}

#[test]
fn new_column_names_that_differ_only_in_case_from_a_column_are_rejected() {
    let fx = Fixture::new("names2");
    for (program, clash) in [
        ("colcase_derive.magi", "column `X` clashes with column `x`"),
        ("colcase_rename.magi", "column `V` clashes with column `v`"),
        ("colcase_select.magi", "column `X` clashes with column `x`"),
        (
            "colcase_aggregate.magi",
            "column `V` clashes with column `v`",
        ),
    ] {
        let out = fx.check(program);
        out.assert_code(1).assert_diagnostic("M003");
        assert!(
            out.stderr_flat().contains(clash),
            "{program}:\n{}",
            out.stderr
        );
    }
}

#[test]
fn deriving_an_existing_column_with_its_exact_name_replaces_it() {
    let fx = Fixture::new("names2");
    fx.run_ok("replace.magi");
    let d = fx.csv("out/d.csv");
    assert_eq!(d.header, lines!["x", "v"]);
    assert_eq!(d.column("x"), lines!["10", "20", "30"]);
}

// ---------------------------------------------------------------------------------------------
// Reserved prefix and `.` in names

#[test]
fn reserved_prefix_is_rejected_in_any_case() {
    let fx = Fixture::new("names2");
    fx.check("reserved_upper.magi")
        .assert_code(1)
        .assert_diagnostic("M013");
}

#[test]
fn a_source_column_with_the_reserved_prefix_is_rejected_at_check() {
    let fx = Fixture::new("names2");
    fx.check("reserved_header.magi")
        .assert_code(1)
        .assert_diagnostic("M013");
    let run = fx.run("reserved_header.magi", &[]);
    run.assert_diagnostic("M013").assert_no_diagnostic("M400");
    assert!(!fx.exists("out/a.csv"));
}

#[test]
fn declared_names_containing_a_dot_are_rejected() {
    let fx = Fixture::new("names2");
    // `v.rejects` and `r.matches` would otherwise read the source's rejects / the
    // reconciliation's matches instead of the declared dataset
    for program in ["dotted_part.magi", "dotted_reconcile.magi"] {
        let out = fx.check(program);
        out.assert_code(1).assert_diagnostic("M014");
    }
    fx.run("dotted_part.magi", &[]).assert_diagnostic("M014");
    assert!(!fx.exists("out/vr.csv"));
}

// ---------------------------------------------------------------------------------------------
// Joins

#[test]
fn join_renaming_never_produces_two_columns_with_one_name() {
    // `a.id` would become `a_id`, which `b` already has
    let fx = Fixture::new("names2");
    fx.check("join_prefix.magi")
        .assert_code(0)
        .assert_diagnostic("M112");
    fx.run_ok("join_prefix.magi");
    let d = fx.csv("out/d.csv");
    assert_eq!(d.header, lines!["a_k", "a_id_2", "b_k", "b_id", "a_id"]);
    assert_eq!(d.project(&["a_id_2", "b_id", "a_id"]), lines!["10,20,30"]);
}

#[test]
fn join_renames_a_column_made_in_the_pipeline_after_the_pipeline_input() {
    let fx = Fixture::new("names2");
    fx.run_ok("join_derived.magi");
    let d = fx.csv("out/d.csv");
    assert_eq!(d.header, lines!["a_k", "id", "a_z", "c_k", "c_z"]);
    assert_eq!(d.project(&["a_z", "c_z"]), lines!["1,99"]);
}

#[test]
fn right_and_full_join_keys_are_nullable_when_a_joined_key_is() {
    let fx = Fixture::new("names2");
    for program in ["join_right.magi", "join_full.magi"] {
        let schema = fx.magi(&["schema", program, "j"]);
        schema.assert_code(0);
        assert!(
            support::flatten(&schema.stdout).contains("id int?"),
            "{program}:\n{}",
            schema.stdout
        );
        // the check can fail, so MAGI must not call it redundant
        fx.check(program)
            .assert_code(0)
            .assert_no_diagnostic("M401");
        fx.run(program, &[])
            .assert_code(1)
            .assert_diagnostic("M402");
    }
}

#[test]
fn right_join_key_takes_the_right_keys_nullability() {
    // left key nullable, right key not: every output row has the right key
    let fx = Fixture::new("names2");
    let schema = fx.magi(&["schema", "join_right_nonnull.magi", "j"]);
    let flat = support::flatten(&schema.stdout);
    assert!(
        flat.contains("id int") && !flat.contains("id int?"),
        "{}",
        schema.stdout
    );
    fx.check("join_right_nonnull.magi")
        .assert_diagnostic("M401");
    fx.run_ok("join_right_nonnull.magi");
    assert_eq!(fx.csv("out/j.csv").column("id"), lines!["1"]);
}

// ---------------------------------------------------------------------------------------------
// duckdb.* functions

#[test]
fn run_time_duckdb_functions_are_reported_and_named_by_plan() {
    let fx = Fixture::new("names2");
    let check = fx.check("now.magi");
    // `current_date` comes from DuckDB's ICU extension, which runs load
    check
        .assert_code(0)
        .assert_diagnostic("M123")
        .assert_no_diagnostic("M107");
    let flat = check.stderr_flat();
    for f in ["now", "current_date"] {
        assert!(
            flat.contains(&format!("`duckdb.{f}` depends on when the run happens")),
            "{}",
            check.stderr
        );
    }
    // a stable function gets no reproducibility warning
    assert!(!flat.contains("`duckdb.upper` depends"), "{}", check.stderr);

    let plan = fx.magi(&["plan", "--today", "2026-07-01", "now.magi"]);
    plan.assert_code(0);
    let out = support::flatten(&plan.stdout);
    assert!(
        out.contains(
            "`duckdb.current_date`, `duckdb.now` depend on when the run happens; `--today` does not pin them"
        ),
        "{}",
        plan.stdout
    );
    assert!(!out.contains("duckdb.upper"), "{}", plan.stdout);
}

#[test]
fn duckdb_aggregate_and_table_functions_fail_at_check() {
    let fx = Fixture::new("names2");
    for (program, kind) in [
        (
            "native_aggregate.magi",
            "`duckdb.sum` is an aggregate function",
        ),
        ("native_table.magi", "`duckdb.read_csv` is a table function"),
    ] {
        let out = fx.check(program);
        out.assert_code(1).assert_diagnostic("M107");
        assert!(
            out.stderr_flat().contains(kind),
            "{program}:\n{}",
            out.stderr
        );
    }
    // a name that is both a table and a scalar function is callable as the scalar
    fx.check("native_scalar.magi")
        .assert_code(0)
        .assert_no_diagnostic("M107");
}

// ---------------------------------------------------------------------------------------------
// Steps over inputs with unknown columns, literal arguments

#[test]
fn steps_that_list_their_columns_are_rejected_over_unknown_columns() {
    let fx = Fixture::new("names2");
    // `derive` over a schema-less native_sql result would drop `v` and `w`
    fx.check("open_derive.magi")
        .assert_code(1)
        .assert_diagnostic("M124");
    // `select` names the columns, after which `derive` keeps them
    fx.run_ok("open_select.magi");
    let e = fx.csv("out/e.csv");
    assert_eq!(e.header, lines!["v", "n"]);
    assert_eq!(e.project(&["v", "n"]), lines!["cd,2"]);
}

#[test]
fn regexp_extract_group_must_be_a_literal() {
    let fx = Fixture::new("names2");
    fx.check("regexp_group.magi")
        .assert_code(1)
        .assert_diagnostic("M108");
    fx.run_ok("regexp_literal.magi");
    assert_eq!(fx.csv("out/d.csv").column("r"), lines!["A", ""]);
}
