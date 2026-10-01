//! End-to-end tests of execution and export: all-or-nothing output commits, aliased output
//! paths, null aggregate checks, `<target>.checks` column types, generated SQL for negation,
//! comparisons and INTEGER-only DuckDB arguments, verbatim raw SQL, exact XLSX numbers, and the
//! session settings `magi sql` prints.

#[macro_use]
#[allow(dead_code)]
mod support;

use support::Fixture;

/// Files in `dir` of the fixture, sorted (hidden temp/backup files included).
fn listing(f: &Fixture, dir: &str) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(f.file(dir))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A failure while moving outputs into place changes none of them — not the ones before the
/// failing export, not the ones after — and leaves no temp or backup file behind.
#[test]
fn a_failed_commit_changes_no_output_and_leaves_no_temp_files() {
    let f = Fixture::new("backend2");
    std::fs::create_dir_all(f.file("out/blocker.csv")).unwrap();
    std::fs::write(f.file("out/first.csv"), "old\n").unwrap();
    std::fs::write(f.file("out/zlast.csv"), "old\n").unwrap();
    let out = f.run("stage.magi", &[]);
    out.assert_code(1).assert_diagnostic("M505");
    assert_eq!(f.read("out/first.csv"), "old\n");
    assert_eq!(f.read("out/zlast.csv"), "old\n");
    assert_eq!(
        listing(&f, "out"),
        lines!["blocker.csv", "first.csv", "zlast.csv"]
    );

    // with the directory gone every output is replaced, and nothing else remains
    std::fs::remove_dir(f.file("out/blocker.csv")).unwrap();
    f.run_ok("stage.magi");
    for file in ["out/first.csv", "out/blocker.csv", "out/zlast.csv"] {
        assert!(f.read(file).starts_with("id,amount,flag,name\n"), "{file}");
    }
    assert_eq!(
        listing(&f, "out"),
        lines!["blocker.csv", "first.csv", "zlast.csv"]
    );
}

/// `out/a.csv` and `out/../out/a.csv`, or `./out/b.csv` and `out/b.csv`, are one file.
#[test]
fn aliased_output_paths_are_one_file() {
    let f = Fixture::new("backend2");
    let out = f.check("alias.magi");
    out.assert_code(1);
    assert_eq!(out.codes(), lines!["M504", "M504"], "{}", out.stderr);
    // the directory existing (canonical paths) gives the same answer
    std::fs::create_dir_all(f.file("out")).unwrap();
    let out = f.check("alias.magi");
    assert_eq!(out.codes(), lines!["M504", "M504"], "{}", out.stderr);
}

/// `require sum(amount) > 0` over no rows compares null: it fails and stops the run.
#[test]
fn an_aggregate_check_over_no_rows_fails() {
    let f = Fixture::new("backend2");
    let out = f.run("empty_agg.magi", &[]);
    out.assert_code(1)
        .assert_diagnostic("M402")
        .assert_diagnostic("M405");
    assert!(
        out.stderr_flat().contains("the aggregate is null"),
        "{}",
        out.stderr
    );
    assert!(!f.exists("big.csv"));
}

/// Over only nulls, `expect`/`warn` aggregate checks fail with an empty `measured`; a check whose
/// aggregate is not null still passes.
#[test]
fn an_aggregate_check_over_only_nulls_fails_with_null_measured() {
    let f = Fixture::new("backend2");
    let out = f.run("null_agg.magi", &[]);
    out.assert_code(1)
        .assert_diagnostic("M403")
        .assert_diagnostic("M404");
    let checks = f.csv("checks.csv");
    assert_eq!(
        checks.project(&["check", "status", "measured"]),
        lines![
            "count() == 2,pass,2",
            "mean(amount) > 100,fail,",
            "sum(amount) > 0,fail,",
        ]
    );
}

/// With only row checks every `measured` is null and `failing_rows` small: the exported table
/// still has the documented types (read back from Parquet).
#[test]
fn checks_columns_have_fixed_types() {
    let f = Fixture::new("backend2");
    f.run_ok("checks_types.magi");
    let out = f.magi(&["schema", "checks_read.magi", "c"]);
    out.assert_code(0);
    let types: Vec<String> = out
        .stdout
        .lines()
        .skip(1)
        .map(|l| l.split_whitespace().take(2).collect::<Vec<_>>().join(" "))
        .collect();
    assert_eq!(
        types,
        lines![
            "check string?",
            "severity string?",
            "status string?",
            "failing_rows int?",
            "total_rows int?",
            "measured string?",
        ],
        "{}",
        out.stdout
    );
}

