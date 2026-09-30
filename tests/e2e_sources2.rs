//! End-to-end tests for source reading (cycle-2 audit): validation of sources through their
//! staged rows, CSV shape and type inference over every value, exact temporal and decimal casts,
//! literal file paths, Excel numbers stored as text, staging SQL and ODBC credentials.

#[macro_use]
#[allow(dead_code)]
mod support;

use support::Fixture;

fn fixture() -> Fixture {
    Fixture::new("sources2")
}

fn write(fx: &Fixture, name: &str, text: &str) {
    std::fs::write(fx.file(name), text).unwrap();
}

// ---------------------------------------------------------------------------------------------
// validation of sources

#[test]
fn a_source_column_named_rowid_does_not_hide_failing_rows() {
    let fx = fixture();
    write(&fx, "rid.csv", "rowid,amount\n100,1\n200,-5\n");
    write(
        &fx,
        "rid.magi",
        "source s = csv(\"rid.csv\")\n\
         validate s { require amount > 0 }\n\
         export s to \"out/s.csv\"\n",
    );
    fx.run("rid.magi", &[])
        .assert_code(1)
        .assert_diagnostic("M402");
    assert!(!fx.exists("out/s.csv"));

    // the failure names the data row it is on, whatever the `rowid` column holds
    write(&fx, "rid2.csv", "rowid,amount\n1,-1\n0,5\n");
    write(
        &fx,
        "rid2.magi",
        "source s = csv(\"rid2.csv\")\n\
         validate s { require amount > 0 }\n\
         export s.failures to \"out/f.csv\"\n",
    );
    fx.run("rid2.magi", &["--keep-going"]).assert_code(1);
    assert_eq!(
        fx.csv("out/f.csv")
            .project(&["source_row", "rowid", "amount"]),
        lines!["1,1,-1"]
    );
}

// ---------------------------------------------------------------------------------------------
// CSV shape

#[test]
fn a_ragged_file_without_header_is_m212() {
    let fx = fixture();
    write(&fx, "nh.csv", "1,a,10\n2,b,20,EXTRA\n3,c,30\n");
    write(
        &fx,
        "nh.magi",
        "source s = csv(\"nh.csv\") { header: false }\nexport s to \"out/s.csv\"\n",
    );
    let check = fx.check("nh.magi");
    check.assert_code(1).assert_diagnostic("M212");
    assert!(check.stderr_flat().contains("line 2"), "{}", check.stderr);
    assert!(!check.stderr.contains("EXTRA"), "{}", check.stderr);
    // with a header, the help does not suggest `header: false`, which reads the same file
    write(&fx, "hd.csv", "id,a,b\n1,2,3\n4,5,6,7\n");
    write(
        &fx,
        "hd.magi",
        "source s = csv(\"hd.csv\")\nexport s to \"out/s.csv\"\n",
    );
    let check = fx.check("hd.magi");
    check.assert_code(1).assert_diagnostic("M212");
    assert!(!check.stderr.contains("header: false"), "{}", check.stderr);
}

#[test]
fn a_trailing_extra_field_is_m212_with_or_without_a_declared_delimiter() {
    let fx = fixture();
    write(
        &fx,
        "trail.csv",
        "id,desc,amount\n1,a,10\n2,b,20,\n3,c,30\n",
    );
    for options in ["", "{ delimiter: \",\" }"] {
        write(
            &fx,
            "trail.magi",
            &format!("source s = csv(\"trail.csv\") {options}\nexport s to \"out/s.csv\"\n"),
        );
        let check = fx.check("trail.magi");
        check.assert_code(1).assert_diagnostic("M212");
        assert!(check.stderr_flat().contains("line 3"), "{}", check.stderr);
    }
}

#[test]
fn an_empty_csv_says_it_is_empty() {
    let fx = fixture();
    write(&fx, "empty.csv", "");
    write(
        &fx,
        "empty.magi",
        "source s = csv(\"empty.csv\")\nexport s to \"out/s.csv\"\n",
    );
    let check = fx.check("empty.magi");
    check.assert_code(1).assert_diagnostic("M200");
    assert!(
        check.stderr_flat().contains("`empty.csv` is empty"),
        "{}",
        check.stderr
    );
}

