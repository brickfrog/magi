//! End-to-end tests of BI semantic models: `magi compile --target tmdl`
//! on the `model` fixture, checked through the generated TMDL text and the diagnostics.

#[macro_use]
#[allow(dead_code)]
mod support;

use support::Fixture;

/// `magi compile <program> --target tmdl --out bi`, which must succeed.
#[track_caller]
fn compile(fx: &Fixture, program: &str) {
    fx.magi(&["compile", program, "--target", "tmdl", "--out", "bi"])
        .assert_code(0);
}

#[test]
fn compile_writes_a_tmdl_model_over_the_exported_files() {
    let fx = Fixture::new("model");
    fx.run_ok("model.magi");
    let out = fx.magi(&[
        "compile",
        "model.magi",
        "--target",
        "tmdl",
        "--out",
        "bi",
        "--data-folder",
        "C:\\data",
    ]);
    out.assert_code(0);
    let def = "bi/sales_model.SemanticModel/definition";
    assert!(fx.exists("bi/sales_model.SemanticModel/definition.pbism"));
    assert_eq!(
        fx.read(&format!("{def}/model.tmdl"))
            .lines()
            .filter(|l| l.starts_with("ref table"))
            .collect::<Vec<_>>(),
        ["ref table sales", "ref table customers"]
    );
    // the data folder is a parameter; tables read their files relative to it
    assert!(
        fx.read(&format!("{def}/expressions.tmdl"))
            .contains("expression DataFolder = \"C:\\data\\\"")
    );
    let sales = fx.read(&format!("{def}/tables/sales.tmdl"));
    // measures: SQL aggregates with the same meaning in DAX; division is DIVIDE (blank on zero)
    assert!(sales.contains("\t/// Recognised revenue\n\tmeasure revenue = SUM('sales'[amount])\n\t\tformatString: \\$#,0.00;(\\$#,0.00);\\$#,0.00\n"), "{sales}");
    assert!(sales.contains("measure average_price = DIVIDE(SUM('sales'[amount]), SUM('sales'[qty]))\n\t\tformatString: #,0.00\n"), "{sales}");
    assert!(
        sales.contains("measure orders = COUNTA('sales'[sale_id])\n\t\tformatString: #,0\n"),
        "{sales}"
    );
    // the foreign key on the many side is hidden; decimal(12,2) fits Power BI's fixed decimal
    assert!(
        sales.contains("\tcolumn customer_id\n\t\tdataType: int64\n\t\tisHidden\n"),
        "{sales}"
    );
    assert!(
        sales.contains("\tcolumn amount\n\t\tdataType: decimal\n"),
        "{sales}"
    );
    assert!(
        sales.contains("Source = Parquet.Document(File.Contents(DataFolder & \"sales.parquet\"))"),
        "{sales}"
    );
    let customers = fx.read(&format!("{def}/tables/customers.tmdl"));
    assert!(customers.contains("\t/// Sales region\n\tcolumn region_name\n\t\tdataType: string\n\t\tsummarizeBy: none\n\t\tsourceColumn: region\n"), "{customers}");
    assert!(
        customers.contains(
            "Source = Parquet.Document(File.Contents(DataFolder & \"customers.parquet\"))"
        ),
        "{customers}"
    );
    assert_eq!(
        fx.read(&format!("{def}/relationships.tmdl")),
        "relationship 'sales.customer_id to customers.customer_id'\n\tfromColumn: sales.customer_id\n\ttoColumn: customers.customer_id\n\n"
    );
}

#[test]
fn model_tables_must_be_exported_and_metrics_translatable() {
    let fx = Fixture::new("model");
    let out = fx.check("errors.magi");
    out.assert_code(1).assert_diagnostic("M601");
    assert!(
        out.stderr_flat()
            .contains("add `export sales to \"sales.parquet\"`"),
        "{}",
        out.stderr
    );
    let fx = Fixture::new("model");
    std::fs::write(
        fx.file("errors.magi"),
        fx.read("errors.magi").replace(
            "model broken",
            "export sales to \"s.parquet\"\n\nmodel broken",
        ),
    )
    .unwrap();
    let out = fx.check("errors.magi");
    out.assert_code(1).assert_diagnostic("M604");
    assert!(
        out.stderr_flat()
            .contains("`missing` has no equivalent DAX measure"),
        "{}",
        out.stderr
    );
}