/// `floor`, `ceil` and `round` declare the types DuckDB produces (read back from Parquet): an int
/// stays an exact int beyond 2^53, and a decimal gets the scale of the rounding.
#[test]
fn rounding_functions_declare_the_types_they_produce() {
    let f = Fixture::new("backend2");
    f.run_ok("rounding.magi");
    let types = |program: &str, relation: &str| -> Vec<String> {
        let out = f.magi(&["schema", program, relation]);
        out.assert_code(0);
        out.stdout
            .lines()
            .skip(1)
            .map(|l| {
                let mut w = l.split_whitespace();
                let (name, ty) = (w.next().unwrap(), w.next().unwrap());
                format!("{name} {}", ty.trim_end_matches('?'))
            })
            .collect()
    };
    let declared = types("rounding.magi", "d");
    assert_eq!(declared, types("rounding_read.magi", "back"));
    assert_eq!(
        declared[3..],
        lines![
            "n_floor int",
            "n_ceil int",
            "n_round int",
            "m_floor decimal(18,0)",
            "m_ceil decimal(18,0)",
            "m_round decimal(18,0)",
            "m_round1 decimal(18,1)",
            "m_round5 decimal(18,2)",
            "m_round_neg decimal(18,0)",
        ]
    );
    assert_eq!(
        f.csv("rounding_out.csv")
            .project(&["n_floor", "n_ceil", "m_floor", "m_ceil"]),
        lines![
            "9007199254740993,9007199254740993,12,13",
            "-9007199254740993,-9007199254740993,-13,-12",
        ]
    );
}

/// DuckDB needs the digits of `round` on a decimal when it plans the query: `check` refuses
/// anything but a literal instead of the run failing.
#[test]
fn round_of_a_decimal_needs_literal_digits() {
    let f = Fixture::new("backend2");
    let out = f.check("round_digits.magi");
    out.assert_code(1).assert_diagnostic("M108");
    assert!(
        out.stderr_flat()
            .contains("the digits of `round` on a decimal must be a literal number"),
        "{}",
        out.stderr
    );
}

/// `-(-x)`, `- -1.5` and nested comparisons run in derive, filter and validation.
#[test]
fn double_negation_and_nested_comparisons_run() {
    let f = Fixture::new("backend2");
    f.run_ok("neg.magi");
    assert_eq!(
        f.csv("d.csv").project(&["id", "y", "z", "w"]),
        lines!["1,5,1.5,5"]
    );
}

/// The same inside a reconciliation's `require`, `rank by` and evidence.
#[test]
fn double_negation_runs_in_a_reconciliation() {
    let f = Fixture::new("backend2");
    f.run_ok("neg_rc.magi");
    assert_eq!(
        f.csv("matches.csv").project(&["a_id", "b_id", "e"]),
        lines!["1,8,10", "2,7,20"]
    );
}

/// `lpad`/`rpad`/`round` accept computed int arguments (DuckDB wants INTEGER, MAGI ints are
/// BIGINT).
#[test]
fn integer_arguments_may_be_computed() {
    let f = Fixture::new("backend2");
    f.run_ok("pad.magi");
    assert_eq!(
        f.csv("d.csv").project(&["l", "r", "o"]),
        lines!["xxxab,abyy,1.67"]
    );
}

/// A multi-line string literal in a source `query:` or `native_sql` keeps its exact text.
#[test]
fn raw_sql_is_not_reindented() {
    let f = Fixture::new("backend2");
    f.run_ok("raw_mk.magi");
    f.run_ok("raw.magi");
    assert_eq!(f.csv("d.csv").project(&["v", "n"]), lines!["a\nb,3"]);
    assert_eq!(f.csv("e.csv").project(&["v", "n"]), lines!["c\nd,3"]);
}

/// UBIGINT beyond i64 and a DECIMAL(38,2) with 22 digits are written as exact text cells, with a
/// warning per column; a short decimal stays a number.
#[test]
fn xlsx_keeps_numbers_an_excel_double_cannot_hold() {
    let f = Fixture::new("backend2");
    let out = f.run_ok("big_mk.magi");
    let flat = out.stderr_flat();
    assert_eq!(
        out.codes().iter().filter(|c| *c == "M506").count(),
        2,
        "{}",
        out.stderr
    );
    assert!(flat.contains("column `big`"), "{flat}");
    assert!(flat.contains("column `d`"), "{flat}");
    assert!(!flat.contains("column `small`"), "{flat}");
    f.run_ok("big_read.magi");
    assert_eq!(
        f.csv("back.csv").project(&["big", "d", "small"]),
        lines!["18446744073709551615,12345678901234567890.12,1.25"]
    );
}

/// Whole-program `magi sql` starts with the session settings `magi run` applies.
#[test]
fn whole_program_sql_starts_with_the_session_settings() {
    let f = Fixture::new("backend2");
    let out = f.magi(&["sql", "settings.magi"]);
    out.assert_code(0);
    let sets: Vec<&str> = out
        .stdout
        .lines()
        .take_while(|l| !l.is_empty())
        .filter(|l| !l.starts_with("--"))
        .collect();
    assert_eq!(
        sets,
        vec![
            "SET preserve_insertion_order = TRUE;",
            "SET TimeZone = 'UTC';",
            "SET temp_directory = '';",
            "SET threads = 2;",
            "SET memory_limit = '1GB';",
        ],
        "{}",
        out.stdout
    );
}
