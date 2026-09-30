#[macro_use]
#[allow(dead_code)]
mod support;

use support::Fixture;

/// A lexer error drops only its own statement: later parse and resolution errors are still
/// reported, and references to the dropped declaration stay silent.
#[test]
fn lexer_error_does_not_hide_later_errors() {
    let fx = Fixture::new("parser2");
    let out = fx.check("l1.magi");
    out.assert_code(1);
    let err = out.stderr_flat();
    // `&&` is one bad token, not two.
    assert_eq!(err.matches("unexpected character `&`").count(), 1, "{err}");
    assert!(err.contains("did you mean `filter`?"), "{err}");
    assert!(err.contains("unknown relation `nothere`"), "{err}");
    assert!(!err.contains("unknown relation `d`"), "{err}");
    assert_eq!(out.codes(), ["M001", "M001", "M002"], "{err}");
}

/// A non-ASCII character outside strings and comments is an unexpected character, not a crash
/// (the lexer looks two bytes ahead for operators such as `|>`).
#[test]
fn non_ascii_character_is_a_lexer_error() {
    let fx = Fixture::new("parser2");
    let out = fx.check("nonascii.magi");
    out.assert_code(1);
    assert_eq!(out.codes(), ["M001"], "{}", out.stderr);
    assert!(out.stderr_flat().contains("unexpected character `€`"));
}

/// A lex error in an imported file does not make the importer's references to that file's
/// declarations unknown.
#[test]
fn lexer_error_in_import_does_not_cascade() {
    let fx = Fixture::new("parser2");
    let out = fx.check("main.magi");
    out.assert_code(1).assert_no_diagnostic("M002");
    assert_eq!(out.codes(), ["M001"], "{}", out.stderr);
    assert!(out.stderr_flat().contains("unterminated string literal"));
}

/// Equally close candidates give the same suggestion every run (the smallest name), even though
/// declarations live in a hash map with per-process ordering.
#[test]
fn did_you_mean_is_deterministic() {
    let fx = Fixture::new("parser2");
    for _ in 0..12 {
        let out = fx.check("dym.magi");
        out.assert_code(1).assert_diagnostic("M002");
        let err = out.stderr_flat();
        assert!(err.contains("did you mean `orders_2021`?"), "{err}");
    }
}
