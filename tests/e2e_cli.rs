//! End-to-end tests of the command-line surface: parse-error recovery, `--today` on every
//! analysing command, and what `sql` / `explain` say about incomplete schemas and consumption.

#[macro_use]
#[allow(dead_code)]
mod support;

use support::Fixture;

/// A source passing `sheet` as an argument and an undeclared connection: only the real problems
/// are reported, and the misplaced option gets a hint. The source that failed to parse must not
/// come back as an unknown relation.
#[test]
fn parse_error_does_not_cascade_into_unknown_relation() {
    let f = Fixture::new("cli");
    let out = f.check("mixed.magi");
    out.assert_code(1);
    assert_eq!(out.codes(), lines!["M001", "M009"], "{}", out.stderr);
    assert!(
        out.stderr_flat()
            .contains("options go in a `{ }` block after the parentheses"),
        "{}",
        out.stderr
    );
}

/// References to a dataset and to reconcile outputs whose declarations failed to parse stay
/// silent; a name that is really undefined is still reported.
#[test]
fn failed_declarations_are_not_reported_again() {
    let f = Fixture::new("cli");
    let out = f.check("cascade.magi");
    out.assert_code(1);
    assert_eq!(
        out.codes(),
        lines!["M001", "M001", "M002"],
        "{}",
        out.stderr
    );
    let flat = out.stderr_flat();
    assert!(flat.contains("unknown relation `missing`"), "{flat}");
    assert!(!flat.contains("`broken`"), "{flat}");
    assert!(!flat.contains("`later`"), "{flat}");
    assert!(!flat.contains("unknown relation `r`"), "{flat}");
}

/// `magi sql --today` shows the date `magi run --today` uses.
#[test]
fn sql_shows_the_pinned_today() {
    let f = Fixture::new("cli");
    let out = f.magi(&["sql", "--today", "2024-01-01", "today.magi", "d"]);
    out.assert_code(0);
    assert!(out.stdout.contains("DATE '2024-01-01'"), "{}", out.stdout);

    f.magi(&["run", "--quiet", "--today", "2024-01-01", "today.magi"])
        .assert_code(0);
    assert_eq!(
        f.csv("out/d.csv").column("day"),
        lines!["2024-01-01", "2024-01-01"]
    );
}

#[test]
fn every_analysing_command_accepts_today() {
    let f = Fixture::new("cli");
    for args in [
        &["check"][..],
        &["plan"],
        &["explain", "d"],
        &["schema", "d"],
        &["trace", "d.day"],
    ] {
        let mut cmd = vec![args[0], "--today", "2024-02-29", "today.magi"];
        cmd.extend_from_slice(&args[1..]);
        f.magi(&cmd).assert_code(0);
    }
    let plan = f.magi(&["plan", "--today", "2024-02-29", "today.magi"]);
    assert!(
        plan.stdout.contains("`today()` is 2024-02-29"),
        "{}",
        plan.stdout
    );
    let explain = f.magi(&["explain", "--today", "2024-02-29", "today.magi", "d"]);
    assert!(explain.stdout.contains("2024-02-29"), "{}", explain.stdout);
}

#[test]
fn today_must_be_a_calendar_date() {
    let f = Fixture::new("cli");
    for (bad, message) in [
        ("01/02/2024", "YYYY-MM-DD"),
        ("2024-1-01", "YYYY-MM-DD"),
        ("2024-13-01", "month 13 does not exist"),
        ("2023-02-29", "2023-02 has no day 29"),
        ("2024-04-31", "2024-04 has no day 31"),
    ] {
        for cmd in ["run", "sql"] {
            let out = f.magi(&[cmd, "--today", bad, "today.magi"]);
            out.assert_code(2);
            assert!(out.stderr.contains(message), "{bad}: {}", out.stderr);
        }
    }
    assert!(!f.exists("out/d.csv"));
}

