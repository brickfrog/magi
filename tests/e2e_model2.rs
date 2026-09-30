//! End-to-end tests of BI semantic models, second round: stale files, column types, measure
//! formats and filter paths as Power BI reads them.

#[macro_use]
#[allow(dead_code)]
mod support;

use support::Fixture;

/// `magi compile <program> --target tmdl --out bi`, which must succeed.
#[track_caller]
fn compile(fx: &Fixture, program: &str) -> support::Output {
    let out = fx.magi(&["compile", program, "--target", "tmdl", "--out", "bi"]);
    out.assert_code(0);
    out
}

/// Replace `from` by `to` in fixture file `program`.
#[track_caller]
fn edit(fx: &Fixture, program: &str, from: &str, to: &str) {
    let text = fx.read(program);
    assert!(text.contains(from), "{program} lacks {from:?}");
    std::fs::write(fx.file(program), text.replace(from, to)).unwrap();
}

/// Power BI loads every file under `definition/`: recompiling must drop the tables and
/// relationships the program no longer declares, and keep files Power BI Desktop added.
#[test]
fn recompiling_a_model_removes_dropped_tables_and_relationships() {
    let fx = Fixture::new("model2");
    compile(&fx, "full.magi");
    let def = "bi/shop.SemanticModel/definition";
    assert!(fx.exists(&format!("{def}/tables/customers.tmdl")));
    assert!(fx.exists(&format!("{def}/relationships.tmdl")));
    // files Power BI Desktop writes next to the definition, and files outside the model folder
    std::fs::write(fx.file("bi/shop.SemanticModel/diagramLayout.json"), "{}").unwrap();
    std::fs::write(fx.file("bi/notes.txt"), "mine").unwrap();

    compile(&fx, "reduced.magi");
    assert!(!fx.exists(&format!("{def}/tables/customers.tmdl")));
    assert!(!fx.exists(&format!("{def}/relationships.tmdl")));
    assert!(
        fx.read(&format!("{def}/tables/sales.tmdl"))
            .contains("measure revenue = SUM('sales'[amount])")
    );
    assert_eq!(
        fx.read(&format!("{def}/model.tmdl"))
            .lines()
            .filter(|l| l.starts_with("ref table"))
            .collect::<Vec<_>>(),
        ["ref table sales"]
    );
    assert_eq!(fx.read("bi/shop.SemanticModel/diagramLayout.json"), "{}");
    assert_eq!(fx.read("bi/notes.txt"), "mine");
    // no scratch folder is left behind
    let mut entries: Vec<String> = std::fs::read_dir(fx.file("bi/shop.SemanticModel"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        ["definition", "definition.pbism", "diagramLayout.json"]
    );
}

/// A column MAGI cannot type would be guessed as text, and DAX `SUM` rejects text.
#[test]
fn a_model_column_of_unknown_type_must_be_cast() {
    let fx = Fixture::new("model2");
    let out = fx.check("types.magi");
    out.assert_code(1).assert_diagnostic("M606");
    let err = out.stderr_flat();
    assert!(
        err.contains("column `root` of model table `sales` has a type MAGI does not know"),
        "{}",
        out.stderr
    );
    assert!(
        err.contains("`derive root = to_float(root)`"),
        "{}",
        out.stderr
    );

    edit(
        &fx,
        "types.magi",
        "root = duckdb.sqrt(amount)",
        "root = to_float(duckdb.sqrt(amount))",
    );
    compile(&fx, "types.magi").assert_no_diagnostic("M606");
    let sales = fx.read("bi/typed.SemanticModel/definition/tables/sales.tmdl");
    assert!(
        sales.contains("\tcolumn root\n\t\tdataType: double\n"),
        "{sales}"
    );
}

/// Power BI's fixed decimal holds 4 decimal places and about 9.2e14; wider decimals are doubles.
/// Date and timestamp measures are formatted as dates, not as numbers.
#[test]
fn decimals_power_bi_cannot_hold_become_double_and_date_measures_look_like_dates() {
    let fx = Fixture::new("model2");
    edit(
        &fx,
        "types.magi",
        "root = duckdb.sqrt(amount)",
        "root = to_float(amount)",
    );
    compile(&fx, "types.magi");
    let sales = fx.read("bi/typed.SemanticModel/definition/tables/sales.tmdl");
    for (column, ty) in [
        ("amount", "decimal"),
        ("fine", "decimal"),
        ("wide", "double"),
        ("precise", "double"),
        ("huge", "double"),
        ("seen", "dateTime"),
    ] {
        assert!(
            sales.contains(&format!("\tcolumn {column}\n\t\tdataType: {ty}\n")),
            "{column} should be {ty}:\n{sales}"
        );
    }
    for (measure, format) in [
        ("first_day = MIN('sales'[day])", "yyyy-mm-dd"),
        ("last_seen = MAX('sales'[seen])", "yyyy-mm-dd hh:nn:ss"),
        ("revenue = SUM('sales'[amount])", "#,0.00"),
        ("sales_count = COUNTA('sales'[sale_id])", "#,0"),
    ] {
        assert!(
            sales.contains(&format!(
                "\tmeasure {measure}\n\t\tformatString: {format}\n"
            )),
            "{measure} should use {format}:\n{sales}"
        );
    }
}

/// DAX `MIN` and `MAX` reject true/false columns.
#[test]
fn min_and_max_over_booleans_are_not_translated() {
    let fx = Fixture::new("model2");
    edit(
        &fx,
        "types.magi",
        "root = duckdb.sqrt(amount)",
        "root = to_float(amount)",
    );
    edit(
        &fx,
        "types.magi",
        "metric revenue",
        "metric any_large { value: max(sales.large) }\n    metric revenue",
    );
    let out = fx.check("types.magi");
    out.assert_code(1).assert_diagnostic("M604");
    assert!(
        out.stderr_flat()
            .contains("metric `any_large` cannot be expressed as a Power BI measure: DAX `MAX` does not accept boolean columns"),
        "{}",
        out.stderr
    );
    // counting them is fine
    edit(&fx, "types.magi", "max(sales.large)", "count(sales.large)");
    fx.check("types.magi").assert_code(0);
}

/// VertiPaq compares text ignoring case: "abc" and "ABC" are one value in Power BI.
#[test]
fn text_comparisons_in_measures_get_a_case_note() {
    let fx = Fixture::new("model2");
    edit(
        &fx,
        "types.magi",
        "root = duckdb.sqrt(amount)",
        "root = to_float(amount)",
    );
    fx.check("types.magi")
        .assert_code(0)
        .assert_no_diagnostic("M607");
    edit(
        &fx,
        "types.magi",
        "metric revenue",
        "metric codes { value: count_distinct(sales.code) }\n    metric ids { value: count_distinct(sales.sale_id) }\n    metric revenue",
    );
    let out = fx.check("types.magi");
    out.assert_code(0).assert_diagnostic("M607");
    let err = out.stderr_flat();
    assert!(
        err.contains("metric `codes` compares text, which Power BI does ignoring case"),
        "{}",
        out.stderr
    );
    assert!(
        !err.contains("metric `ids` compares text"),
        "{}",
        out.stderr
    );
}

/// Two paths between the same tables make a model ambiguous: the later relationship is written
/// inactive. Two fact tables sharing dimensions are not ambiguous.
#[test]
fn a_second_filter_path_between_two_tables_is_inactive() {
    let fx = Fixture::new("model2");
    let out = compile(&fx, "paths.magi");
    out.assert_diagnostic("M605");
    let err = out.stderr_flat();
    assert!(
        err.contains("this relationship adds a second filter path from `regions` to `sales`; it is inactive in Power BI"),
        "{}",
        out.stderr
    );
    assert_eq!(
        out.codes().iter().filter(|c| *c == "M605").count(),
        1,
        "{}",
        out.stderr
    );
    let triangle = fx.read("bi/triangle.SemanticModel/definition/relationships.tmdl");
    assert_eq!(triangle.matches("isActive: false").count(), 1, "{triangle}");
    assert!(
        triangle.contains("relationship 'customers.region to regions.region'\n\tisActive: false\n"),
        "{triangle}"
    );
    let two_facts = fx.read("bi/two_facts.SemanticModel/definition/relationships.tmdl");
    assert!(!two_facts.contains("isActive"), "{two_facts}");
}