// ---------------------------------------------------------------------------------------------
// file paths

#[test]
fn file_paths_are_literal_not_glob_patterns() {
    let fx = fixture();
    write(&fx, "q1.csv", "id,v\n1,plain\n");
    write(&fx, "q[1].csv", "id,v\n9,bracket\n");
    write(&fx, "a*b.csv", "id,v\n5,star\n");
    write(&fx, "a?b.csv", "id,v\n6,question\n");
    write(
        &fx,
        "glob.magi",
        "source s = csv(\"q[1].csv\")\n\
         source t = csv(\"a*b.csv\")\n\
         export s to \"out/s.csv\"\n\
         export t to \"out/t.csv\"\n",
    );
    fx.run_ok("glob.magi");
    assert_eq!(fx.read("out/s.csv"), "id,v\n9,bracket\n");
    assert_eq!(fx.read("out/t.csv"), "id,v\n5,star\n");
}

// ---------------------------------------------------------------------------------------------
// CSV types from every value

#[test]
fn csv_bool_and_blank_sampled_columns_are_typed_from_every_value() {
    let fx = fixture();
    // past DuckDB's sniffing sample: an odd flag, and numbers after a long run of blanks
    let mut text = String::from("id,flag,x\n");
    for i in 1..=30_000 {
        let flag = if i == 29_000 {
            "maybe"
        } else if i % 2 == 0 {
            "true"
        } else {
            "false"
        };
        let x = if i < 25_000 {
            String::new()
        } else {
            i.to_string()
        };
        text += &format!("{i},{flag},{x}\n");
    }
    write(&fx, "late.csv", &text);
    write(
        &fx,
        "late.magi",
        "source s = csv(\"late.csv\")\nexport s to \"out/s.csv\"\n",
    );
    let schema = fx.magi(&["schema", "late.magi", "s"]);
    schema.assert_code(0);
    let flat = support::flatten(&schema.stdout);
    assert!(flat.contains("flag string?"), "{}", schema.stdout);
    assert!(flat.contains("x int?"), "{}", schema.stdout);
    let check = fx.check("late.magi");
    check.assert_diagnostic("M201");
    assert!(
        check
            .stderr_flat()
            .contains("column flag: 1 value is not booleans"),
        "{}",
        check.stderr
    );
    assert!(!check.stderr.contains("maybe"), "{}", check.stderr);
}

// ---------------------------------------------------------------------------------------------
// exact casts

#[test]
fn decimal_38_rounding_is_counted_for_every_rounded_value() {
    let fx = fixture();
    write(&fx, "r38.csv", "id,amt\n1,1.0004\n2,1.0051\n3,2.50\n");
    write(
        &fx,
        "r38.magi",
        "source s = csv(\"r38.csv\") { schema { id: int amt: decimal(38,2)? } }\n\
         export s to \"out/s.csv\"\n",
    );
    let out = fx.run_ok("r38.magi");
    out.assert_diagnostic("M209");
    assert!(
        out.stderr_flat().contains("2 value(s) in column `amt`"),
        "{}",
        out.stderr
    );
}

#[test]
fn a_declared_decimal_too_narrow_for_the_values_is_m203_at_check() {
    let fx = fixture();
    write(&fx, "prec.csv", "id,amt\n1,123456.7\n2,1.5\n");
    write(
        &fx,
        "prec.magi",
        "source s = csv(\"prec.csv\") { schema { id: int amt: decimal(5,2)? } }\n\
         export s to \"out/s.csv\"\n",
    );
    let check = fx.check("prec.magi");
    check.assert_code(0).assert_diagnostic("M203");
    assert!(
        check
            .stderr_flat()
            .contains("the source reports `decimal(7,1)`"),
        "{}",
        check.stderr
    );
    // a wide enough declaration is quiet
    write(
        &fx,
        "wide.magi",
        "source s = csv(\"prec.csv\") { schema { id: int amt: decimal(8,2)? } }\n\
         export s to \"out/s.csv\"\n",
    );
    fx.check("wide.magi")
        .assert_code(0)
        .assert_no_diagnostic("M203");
}

