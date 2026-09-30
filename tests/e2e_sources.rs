//! End-to-end tests for source reading: CSV type inference and table shape, declared types,
//! Excel cells and ODBC connection options.

#[macro_use]
#[allow(dead_code)]
mod support;

use support::Fixture;

// ---------------------------------------------------------------------------------------------
// CSV inference

#[test]
fn csv_decimals_are_exact_and_runs_are_reproducible() {
    let fx = Fixture::new("sources_csv");
    // amounts that no binary float holds, spread over groups so parallel sums interleave them
    let rows = 300_000u64;
    let mut text = String::from("k,amount\n");
    let mut expected = [0u64; 7]; // in ten-thousandths
    for i in 0..rows {
        let cents = 1 + (i * 7919) % 99_999; // 0.0001 .. 9.9999
        let k = (i % 7) as usize;
        expected[k] += cents;
        text += &format!("{k},{}.{:04}\n", cents / 10_000, cents % 10_000);
    }
    std::fs::write(fx.file("money.csv"), text).unwrap();
    std::fs::write(
        fx.file("money.magi"),
        "source s = csv(\"money.csv\")\n\
         dataset t = s |> group by k |> aggregate { total = sum(amount) }\n\
         export t to \"out/t.csv\"\n",
    )
    .unwrap();

    let schema = fx.magi(&["schema", "money.magi", "s"]);
    schema.assert_code(0);
    assert!(
        support::flatten(&schema.stdout).contains("amount decimal(18,4)?"),
        "{}",
        schema.stdout
    );
    fx.run_ok("money.magi");
    let first = fx.read("out/t.csv");
    fx.run_ok("money.magi");
    assert_eq!(fx.read("out/t.csv"), first);
    let mut want = String::from("k,total\n");
    for (k, v) in expected.iter().enumerate() {
        want += &format!("{k},{}.{:04}\n", v / 10_000, v % 10_000);
    }
    assert_eq!(first, want);
}

#[test]
fn csv_long_numeric_ids_and_leading_zero_codes_stay_text() {
    let fx = Fixture::new("sources_csv");
    let check = fx.check("ids.magi");
    check.assert_code(0).assert_diagnostic("M201");
    assert!(
        check
            .stderr_flat()
            .contains("column acct: 2 integers are too large for a 64-bit int"),
        "{}",
        check.stderr
    );
    fx.run_ok("ids.magi");
    assert_eq!(
        fx.read("out/ids.csv"),
        "acct,code\n1,42\n12345678901234567890,007\n12345678901234567891,00123\n"
    );
}

#[test]
fn csv_declared_numeric_types_read_codes_and_long_ids_as_numbers() {
    let fx = Fixture::new("sources_csv");
    fx.check("ids_declared.magi")
        .assert_code(0)
        .assert_no_diagnostic("M203");
    fx.run_ok("ids_declared.magi");
    assert_eq!(
        fx.read("out/ids.csv"),
        "acct,code\n1,42\n12345678901234567890,7\n12345678901234567891,123\n"
    );
}

#[test]
fn csv_non_iso_dates_are_text_with_a_note_naming_the_format() {
    let fx = Fixture::new("sources_csv");
    let check = fx.check("dates.magi");
    check.assert_code(0);
    assert!(
        check
            .stderr_flat()
            .contains("declare `dmy: date(\"%d/%m/%Y\")` to read them as dates"),
        "{}",
        check.stderr
    );
    fx.run_ok("dates.magi");
    let dates = fx.csv("out/dates.csv");
    assert_eq!(dates.column("dmy"), lines!["25/12/2026", "31/01/2026"]);

    fx.run_ok("dates_declared.magi");
    let dates = fx.csv("out/dates.csv");
    assert_eq!(dates.column("dmy"), lines!["2026-12-25", "2026-01-31"]);
}

// ---------------------------------------------------------------------------------------------
// declared types

#[test]
fn declared_int_rejects_fractions_instead_of_rounding() {
    let fx = Fixture::new("sources_csv");
    let check = fx.check("values.magi");
    check.assert_code(0).assert_diagnostic("M203");
    assert!(
        check
            .stderr_flat()
            .contains("column `amount` is declared `int?` but the source reports `decimal(18,1)`"),
        "{}",
        check.stderr
    );
    let out = fx.run_ok("values.magi");
    out.assert_diagnostic("M208");
    assert_eq!(
        fx.read("out/rejects.csv"),
        "row,column,expected,value\n1,amount,int,1.5\n"
    );
    let values = fx.csv("out/values.csv");
    assert_eq!(values.column("amount"), lines!["", "2"]);
}

