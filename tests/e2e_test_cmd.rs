//! End-to-end tests of `test` declarations and `magi test`: a program in `program/` run by the
//! tests in `tests/` on the case files in `tests/cases/`.

#[macro_use]
#[allow(dead_code)]
mod support;

use support::Fixture;

fn magi_test(f: &Fixture, file: &str, filter: &str) -> support::Output {
    f.magi(&["test", "--today", support::TODAY, file, "--filter", filter])
}

/// The `given` paths resolve against the test's file and the declarations keep their options
/// (`bank`'s schema turns the case file's `5` into `5.00`); `offices`, a SQL source, is replaced
/// by a CSV file; the expected file names a subset of the columns in another letter case and
/// another row order. Nothing is written.
#[test]
fn test_passes_when_relations_equal_expected_files() {
    let f = Fixture::new("testing");
    let out = magi_test(&f, "tests/rec_test.magi", "matches_by_ref");
    out.assert_code(0);
    assert_eq!(
        out.stdout, "test matches_by_ref ... ok\n1 test: 1 passed, 0 failed\n",
        "{}",
        out.stderr
    );
    assert!(
        !f.exists("program/out"),
        "a test wrote the program's exports"
    );
}

/// Rows are compared as multisets on the file's columns; the differences are listed.
#[test]
fn differing_rows_are_listed_as_missing_and_unexpected() {
    let f = Fixture::new("testing");
    let out = magi_test(&f, "tests/rec_test.magi", "wrong_matches");
    out.assert_code(1);
    let expected = "test wrong_matches ... FAILED\n\
        \x20 rec.matches differs from cases/matches_wrong.csv (on a_ref, b_id): 1 missing row, 1 unexpected row\n\
        \x20   missing:    a_ref=A2, b_id=L9\n\
        \x20   unexpected: a_ref=A2, b_id=L2\n\
        \x20 rec.unmatched_a is not empty: 1 row\n\
        \x20   row: match_status=unmatched, duplicate_of=, ref=A3, amount=9.00, day=2026-02-01\n\
        1 test: 0 passed, 1 failed\n";
    assert_eq!(out.stdout, expected, "{}", out.stderr);
}

/// A failed `require` of the program fails a test, unless the test says what it expects of the
/// validation's checks.
#[test]
fn failed_require_fails_the_test_unless_the_test_expects_its_checks() {
    let f = Fixture::new("testing");
    let out = magi_test(&f, "tests/rec_test.magi", "zero_amount");
    out.assert_code(1);
    assert_eq!(
        out.stdout,
        "test zero_amount ... FAILED\n\
        \x20 check `no zero amounts` on bank failed (1 row)\n\
        test zero_amount_checked ... ok\n\
        2 tests: 1 passed, 1 failed\n",
        "{}",
        out.stderr
    );
}

/// `today:` pins `today()` for its test; a test without one uses `--today`.
#[test]
fn today_in_a_test_wins_over_the_command_line() {
    let f = Fixture::new("testing");
    let out = f.magi(&[
        "test",
        "--today",
        "2026-01-01",
        "tests/rec_test.magi",
        "--filter",
        "today",
    ]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "test today_pinned ... ok\ntest today_from_command_line ... ok\n2 tests: 2 passed, 0 failed\n",
        "{}",
        out.stderr
    );
}

#[test]
fn a_filter_that_selects_no_test_is_a_usage_error() {
    let f = Fixture::new("testing");
    magi_test(&f, "tests/rec_test.magi", "no_such_test").assert_code(2);
}

/// Tests read only files: a SQL source that no `given` replaces fails the test before it runs.
#[test]
fn a_test_that_keeps_a_sql_source_fails() {
    let f = Fixture::new("testing");
    let out = f.magi(&["test", "--today", support::TODAY, "tests/sql_test.magi"]);
    out.assert_code(1).assert_diagnostic("M707");
    assert!(
        out.stdout.starts_with("test keeps_database ... FAILED\n"),
        "{}",
        out.stdout
    );
}

/// `magi check` analyses the tests: a duplicate name, a `given` that names no source and an
/// expected file whose header names a column the relation does not have.
#[test]
fn check_reports_errors_in_tests() {
    let f = Fixture::new("testing");
    let out = f.check("tests/bad_test.magi");
    out.assert_code(1);
    let mut codes = out.codes();
    codes.sort();
    assert_eq!(codes, lines!["M701", "M702", "M706"], "{}", out.stderr);
    assert!(
        out.stderr_flat().contains("did you mean `b_id`?"),
        "{}",
        out.stderr
    );
}