/// Without `--sources` the columns of an undeclared SQL source are unknown, so the SQL and plan
/// shown for relations built on it are incomplete, and say so first.
#[test]
fn sql_and_explain_flag_unknown_source_schemas() {
    let f = Fixture::new("cli");
    let sql = f.magi(&["sql", "open.magi", "d"]);
    sql.assert_code(0);
    assert!(
        sql.stdout
            .starts_with("-- incomplete: the schema of source `s` is unknown"),
        "{}",
        sql.stdout
    );
    assert!(sql.stdout.contains("--sources"), "{}", sql.stdout);

    let all = f.magi(&["sql", "open.magi"]);
    all.assert_code(0);
    assert!(
        all.stdout
            .starts_with("-- incomplete: the schema of source `s`"),
        "{}",
        all.stdout
    );

    let explain = f.magi(&["explain", "open.magi", "d"]);
    explain.assert_code(0);
    assert!(
        explain
            .stdout
            .starts_with("note: incomplete: the schema of source `s` is unknown"),
        "{}",
        explain.stdout
    );

    // relations that do not read `s` are complete
    for cmd in ["sql", "explain"] {
        let out = f.magi(&[cmd, "open.magi", "k"]);
        out.assert_code(0);
        assert!(!out.stdout.contains("incomplete"), "{}", out.stdout);
    }
}

#[test]
fn explain_reports_where_a_source_schema_comes_from() {
    let f = Fixture::new("cli");
    for (source, schema) in [
        ("s", "schema: unknown (not contacted; use --sources)"),
        ("t", "schema: inferred from the source"),
        ("typed", "schema: declared (source contract)"),
    ] {
        let out = f.magi(&["explain", "open.magi", source]);
        out.assert_code(0);
        assert!(out.stdout.contains(schema), "{source}: {}", out.stdout);
    }
}

#[test]
fn explain_describes_the_consume_policy() {
    let f = Fixture::new("consumption");
    for (reconcile, text) in [
        (
            "both",
            "rows matched by a tier are not seen by later tiers)",
        ),
        (
            "none",
            "every tier sees every row, but a pair matched once is not matched again",
        ),
        (
            "only_a",
            "side a rows matched by a tier are not seen by later tiers; side b rows stay available to later tiers",
        ),
        (
            "only_b",
            "side b rows matched by a tier are not seen by later tiers; side a rows stay available to later tiers",
        ),
    ] {
        let out = f.magi(&["explain", "consumption.magi", reconcile]);
        out.assert_code(0);
        assert!(
            out.stdout
                .contains(&format!("tiers, in priority order ({text}")),
            "{reconcile}: {}",
            out.stdout
        );
    }
}

/// `magi fmt` takes directories: every `.magi` file below them is checked or rewritten (hidden
/// entries and other files are left alone), and `--check` names every unformatted file.
#[test]
fn fmt_formats_every_program_under_a_directory() {
    let f = Fixture::new("fmt");
    let messy = f.read("messy.magi");
    let out = f.magi(&["fmt", "--check", "."]);
    out.assert_code(1);
    let listed: Vec<&str> = out.stderr.lines().collect();
    assert_eq!(
        listed,
        [
            "./messy.magi is not formatted",
            "./sub/also_messy.magi is not formatted"
        ],
        "{}",
        out.stderr
    );
    assert_eq!(f.read("messy.magi"), messy, "--check must not write");

    f.magi(&["fmt", "."]).assert_code(0);
    f.magi(&["fmt", "--check", "."]).assert_code(0);
    assert_eq!(f.read("messy.magi"), f.read("ok.magi"));
    assert_eq!(f.read("sub/also_messy.magi"), f.read("ok.magi"));
    assert_eq!(f.read(".hidden/skipped.magi"), messy);
    assert_eq!(f.read("notes.txt"), "not a program {\n");

    let out = f.magi(&["fmt", "--stdout", "ok.magi", "sub"]);
    out.assert_code(2);
    assert!(
        out.stderr.contains("`--stdout` prints one formatted file"),
        "{}",
        out.stderr
    );
}