#[test]
fn a_date_format_with_a_time_rejects_times_other_than_midnight() {
    let fx = fixture();
    write(
        &fx,
        "df.csv",
        "id,d\n1,05/01/2026 10:30\n2,06/01/2026 00:00\n",
    );
    write(
        &fx,
        "df.magi",
        "source s = csv(\"df.csv\") { schema { id: int d: date(\"%d/%m/%Y %H:%M\")? } }\n\
         export s to \"out/s.csv\"\n\
         export s.rejects to \"out/r.csv\"\n",
    );
    fx.run_ok("df.magi").assert_diagnostic("M208");
    assert_eq!(fx.read("out/s.csv"), "id,d\n1,\n2,2026-01-06\n");
    assert_eq!(
        fx.csv("out/r.csv").project(&["row", "column"]),
        lines!["1,d"]
    );
}

#[test]
fn a_zone_format_needs_timestamp_tz() {
    let fx = fixture();
    write(&fx, "z.csv", "id,ts\n1,05/01/2026 10:00 +0200\n");
    write(
        &fx,
        "z.magi",
        "source s = csv(\"z.csv\") { schema { id: int ts: timestamp(\"%d/%m/%Y %H:%M %z\") } }\n\
         export s to \"out/s.csv\"\n",
    );
    let check = fx.check("z.magi");
    check.assert_code(1).assert_diagnostic("M101");
    assert!(check.stderr.contains("timestamp_tz"), "{}", check.stderr);
    write(
        &fx,
        "ztz.magi",
        "source s = csv(\"z.csv\") { schema { id: int ts: timestamp_tz(\"%d/%m/%Y %H:%M %z\") } }\n\
         export s to \"out/s.csv\"\n",
    );
    fx.run_ok("ztz.magi");
    assert_eq!(fx.read("out/s.csv"), "id,ts\n1,2026-01-05 08:00:00+00\n");
}

#[test]
fn timestamps_never_drop_an_offset_or_cut_sub_microseconds() {
    let fx = fixture();
    write(
        &fx,
        "us.csv",
        "id,t,tz\n\
         1,2026-01-05 10:00:00.1234567,2026-01-05 10:00:00+02:00\n\
         2,2026-01-05 10:00:00.1234560,2026-01-05 10:00:00\n",
    );
    write(
        &fx,
        "us.magi",
        "source s = csv(\"us.csv\") { schema { id: int t: timestamp? tz: timestamp? } }\n\
         export s to \"out/s.csv\"\n\
         export s.rejects to \"out/r.csv\"\n",
    );
    fx.run_ok("us.magi").assert_diagnostic("M208");
    assert_eq!(
        fx.read("out/s.csv"),
        "id,t,tz\n1,,\n2,2026-01-05 10:00:00.123456,2026-01-05 10:00:00\n"
    );
    assert_eq!(
        fx.csv("out/r.csv").project(&["row", "column"]),
        lines!["1,t", "1,tz"]
    );
    // undeclared, sub-microsecond digits keep their text
    write(
        &fx,
        "us2.magi",
        "source s = csv(\"us.csv\")\nexport s to \"out/s2.csv\"\n",
    );
    let check = fx.check("us2.magi");
    check.assert_diagnostic("M201");
    assert!(
        check
            .stderr_flat()
            .contains("more than 6 decimal places of seconds"),
        "{}",
        check.stderr
    );
    fx.run_ok("us2.magi");
    assert_eq!(
        fx.csv("out/s2.csv").column("t"),
        lines!["2026-01-05 10:00:00.1234567", "2026-01-05 10:00:00.1234560"]
    );
}

#[test]
fn a_bad_format_specifier_is_a_check_error() {
    let fx = fixture();
    write(&fx, "d.csv", "id,d\n1,2026-01-05\n");
    write(
        &fx,
        "bad.magi",
        "source s = csv(\"d.csv\") { schema { id: int d: date(\"%Q\") } }\n\
         export s to \"out/s.csv\"\n",
    );
    let check = fx.check("bad.magi");
    check.assert_code(1).assert_diagnostic("M101");
    assert!(check.stderr.contains("%Q"), "{}", check.stderr);
}

