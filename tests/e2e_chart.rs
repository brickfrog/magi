//! End-to-end tests of `chart` declarations exported to .svg: the four chart kinds, rows left
//! out for null values (M509), byte-identical reruns, and the declaration and data errors.

#[macro_use]
#[allow(dead_code)]
mod support;

use support::Fixture;

const CHARTS: [&str; 4] = [
    "out/defects.svg",
    "out/trend.svg",
    "out/share.svg",
    "out/movement.svg",
];

/// The source of `chart.magi`, for programs written by the tests.
const SOURCE: &str = r#"source lines = csv("lines.csv") {
    schema {
        day: date?
        kind: string
        n: int
        amount: decimal(10, 2)?
    }
}
"#;

/// Trimmed contents of every `<text>` element.
fn texts(svg: &str) -> Vec<String> {
    svg.split("<text")
        .skip(1)
        .filter_map(|t| {
            let body = t.split_once('>')?.1;
            Some(body.split_once("</text>")?.0.trim().to_string())
        })
        .collect()
}

#[test]
fn charts_are_written_and_reproducible() {
    let f = Fixture::new("chart");
    let out = f.run_ok("chart.magi");
    for c in CHARTS {
        assert!(f.read(c).starts_with("<svg"), "{c}: {}", f.read(c));
    }
    let defects = f.read("out/defects.svg");
    for s in ["Defects by month", "2026-01-05", "glitch", "no_rate"] {
        assert!(defects.contains(s), "defects.svg lacks {s}");
    }
    let share = f.read("out/share.svg");
    assert!(share.contains("glitch") && share.contains("no_rate"));
    // a time axis: ticks are `MM-DD`, not the categories `2026-01-05`, ...
    assert!(
        texts(&f.read("out/trend.svg")).iter().any(|t| {
            let b = t.as_bytes();
            b.len() == 5 && b[2] == b'-' && [0, 1, 3, 4].iter().all(|&i| b[i].is_ascii_digit())
        }),
        "{:?}",
        texts(&f.read("out/trend.svg"))
    );
    // defects and trend leave out the row without a day, movement the row without an amount
    assert_eq!(
        out.codes().iter().filter(|c| *c == "M509").count(),
        3,
        "{}",
        out.stderr
    );
    assert!(
        out.stderr_flat().contains("left out 1 row "),
        "{}",
        out.stderr
    );

    let first: Vec<Vec<u8>> = CHARTS.iter().map(|c| f.bytes(c)).collect();
    f.run_ok("chart.magi");
    for (c, bytes) in CHARTS.iter().zip(first) {
        assert_eq!(f.bytes(c), bytes, "{c} changed between runs");
    }
}

#[test]
fn chart_errors() {
    let f = Fixture::new("chart");
    let cases: &[(&str, &str, &[&str])] = &[
        (
            "relation_to_svg",
            r#"export lines to "out/x.svg""#,
            &["M507"],
        ),
        (
            "chart_to_csv",
            "chart defects = lines { kind: bar  x: day  y: n }\nexport defects to \"out/x.csv\"",
            &["M507"],
        ),
        ("no_kind", "chart c = lines { x: day  y: n }", &["M508"]),
        (
            "bad_kind",
            "chart c = lines { kind: bars  x: day  y: n }",
            &["M508", "did you mean `bar`?"],
        ),
        (
            "y_not_number",
            "chart c = lines { kind: bar  x: day  y: kind }",
            &["M508"],
        ),
        (
            "pie_series",
            "chart c = lines { kind: pie  x: kind  y: n  series: day }",
            &["M508", "only for bar and line"],
        ),
        (
            "same_name",
            "chart lines = lines { kind: bar  x: kind  y: n }",
            &["M003"],
        ),
        (
            "unknown_option",
            "chart c = lines { kind: bar  x: day  y: n  colour: \"red\" }",
            &["M007"],
        ),
    ];
    for (name, body, expected) in cases {
        let file = format!("{name}.magi");
        std::fs::write(f.file(&file), format!("{SOURCE}\n{body}\n")).unwrap();
        let out = f.check(&file);
        out.assert_code(1);
        for e in *expected {
            if e.starts_with('M') && e.len() == 4 {
                out.assert_diagnostic(e);
            } else {
                assert!(out.stderr_flat().contains(e), "{name}: {}", out.stderr);
            }
        }
    }
}

#[test]
fn chart_data_errors() {
    let f = Fixture::new("chart");
    let cases = [
        (
            "negative_slice",
            "dataset net = lines |> group by kind |> aggregate { amount = sum(amount) }\n\
             chart neg = net { kind: pie  x: kind  y: amount }\n\
             export neg to \"out/neg.svg\"",
            "is negative",
        ),
        (
            "duplicate_x",
            "chart dup = lines { kind: bar  x: kind  y: n }\nexport dup to \"out/dup.svg\"",
            "two rows for `glitch`",
        ),
    ];
    for (name, body, message) in cases {
        let file = format!("{name}.magi");
        std::fs::write(f.file(&file), format!("{SOURCE}\n{body}\n")).unwrap();
        let out = f.run(&file, &[]);
        out.assert_code(1).assert_diagnostic("M505");
        assert!(
            out.stderr_flat().contains(message),
            "{name}: {}",
            out.stderr
        );
    }
}