#[test]
fn declared_date_rejects_a_trailing_time() {
    let fx = Fixture::new("sources_csv");
    let out = fx.run_ok("dates.magi");
    out.assert_diagnostic("M203").assert_diagnostic("M208");
    assert_eq!(
        fx.read("out/rejects.csv"),
        "row,column,expected,value\n1,d,date,2026-01-05 13:45:00\n"
    );
    assert_eq!(
        fx.csv("out/dates.csv").column("d"),
        lines!["", "2026-01-06"]
    );
}

#[test]
fn declared_time_and_timestamp_tz_formats_are_applied() {
    let fx = Fixture::new("sources_csv");
    let out = fx.run_ok("tz.magi");
    out.assert_no_diagnostic("M208");
    assert_eq!(
        fx.read("out/tz.csv"),
        "id,t,ts\n1,13:45:00,2025-12-31 21:00:00+00\n2,09:05:00,2026-01-02 10:00:00+00\n"
    );
    // `values.magi` declares `t: time("%H.%M")` too
    fx.run_ok("values.magi");
    assert_eq!(
        fx.csv("out/values.csv").column("t"),
        lines!["13:45:00", "09:05:00"]
    );
}

#[test]
fn m209_reports_only_values_the_declared_scale_changes() {
    let fx = Fixture::new("sources_csv");
    // 12.500 and 3.1 fit decimal(10,2) exactly
    fx.run_ok("values.magi").assert_no_diagnostic("M209");
    assert_eq!(
        fx.csv("out/values.csv").column("big"),
        lines!["12.50", "3.10"]
    );
    // 1.2345e-1 becomes 0.12
    let out = fx.run_ok("rounded.magi");
    out.assert_diagnostic("M209");
    assert!(
        out.stderr_flat()
            .contains("1 value(s) in column `big` have more than 2 decimal places"),
        "{}",
        out.stderr
    );
    assert_eq!(fx.read("out/rounded.csv"), "id,big\n1,0.12\n2,4.50\n");
}

// ---------------------------------------------------------------------------------------------
// CSV table shape

#[test]
fn ragged_csv_is_an_error_naming_lines_not_values() {
    let fx = Fixture::new("sources_csv");
    for (program, line) in [("ragged.magi", "line 4"), ("skipped.magi", "line 3")] {
        let out = fx.check(program);
        out.assert_code(1).assert_diagnostic("M212");
        out.assert_no_diagnostic("M202");
        let err = out.stderr_flat();
        assert!(err.contains("as a table"), "{program}: {err}");
        assert!(err.contains(line), "{program}: {err}");
        assert!(!err.contains("SECRETVALUE"), "{program}: {err}");
        let run = fx.run(program, &[]);
        run.assert_code(1).assert_diagnostic("M212");
        assert!(!run.stderr.contains("SECRETVALUE"), "{}", run.stderr);
        assert!(!fx.exists("out"));
    }
}

// ---------------------------------------------------------------------------------------------
// Excel

#[test]
fn excel_numbers_stored_as_text_are_not_rewritten() {
    let fx = Fixture::new("sources_excel");
    fx.run_ok("codes.magi");
    let codes = fx.csv("out/codes.csv");
    assert_eq!(codes.column("code"), lines!["00123", "007", "1.50"]);
    assert_eq!(
        codes.column("id"),
        lines![
            "1234567890123456789012",
            "1234567890123456789013",
            "1234567890123456789014"
        ]
    );
}

#[test]
fn excel_date_cells_are_accepted_by_a_declared_date_format() {
    let fx = Fixture::new("sources_excel");
    let out = fx.run_ok("dates.magi");
    out.assert_no_diagnostic("M208");
    assert_eq!(
        fx.csv("out/dates.csv").column("d"),
        lines!["2026-01-05", "2026-01-06", "2026-01-07"]
    );
    assert_eq!(fx.read("out/rejects.csv"), "row,column,expected,value\n");
}

#[test]
fn excel_merged_cells_overlapping_the_table_are_reported() {
    let fx = Fixture::new("sources_excel");
    let out = fx.check("merged.magi");
    out.assert_code(0).assert_diagnostic("M201");
    let err = out.stderr_flat();
    assert!(
        err.contains("1 merged range overlaps the table (B2:B3)"),
        "{err}"
    );
    assert!(!err.contains("`X`"), "{err}");
}

// ---------------------------------------------------------------------------------------------
// ODBC connections

#[test]
fn odbc_auth_accepts_only_integrated() {
    let fx = Fixture::new("sources_odbc");
    fx.check("auth.magi")
        .assert_code(0)
        .assert_no_diagnostic("M007");
    let typo = fx.check("auth_typo.magi");
    typo.assert_code(1).assert_diagnostic("M007");
    assert!(
        typo.stderr_flat().contains("did you mean `integrated`?"),
        "{}",
        typo.stderr
    );
    fx.check("auth_unknown.magi")
        .assert_code(1)
        .assert_diagnostic("M007");
}