// ---------------------------------------------------------------------------------------------
// database sources

#[test]
fn a_query_ending_in_a_semicolon_runs() {
    let fx = fixture();
    write(&fx, "orders.csv", "id,amount\n1,10.50\n2,3.25\n");
    write(
        &fx,
        "make_db.magi",
        "source orders = csv(\"orders.csv\") { identity id }\n\
         export to \"w.duckdb\" { table \"orders\" = orders }\n",
    );
    fx.run_ok("make_db.magi");
    write(
        &fx,
        "semi.magi",
        "source s = duckdb(\"w.duckdb\") { query: \"\"\"SELECT id, amount FROM orders ORDER BY id;\n\"\"\" identity id }\n\
         export s to \"out/s.csv\"\n",
    );
    fx.check("semi.magi").assert_code(0);
    fx.run_ok("semi.magi");
    assert_eq!(fx.read("out/s.csv"), "id,amount\n1,10.50\n2,3.25\n");
}

#[test]
fn staging_sql_shows_the_casts_run_applies_to_parquet() {
    let fx = fixture();
    write(
        &fx,
        "pq.magi",
        "source p = parquet(\"dbl.parquet\") { schema { id: int x: decimal(18,2) } }\n\
         export p to \"out/p.csv\"\n",
    );
    let sql = fx.magi(&["sql", "pq.magi", "p"]);
    sql.assert_code(0);
    // the file holds a double: `run` casts it, so the printed SQL must too
    assert!(
        sql.stdout.contains("TRY_CAST(\"x\" AS DECIMAL(18,2))"),
        "{}",
        sql.stdout
    );
    fx.run_ok("pq.magi").assert_diagnostic("M209");
    assert_eq!(fx.csv("out/p.csv").column("x"), lines!["1.23"]);
}

#[test]
fn source_columns_differing_only_in_case_are_m003() {
    let fx = fixture();
    write(
        &fx,
        "case.magi",
        "connection w = odbc { dsn: \"nowhere\" }\n\
         source s = sql(w) { query: \"SELECT 1\" schema { Id: int id: int } }\n\
         export s to \"out/s.csv\"\n",
    );
    let check = fx.check("case.magi");
    check.assert_code(1).assert_diagnostic("M003");
    assert!(
        check.stderr_flat().contains("differ only in case"),
        "{}",
        check.stderr
    );
}

// ---------------------------------------------------------------------------------------------
// Excel numbers stored as text

#[test]
fn excel_columns_of_numbers_stored_as_text_keep_their_text() {
    let fx = fixture();
    let mut wb = rust_xlsxwriter::Workbook::new();
    let ws = wb.add_worksheet();
    ws.set_name("Data").unwrap();
    for (c, h) in ["id", "tn", "code"].iter().enumerate() {
        ws.write_string(0, c as u16, *h).unwrap();
    }
    for (r, (tn, code)) in [("1.50", "00123"), ("42", "007"), ("0.125", "1.50")]
        .iter()
        .enumerate()
    {
        let r = r as u32 + 1;
        ws.write_number(r, 0, f64::from(r)).unwrap();
        ws.write_string(r, 1, *tn).unwrap();
        ws.write_string(r, 2, *code).unwrap();
    }
    wb.save(fx.file("t.xlsx")).unwrap();
    write(
        &fx,
        "t.magi",
        "source s = excel(\"t.xlsx\") { sheet: \"Data\" }\nexport s to \"out/s.csv\"\n",
    );
    let check = fx.check("t.magi");
    check.assert_code(0);
    let flat = check.stderr_flat();
    assert!(
        flat.contains("column tn: 3 numbers stored as text")
            && flat.contains("declare `tn: decimal(18,3)`"),
        "{}",
        check.stderr
    );
    // a column of text cells is never called numeric
    assert!(!flat.contains("column code"), "{}", check.stderr);
    fx.run_ok("t.magi");
    assert_eq!(
        fx.csv("out/s.csv").column("tn"),
        lines!["1.50", "42", "0.125"]
    );
    assert_eq!(
        fx.csv("out/s.csv").column("code"),
        lines!["00123", "007", "1.50"]
    );
    // declared numeric, the same cells are numbers without a type conflict
    write(
        &fx,
        "td.magi",
        "source s = excel(\"t.xlsx\") { sheet: \"Data\" schema { id: int tn: decimal(10,3) code: string } }\n\
         export s to \"out/d.csv\"\n",
    );
    fx.check("td.magi")
        .assert_code(0)
        .assert_no_diagnostic("M203");
    fx.run_ok("td.magi");
    assert_eq!(
        fx.csv("out/d.csv").column("tn"),
        lines!["1.500", "42.000", "0.125"]
    );
}