/// CSV carries neither types nor nulls, so Power BI would read empty fields as text.
#[test]
fn a_csv_export_does_not_feed_a_model() {
    let fx = Fixture::new("model");
    std::fs::write(
        fx.file("model.magi"),
        fx.read("model.magi")
            .replace("out/customers.parquet", "out/customers.csv"),
    )
    .unwrap();
    let out = fx.check("model.magi");
    out.assert_code(1).assert_diagnostic("M601");
    let err = out.stderr_flat();
    assert!(
        err.contains("model table `customers` is not exported to Parquet"),
        "{}",
        out.stderr
    );
    assert!(
        err.contains("add `export customers to \"customers.parquet\"`"),
        "{}",
        out.stderr
    );
}

/// DAX names the model's columns, so a column renamed by `dimension` is referenced by its new
/// name, even when the dimension is declared after the metric. Counts keep SQL's null handling.
#[test]
fn measures_name_the_model_columns_and_count_like_sql() {
    let fx = Fixture::new("model");
    compile(&fx, "renamed.magi");
    let def = "bi/renamed.SemanticModel/definition";
    let sales = fx.read(&format!("{def}/tables/sales.tmdl"));
    assert!(
        sales.contains("\tmeasure revenue = SUM('sales'[sale_amount])\n"),
        "{sales}"
    );
    assert!(
        sales.contains("\tcolumn sale_amount\n\t\tdataType: decimal\n\t\tsummarizeBy: none\n\t\tsourceColumn: amount\n"),
        "{sales}"
    );
    assert!(!sales.contains("'sales'[amount]"), "{sales}");
    // SQL `count(x)` counts non-null values of any type, booleans included: COUNTA, not COUNT
    assert!(
        sales.contains("\tmeasure large_sales = COUNTA('sales'[large])\n"),
        "{sales}"
    );
    assert!(
        sales.contains(
            "\tmeasure large_share = DIVIDE(COUNTA('sales'[large]), COUNTROWS('sales'))\n"
        ),
        "{sales}"
    );
    // SQL `COUNT(DISTINCT x)` ignores nulls; DISTINCTCOUNT would count BLANK as a value
    let customers = fx.read(&format!("{def}/tables/customers.tmdl"));
    assert!(
        customers.contains("\tmeasure regions = DISTINCTCOUNTNOBLANK('customers'[area])\n"),
        "{customers}"
    );
}

#[test]
fn relationships_have_unique_names_and_one_active_path_per_table_pair() {
    let fx = Fixture::new("model");
    let out = fx.check("relationships.magi");
    out.assert_code(0).assert_diagnostic("M605");
    assert!(
        out.stderr_flat().contains(
            "`CALCULATE(<measure>, USERELATIONSHIP('sales'[bill_to], 'customers'[customer_key]))`"
        ),
        "{}",
        out.stderr
    );
    compile(&fx, "relationships.magi");
    // names use the model's column names (after `dimension`); the second path is inactive
    assert_eq!(
        fx.read("bi/billing.SemanticModel/definition/relationships.tmdl"),
        "relationship 'sales.customer_id to customers.customer_key'\n\
         \tfromColumn: sales.customer_id\n\
         \ttoColumn: customers.customer_key\n\
         \n\
         relationship 'sales.bill_to to customers.customer_key'\n\
         \tisActive: false\n\
         \tfromColumn: sales.bill_to\n\
         \ttoColumn: customers.customer_key\n\
         \n"
    );

    std::fs::write(
        fx.file("relationships.magi"),
        fx.read("relationships.magi").replace(
            "    dimension",
            "    relationship sales.bill_to -> customers.customer_id\n    dimension",
        ),
    )
    .unwrap();
    let out = fx.check("relationships.magi");
    out.assert_code(1).assert_diagnostic("M602");
    assert!(
        out.stderr_flat()
            .contains("this relationship is declared twice"),
        "{}",
        out.stderr
    );
}

/// Power BI needs a measure name that no column or other measure uses, ignoring case.
#[test]
fn metric_names_must_differ_from_columns_and_other_metrics() {
    let fx = Fixture::new("model");
    let out = fx.check("clashes.magi");
    out.assert_code(1);
    assert_eq!(
        out.codes().iter().filter(|c| *c == "M603").count(),
        4,
        "{}",
        out.stderr
    );
    let err = out.stderr_flat();
    for message in [
        "metric `amount` has the same name as column `amount` of table `sales`",
        "metric `Region` has the same name as column `region` of table `customers`",
        "metric `Orders` is defined twice",
        "table `customers` already has a column named `customer_id`",
    ] {
        assert!(err.contains(message), "{message}\n{}", out.stderr);
    }
}
