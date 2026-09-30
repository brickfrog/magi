//! End-to-end tests of source reading that once lost or changed values silently: CSV quoting and
//! comment characters, backslash escapes, dates at midnight, temporal inference, and fixed-width
//! files without line breaks.

#[macro_use]
#[allow(dead_code)]
mod support;

use support::Fixture;

/// A data line starting with `#` is data, not a comment, and a `'` is not a quote character.
#[test]
fn csv_lines_starting_with_a_hash_and_single_quotes_are_data() {
    let fx = Fixture::new("sources4");
    fx.run_ok("csv.magi");
    let hash = fx.csv("out/hash.csv");
    assert_eq!(hash.rows.len(), 4);
    assert_eq!(
        fx.csv("out/hash_rejects.csv")
            .project(&["row", "column", "value"]),
        lines!["2,account,#N/A", "3,amount,x"]
    );
    assert_eq!(
        fx.csv("out/quotes.csv").column("code"),
        lines!["'A1'", "'B2'"]
    );
}

/// A file that escapes quotes as `\"` is read with them; a quoted field that goes on after its
/// closing quote stops the run and names the line.
#[test]
fn csv_escapes_are_read_and_malformed_quoting_is_reported() {
    let fx = Fixture::new("sources4");
    fx.run_ok("csv.magi");
    assert_eq!(
        fx.csv("out/backslash.csv").column("msg"),
        lines!["say \"hi\", ok", "plain"]
    );
    let out = fx.check("stray.magi");
    out.assert_code(1).assert_diagnostic("M212");
    assert!(
        out.stderr_flat()
            .contains("on line 4, a quoted field goes on after its closing quote"),
        "{}",
        out.stderr
    );
}

/// A value inference would turn into a time or date only by its shape (`27:15`, `2024-02-30`)
/// keeps the column text; a declared date reads ISO timestamps at midnight.
#[test]
fn temporal_values_are_types_only_when_they_convert() {
    let fx = Fixture::new("sources4");
    let out = fx.magi(&["schema", "csv.magi", "temporal"]);
    out.assert_code(0);
    for column in ["hours", "day"] {
        let line = out
            .stdout
            .lines()
            .find(|l| l.trim_start().starts_with(column))
            .unwrap();
        assert!(line.contains("string"), "{line}");
    }
    fx.run_ok("csv.magi");
    assert_eq!(
        fx.csv("out/temporal.csv").project(&["id", "at"]),
        lines!["1,2024-01-02", "2,2024-01-03", "3,"]
    );
    assert_eq!(
        fx.csv("out/temporal_rejects.csv")
            .project(&["row", "value"]),
        lines!["3,2024-01-04 12:00:00"]
    );
}

/// A fixed-width file without line breaks is read in blocks of `record_length` bytes, and
/// refused without it; rejects and fields declared `string` show the field as written.
#[test]
fn fixed_width_blocks_and_raw_values() {
    let fx = Fixture::new("sources4");
    fx.run_ok("fixed.magi");
    assert_eq!(
        fx.csv("out/blocks.csv").project(&["id", "day", "amt"]),
        lines!["1,2024-01-31,001500", "2,,002500", "3,2024-02-02,003500"]
    );
    assert_eq!(
        fx.csv("out/blocks_rejects.csv")
            .project(&["row", "column", "value"]),
        lines!["2,day,20241399"]
    );
    let out = fx.check("fixed_nolen.magi");
    out.assert_code(1).assert_diagnostic("M200");
    assert!(
        out.stderr_flat().contains("record_length"),
        "{}",
        out.stderr
    );
}