// ---------------------------------------------------------------------------------------------
// ODBC credentials

#[test]
fn odbc_connection_string_credentials_that_could_add_attributes_are_refused() {
    let fx = fixture();
    write(
        &fx,
        "cs.magi",
        "connection w = odbc { connection_string: env(\"MAGI_CS\") user: \"ana\" password: env(\"MAGI_PW\") }\n\
         source s = sql(w) { query: \"SELECT 1 AS v\" }\n\
         export s to \"out/s.csv\"\n",
    );
    let secret = "x};Database=/tmp/other.db;{";
    let out = fx.magi_env(
        &["check", "--sources", "cs.magi"],
        &[
            (
                "MAGI_CS",
                "Driver=/nonexistent/libnothing.so;Database=/tmp/x.db",
            ),
            ("MAGI_PW", secret),
        ],
    );
    out.assert_code(1).assert_diagnostic("M200");
    assert!(
        out.stderr_flat().contains("the `password` value contains"),
        "{}",
        out.stderr
    );
    assert!(!out.stderr.contains(secret), "{}", out.stderr);
}

#[test]
#[ignore = "needs MAGI_TEST_SQLITE_ODBC_DRIVER"]
fn odbc_dsn_credentials_cannot_switch_the_database() {
    let driver = std::env::var("MAGI_TEST_SQLITE_ODBC_DRIVER").expect(
        "MAGI_TEST_SQLITE_ODBC_DRIVER must name the SQLite ODBC driver (scripts/build-sqlite-odbc.sh)",
    );
    let fx = fixture();
    for (db, value) in [("prod.db", "PROD"), ("other.db", "OTHER")] {
        let status = std::process::Command::new("python3")
            .arg("-c")
            .arg(
                "import sqlite3, sys\n\
                 con = sqlite3.connect(sys.argv[1])\n\
                 con.execute('CREATE TABLE t(v TEXT)')\n\
                 con.execute('INSERT INTO t VALUES (?)', (sys.argv[2],))\n\
                 con.commit()\n",
            )
            .arg(fx.file(db))
            .arg(value)
            .status()
            .expect("python3 is needed to create the SQLite databases");
        assert!(status.success());
    }
    let ini = fx.file("odbc.ini");
    write(
        &fx,
        "odbc.ini",
        &format!(
            "[prod]\nDriver={driver}\nDatabase={}\n",
            fx.file("prod.db").display()
        ),
    );
    write(
        &fx,
        "inj.magi",
        "connection w = odbc { dsn: \"prod\" user: env(\"MAGI_U\") password: env(\"MAGI_P\") }\n\
         source s = sql(w) { query: \"\"\"SELECT v FROM t\"\"\" schema { v: string } }\n\
         export s to \"out/s.csv\"\n",
    );
    let other = fx.file("other.db").display().to_string();
    for password in [
        "plain".to_string(),
        format!("x}};Database={other};{{"),
        format!("x;Database={other}"),
    ] {
        let out = fx.magi_env(
            &["run", "--quiet", "inj.magi"],
            &[
                ("ODBCINI", ini.to_str().unwrap()),
                ("MAGI_U", "bob"),
                ("MAGI_P", &password),
            ],
        );
        out.assert_code(0);
        assert_eq!(fx.read("out/s.csv"), "v\nPROD\n", "password {password}");
    }
    // no file was created from the password's text
    assert!(!fx.exists("other.db}"));
}
