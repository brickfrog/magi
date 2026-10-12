//! End-to-end tests of the `window` step, `asof join`, `magi show` and `magi profile`.

#[macro_use]
#[allow(dead_code)]
mod support;

use support::{Fixture, TODAY, flatten};

#[test]
fn window_functions_follow_partition_and_order() {
    let fx = Fixture::new("window");
    fx.run_ok("window.magi");
    assert_eq!(
        fx.csv("out/running.csv").project(&[
            "acct",
            "row_no",
            "n",
            "previous",
            "next_amount",
            "two_back",
            "so_far",
            "acct_total",
            "acct_lines",
        ]),
        lines![
            "A,1,1,,-5.00,,10.00,25.00,3",
            "A,2,2,110.00,20.00,,5.00,25.00,3",
            "A,3,3,105.00,,110.00,25.00,25.00,3",
            "B,1,1,,7.50,,7.50,15.00,2",
            "B,2,2,57.50,,,15.00,15.00,2",
        ]
    );
}

#[test]
fn ranks_share_ties_and_row_numbers_break_them_by_the_other_columns() {
    let fx = Fixture::new("window");
    fx.run_ok("window.magi");
    assert_eq!(
        fx.csv("out/ranked.csv").project(&["name", "r", "d", "n"]),
        lines!["a,1,1,1", "b,2,2,2", "c,2,2,3", "d,4,3,4"]
    );
}

#[test]
fn a_filter_after_a_window_step_sees_its_columns() {
    let fx = Fixture::new("window");
    fx.run_ok("window.magi");
    assert_eq!(
        fx.csv("out/first_lines.csv").project(&["acct", "row_no"]),
        lines!["A,1", "B,1"]
    );
}

#[test]
fn asof_join_takes_the_latest_row_at_or_before() {
    let fx = Fixture::new("window");
    fx.run_ok("asof.magi");
    assert_eq!(
        fx.csv("out/converted.csv")
            .project(&["ref", "rate_day", "rate"]),
        lines![
            "R1,2026-07-01,1.10",
            "R2,2026-07-15,1.20",
            "R3,,",
            "R4,2026-07-01,1.30",
            "R5,,",
        ]
    );
    assert_eq!(
        fx.csv("out/matched.csv")
            .project(&["ref", "rate_day", "rate"]),
        lines![
            "R1,2026-07-01,1.10",
            "R2,2026-07-15,1.20",
            "R4,2026-07-01,1.30"
        ]
    );
}

const LINES: &str = r#"source lines = csv("lines.csv") {
    identity [acct, row_no]
    schema {
        acct: string
        row_no: int
        amount: decimal(10, 2)
        balance: decimal(10, 2)
    }
}
"#;

const RECEIPTS: &str = r#"source receipts = csv("receipts.csv") {
    identity ref
    schema {
        ref: string
        currency: string
        day: date
        amount: decimal(10, 2)
    }
}
"#;

const RATES: &str = r#"source rates = csv("rates.csv") {
    identity [currency, day]
    schema {
        currency: string
        day: date
        rate: decimal(10, 2)
    }
}
"#;

/// `magi check` of `program` written next to the fixture's data.
fn check(fx: &Fixture, program: &str) -> support::Output {
    std::fs::write(fx.file("case.magi"), program).unwrap();
    fx.check("case.magi")
}

#[test]
fn window_mistakes_are_reported() {
    let fx = Fixture::new("window");
    check(
        &fx,
        &format!("{LINES}dataset d = lines |> derive p = lag(amount)\n"),
    )
    .assert_code(1)
    .assert_diagnostic("M125");
    check(
        &fx,
        &format!("{LINES}dataset d = lines |> window {{ p = lag(amount) }}\n"),
    )
    .assert_code(1)
    .assert_diagnostic("M126");
    check(
        &fx,
        &format!(
            "{LINES}dataset d = lines |> window order by row_no {{ a = lag(amount)  b = a + 1 }}\n"
        ),
    )
    .assert_code(1)
    .assert_diagnostic("M126");
}

#[test]
fn asof_join_mistakes_are_reported() {
    let fx = Fixture::new("window");
    check(
        &fx,
        &format!(
            "{RECEIPTS}{RATES}dataset d = receipts |> asof join rates on receipts.currency == rates.currency\n"
        ),
    )
    .assert_code(1)
    .assert_diagnostic("M127");
    let rates_without_identity = RATES.replace("    identity [currency, day]\n", "");
    check(
        &fx,
        &format!(
            "{RECEIPTS}{rates_without_identity}dataset d = receipts |> asof join rates on receipts.currency == rates.currency and receipts.day >= rates.day\n"
        ),
    )
    .assert_code(1)
    .assert_diagnostic("M128");
}

#[test]
fn show_prints_rows_in_export_order() {
    let fx = Fixture::new("window");
    let out = fx.magi(&[
        "show",
        "window.magi",
        "running",
        "--rows",
        "2",
        "--column",
        "acct",
        "--column",
        "row_no",
        "--column",
        "so_far",
        "--today",
        TODAY,
    ]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "acct  row_no  so_far\nA     1       10.00\nA     2       5.00\n(2 of 5 rows)\n"
    );
    // a show writes no export
    assert!(!fx.exists("out"));

    let out = fx.magi(&[
        "show",
        "window.magi",
        "running",
        "--column",
        "nope",
        "--today",
        TODAY,
    ]);
    out.assert_code(1);
    assert!(
        out.stderr.contains("has no column `nope`"),
        "{}",
        out.stderr
    );

    fx.magi(&["show", "window.magi", "nope", "--today", TODAY])
        .assert_code(1);
}

#[test]
fn profile_prints_column_statistics() {
    let fx = Fixture::new("window");
    let out = fx.magi(&["profile", "window.magi", "lines", "--today", TODAY]);
    out.assert_code(0);
    let mut text = out.stdout.lines();
    assert_eq!(text.next(), Some("lines: 5 rows"));
    let line = |name: &str| {
        out.stdout
            .lines()
            .map(flatten)
            .find(|l| l.starts_with(&format!("{name} ")))
            .unwrap_or_else(|| panic!("no `{name}` line in:\n{}", out.stdout))
    };
    assert!(
        line("amount").contains("0 4 -5.00 20.00 40.00"),
        "{}",
        out.stdout
    );
    assert!(line("row_no").contains("0 3 1 3 9"), "{}", out.stdout);
}
