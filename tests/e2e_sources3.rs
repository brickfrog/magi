//! End-to-end tests for source layouts of real exports: CSV sections (preamble, table, trailer),
//! ragged lines, `fill_down`, `row_number`, declared types silencing Excel inference notes, and
//! fixed-width records.

#[macro_use]
#[allow(dead_code)]
mod support;

use support::Fixture;

fn fixture() -> Fixture {
    Fixture::new("sources3")
}

fn write(fx: &Fixture, name: &str, text: &str) {
    std::fs::write(fx.file(name), text).unwrap();
}

fn sorted(mut rows: Vec<String>) -> Vec<String> {
    rows.sort();
    rows
}

// ---------------------------------------------------------------------------------------------
// CSV sections

// statement.csv: lines 1-4 preamble, 6-10 the table (a quoted field spans lines 8-9), 12-14
// trailer.

#[test]
fn a_section_is_read_as_the_whole_csv_with_rows_counted_inside_it() {
    let fx = fixture();
    write(
        &fx,
        "s.magi",
        "source bank = csv(\"statement.csv\") {\n\
             section: 2\n\
             all_text: true\n\
             row_number: row_no\n\
             identity `Bank Reference`\n\
             schema { `Running Balance`: decimal(12, 2)? }\n\
         }\n\
         validate bank { require `Bank Reference` != \"R2\" }\n\
         export bank to \"out/bank.csv\"\n\
         export bank.rejects to \"out/rejects.csv\"\n\
         export bank.failures to \"out/failures.csv\"\n",
    );
    fx.check("s.magi")
        .assert_code(0)
        .assert_no_diagnostic("M201")
        .assert_no_diagnostic("M212");
    fx.run("s.magi", &["--keep-going"]).assert_code(1);
    let bank = fx.csv("out/bank.csv");
    assert_eq!(
        bank.header,
        lines![
            "Posting Date",
            "Description",
            "Amount",
            "Running Balance",
            "Bank Reference",
            "row_no"
        ]
    );
    assert_eq!(
        sorted(bank.project(&["row_no", "Bank Reference", "Description"])),
        lines!["1,R1,DEPOSIT", "2,R2,CHECK 101\nVOID REISSUE", "3,R3,FEE"]
    );
    // row numbers count the section's data rows, not the file's lines (R3 is on line 10)
    assert_eq!(
        fx.csv("out/rejects.csv").project(&["row", "column"]),
        lines!["3,Running Balance"]
    );
    assert_eq!(
        fx.csv("out/failures.csv")
            .project(&["source_row", "Bank Reference"]),
        lines!["2,R2"]
    );
}

#[test]
fn preamble_and_trailer_sections_read_as_ragged_key_value_lines() {
    let fx = fixture();
    write(
        &fx,
        "p.magi",
        "source pre = csv(\"statement.csv\") { section: 1 header: false ragged: true }\n\
         source tr = csv(\"statement.csv\") {\n\
             section: 3 header: false ragged: true\n\
             schema { column_2: int? }\n\
         }\n\
         export pre to \"out/pre.csv\"\n\
         export tr to \"out/tr.csv\"\n\
         export tr.rejects to \"out/tr_rejects.csv\"\n",
    );
    fx.run("p.magi", &[])
        .assert_code(0)
        .assert_diagnostic("M208");
    // columns without a header are named like Excel's
    let pre = fx.csv("out/pre.csv");
    assert_eq!(pre.header, lines!["column_1", "column_2"]);
    assert_eq!(
        sorted(pre.project(&["column_1", "column_2"])),
        lines![
            "Account:,OPERATING ****1234",
            "Demo Bank - Account Activity Export,",
            "Opening balance:,1,000.00",
            "Statement period:,07/01/2026 - 07/31/2026"
        ]
    );
    // the widest trailer line sets the columns; shorter lines are padded with nulls
    let tr = fx.csv("out/tr.csv");
    assert_eq!(tr.header, lines!["column_1", "column_2", "column_3"]);
    assert_eq!(
        sorted(tr.project(&["column_1", "column_2", "column_3"])),
        lines![
            "Closing balance:,,",
            "Total credits:,1,250.00",
            "Total debits:,2,-55.00"
        ]
    );
    // the declared column is the one the rejects name
    assert_eq!(
        fx.csv("out/tr_rejects.csv").project(&["row", "column"]),
        lines!["1,column_2"]
    );
}

#[test]
fn sections_respect_crlf_a_byte_order_mark_and_blank_lines_inside_quotes() {
    let fx = fixture();
    // line 5 is blank but inside the quoted field that starts on line 4
    write(
        &fx,
        "crlf.csv",
        "\u{feff}Title\r\n\r\nid,name\r\n1,\"a\r\n\r\nb\"\r\n2,c\r\n\r\nTotal:,2\r\n",
    );
    write(
        &fx,
        "c.magi",
        "source t = csv(\"crlf.csv\") { section: 2 }\n\
         source title = csv(\"crlf.csv\") { section: 1 header: false }\n\
         source total = csv(\"crlf.csv\") { section: 3 header: false }\n\
         export t to \"out/t.csv\"\n\
         export title to \"out/title.csv\"\n\
         export total to \"out/total.csv\"\n",
    );
    fx.run_ok("c.magi");
    let t = fx.csv("out/t.csv");
    assert_eq!(t.header, lines!["id", "name"]);
    assert_eq!(sorted(t.column("id")), lines!["1", "2"]);
    let multi = &t.rows[t.column("id").iter().position(|i| i == "1").unwrap()][t.col("name")];
    assert_eq!(multi.replace('\r', ""), "a\n\nb");
    // the byte order mark is not part of the first value
    assert_eq!(fx.csv("out/title.csv").column("column_1"), lines!["Title"]);
    assert_eq!(
        fx.csv("out/total.csv").project(&["column_1", "column_2"]),
        lines!["Total:,2"]
    );
}

#[test]
fn a_section_the_file_does_not_have_is_m213_listing_the_sections() {
    let fx = fixture();
    write(
        &fx,
        "m.magi",
        "source s = csv(\"statement.csv\") { section: 4 }\nexport s to \"out/s.csv\"\n",
    );
    let check = fx.check("m.magi");
    check.assert_code(1).assert_diagnostic("M213");
    let flat = check.stderr_flat();
    for part in [
        "3 sections",
        "section 1: lines 1-4",
        "section 2: lines 6-10 (5 fields)",
        "section 3: lines 12-14",
    ] {
        assert!(flat.contains(part), "{part}: {}", check.stderr);
    }
    assert!(!flat.contains("Demo Bank"), "{}", check.stderr);
    write(
        &fx,
        "z.magi",
        "source s = csv(\"statement.csv\") { section: 0 }\nexport s to \"out/s.csv\"\n",
    );
    fx.check("z.magi").assert_code(1).assert_diagnostic("M007");
}

#[test]
fn a_file_of_several_sections_gets_an_m212_that_suggests_section() {
    let fx = fixture();
    write(
        &fx,
        "w.magi",
        "source s = csv(\"statement.csv\")\nexport s to \"out/s.csv\"\n",
    );
    let check = fx.check("w.magi");
    check.assert_code(1).assert_diagnostic("M212");
    let flat = check.stderr_flat();
    assert!(
        flat.contains("section 2: lines 6-10") && flat.contains("`section: 2`"),
        "{}",
        check.stderr
    );
    // the delimiter is not the problem
    assert!(!flat.contains("delimiter:"), "{}", check.stderr);
    assert!(!flat.contains("Demo Bank"), "{}", check.stderr);
}

#[test]
fn magi_sql_says_which_lines_a_section_reads() {
    let fx = fixture();
    write(
        &fx,
        "q.magi",
        "source bank = csv(\"statement.csv\") { section: 2 }\nexport bank to \"out/b.csv\"\n",
    );
    let sql = fx.magi(&["sql", "q.magi", "bank"]);
    sql.assert_code(0);
    assert!(
        sql.stdout
            .contains("section 2 of `statement.csv` (lines 6-10)"),
        "{}",
        sql.stdout
    );
}

// ---------------------------------------------------------------------------------------------
// ragged lines

#[test]
fn ragged_pads_short_lines_but_lines_wider_than_the_header_are_m212() {
    let fx = fixture();
    write(&fx, "r.csv", "a,b,c\n1,2\n3\n4,5,6\n");
    write(
        &fx,
        "r.magi",
        "source s = csv(\"r.csv\") { ragged: true row_number: n }\n\
         dataset d = s |> derive { b_missing = b is null  c_missing = c is null }\n\
         export d to \"out/d.csv\"\n",
    );
    fx.run_ok("r.magi");
    assert_eq!(
        sorted(
            fx.csv("out/d.csv")
                .project(&["n", "a", "b_missing", "c_missing"])
        ),
        lines!["1,1,false,true", "2,3,true,true", "3,4,false,false"]
    );
    // without `ragged`, the same file is not one table
    write(
        &fx,
        "strict.magi",
        "source s = csv(\"r.csv\")\nexport s to \"out/s.csv\"\n",
    );
    fx.check("strict.magi")
        .assert_code(1)
        .assert_diagnostic("M212");
    // a line wider than the header, named by its line in the file (the section starts at line 3)
    write(&fx, "wide.csv", "note\n\na,b\n1\n3,4,5\n6,7\n");
    write(
        &fx,
        "w.magi",
        "source s = csv(\"wide.csv\") { section: 2 ragged: true }\nexport s to \"out/s.csv\"\n",
    );
    let check = fx.check("w.magi");
    check.assert_code(1).assert_diagnostic("M212");
    let flat = check.stderr_flat();
    assert!(
        flat.contains("more fields than line 3") && flat.contains("line 5"),
        "{}",
        check.stderr
    );
}

// ---------------------------------------------------------------------------------------------
// fill_down

#[test]
fn csv_fill_down_fills_blanks_before_typing_and_leaves_leading_blanks_null() {
    let fx = fixture();
    write(
        &fx,
        "f.magi",
        "source gl = csv(\"gl.csv\") {\n\
             fill_down: [Account, Batch]\n\
             schema { Batch: int }\n\
         }\n\
         dataset d = gl |> derive { no_account = Account is null }\n\
         export d to \"out/d.csv\"\n",
    );
    fx.run_ok("f.magi");
    assert_eq!(
        sorted(
            fx.csv("out/d.csv")
                .project(&["Line", "Account", "Batch", "no_account"])
        ),
        lines![
            "L0,,7,true",
            "L1,1010 Cash,7,false",
            "L2,1010 Cash,7,false",
            "L3,1010 Cash,8,false",
            "L4,1020 Savings,8,false",
            "L5,1020 Savings,8,false"
        ]
    );
    // without it, the declared non-null column has missing values
    write(
        &fx,
        "nf.magi",
        "source gl = csv(\"gl.csv\") { schema { Batch: int } }\nexport gl to \"out/gl.csv\"\n",
    );
    fx.run("nf.magi", &[])
        .assert_code(1)
        .assert_diagnostic("M210");
}

#[test]
fn fill_down_of_a_column_the_source_does_not_have_is_m215() {
    let fx = fixture();
    write(
        &fx,
        "u.magi",
        "source gl = csv(\"gl.csv\") { fill_down: Acount }\nexport gl to \"out/gl.csv\"\n",
    );
    let check = fx.check("u.magi");
    check.assert_code(1).assert_diagnostic("M215");
    assert!(
        check.stderr_flat().contains("did you mean `Account`?"),
        "{}",
        check.stderr
    );
}

/// Sheet `GL`: a title, the header on row 3, section rows naming the account, detail rows
/// without it, a blank row inside the range, and a `Check No` column mixing numbers and text.
fn gl_workbook(fx: &Fixture) {
    let mut wb = rust_xlsxwriter::Workbook::new();
    let ws = wb.add_worksheet();
    ws.set_name("GL").unwrap();
    ws.write_string(0, 0, "Report title").unwrap();
    for (c, h) in ["Account", "Line", "Check No", "Amount"].iter().enumerate() {
        ws.write_string(2, c as u16, *h).unwrap();
    }
    // (worksheet row index, cells): "" stays empty; Check No and Amount numbers are number cells
    let rows = [
        (3, ["", "L0", "1", "5"]),
        (4, ["1010 Cash", "", "", ""]),
        (5, ["", "L1", "101", "10"]),
        (6, ["   ", "L2", "EFT", "20"]),
        (8, ["1020 Savings", "", "", ""]),
        (9, ["", "L3", "102", "30"]),
    ];
    for (r, cells) in rows {
        for (c, value) in cells.into_iter().enumerate() {
            match value.parse::<f64>() {
                _ if value.is_empty() => {}
                Ok(n) if c >= 2 => {
                    ws.write_number(r, c as u16, n).unwrap();
                }
                _ => {
                    ws.write_string(r, c as u16, value).unwrap();
                }
            }
        }
    }
    wb.save(fx.file("gl.xlsx")).unwrap();
}

#[test]
fn excel_fill_down_and_row_number_follow_the_data_rows() {
    let fx = fixture();
    gl_workbook(&fx);
    write(
        &fx,
        "x.magi",
        "source gl = excel(\"gl.xlsx\") {\n\
             sheet: \"GL\"\n\
             range: \"A3:D10\"\n\
             fill_down: Account\n\
             row_number: row_no\n\
             schema { `Check No`: string? }\n\
         }\n\
         dataset d = gl |> derive { no_account = Account is null }\n\
         export d to \"out/d.csv\"\n",
    );
    fx.run_ok("x.magi");
    // the blank worksheet row 8 is not a data row: row numbers stay consecutive
    assert_eq!(
        sorted(
            fx.csv("out/d.csv")
                .project(&["row_no", "Account", "Line", "no_account"])
        ),
        lines![
            "1,,L0,true",
            "2,1010 Cash,,false",
            "3,1010 Cash,L1,false",
            "4,1010 Cash,L2,false",
            "5,1020 Savings,,false",
            "6,1020 Savings,L3,false"
        ]
    );
}

#[test]
fn a_declared_type_silences_excel_inference_warnings_for_its_column() {
    let fx = fixture();
    gl_workbook(&fx);
    let program = |schema: &str| {
        format!(
            "source gl = excel(\"gl.xlsx\") {{ sheet: \"GL\" range: \"A3:D10\" {schema} }}\n\
             export gl to \"out/gl.csv\"\n\
             export gl.rejects to \"out/rejects.csv\"\n"
        )
    };
    write(&fx, "plain.magi", &program(""));
    let check = fx.check("plain.magi");
    check.assert_code(0).assert_diagnostic("M201");
    assert!(
        check.stderr_flat().contains("column Check No"),
        "{}",
        check.stderr
    );
    for schema in [
        "schema { `Check No`: string? }",
        "schema { `Check No`: int? }",
    ] {
        write(&fx, "declared.magi", &program(schema));
        let check = fx.check("declared.magi");
        check.assert_code(0);
        assert!(
            !check.stderr_flat().contains("column Check No"),
            "{schema}: {}",
            check.stderr
        );
    }
    // values that do not convert to the declared type are still rejected
    fx.run("declared.magi", &[])
        .assert_code(0)
        .assert_diagnostic("M208");
    assert_eq!(
        fx.csv("out/rejects.csv").project(&["column", "expected"]),
        lines!["Check No,int"]
    );
}

// ---------------------------------------------------------------------------------------------
// row_number

#[test]
fn row_number_needs_a_name_of_its_own_and_a_file_source() {
    let fx = fixture();
    // `line` is the file's `Line` column: names are case-insensitive
    write(
        &fx,
        "clash.magi",
        "source gl = csv(\"gl.csv\") { row_number: line }\nexport gl to \"out/gl.csv\"\n",
    );
    fx.check("clash.magi")
        .assert_code(1)
        .assert_diagnostic("M003");
    write(
        &fx,
        "reserved.magi",
        "source gl = csv(\"gl.csv\") { row_number: __magi_n }\nexport gl to \"out/gl.csv\"\n",
    );
    fx.check("reserved.magi")
        .assert_code(1)
        .assert_diagnostic("M013");
    // a Parquet file has no line order MAGI reads rows in
    write(
        &fx,
        "pq.magi",
        "source gl = csv(\"gl.csv\")\nexport gl to \"gl.parquet\"\n",
    );
    fx.run_ok("pq.magi");
    write(
        &fx,
        "p.magi",
        "source p = parquet(\"gl.parquet\") { row_number: n  fill_down: Account }\n\
         export p to \"out/p.csv\"\n",
    );
    let check = fx.check("p.magi");
    check.assert_code(1);
    assert_eq!(
        check.codes().iter().filter(|c| *c == "M214").count(),
        2,
        "{}",
        check.stderr
    );
}

// ---------------------------------------------------------------------------------------------
// audit fixes: quoting, line breaks, delimiter-only lines, scratch copies, whitespace, messages

#[test]
fn a_quote_inside_a_field_is_text_and_does_not_pull_the_trailer_into_the_table() {
    let fx = fixture();
    write(
        &fx,
        "q.csv",
        "Title line\nAccount:,X\n\nDate,Desc,Amount\n01/01/2026,PIPE 12\" STEEL,5.00\n01/02/2026,OTHER,6.00\n\nClosing:,11.00\n",
    );
    write(
        &fx,
        "q.magi",
        "source s = csv(\"q.csv\") { section: 2 all_text: true ragged: true }\n\
         source t = csv(\"q.csv\") { section: 3 header: false }\n\
         export s to \"out/s.csv\"\n\
         export t to \"out/t.csv\"\n",
    );
    fx.run_ok("q.magi");
    assert_eq!(
        fx.csv("out/s.csv").project(&["Date", "Desc"]),
        lines!["01/01/2026,PIPE 12\" STEEL", "01/02/2026,OTHER"]
    );
    assert_eq!(fx.csv("out/t.csv").column("column_1"), lines!["Closing:"]);
    // the same table as a whole file is one table
    write(
        &fx,
        "plain.csv",
        "Date,Desc,Amount\n01/01/2026,PIPE 12\" STEEL,5.00\n01/02/2026,OTHER,6.00\n",
    );
    write(
        &fx,
        "p.magi",
        "source s = csv(\"plain.csv\") { all_text: true }\nexport s to \"out/p.csv\"\n",
    );
    fx.run_ok("p.magi");
    assert_eq!(fx.csv("out/p.csv").rows.len(), 2);
}

#[test]
fn mixed_line_breaks_are_read_in_a_section_and_explained_in_a_whole_file() {
    let fx = fixture();
    write(&fx, "m.csv", "t\n\nid,v\r\n1,a\n2,b\r\n3,c\n\r\nend\r\n");
    write(
        &fx,
        "m.magi",
        "source s = csv(\"m.csv\") { section: 2 }\nexport s to \"out/m.csv\"\n",
    );
    let run = fx.run("m.magi", &[]);
    run.assert_code(0);
    assert_eq!(
        fx.csv("out/m.csv").project(&["id", "v"]),
        lines!["1,a", "2,b", "3,c"]
    );
    // a whole file DuckDB cannot sniff: the message says why, and never names a scratch file
    write(&fx, "w.csv", "a,b\n1,2\r\n3,4\r\n5,6\n");
    write(
        &fx,
        "w.magi",
        "source s = csv(\"w.csv\")\nexport s to \"out/w.csv\"\n",
    );
    let check = fx.check("w.magi");
    check.assert_code(1).assert_diagnostic("M200");
    let flat = check.stderr_flat();
    assert!(
        flat.contains("mixes line breaks: 2 lines end in CRLF and 2 in LF"),
        "{}",
        check.stderr
    );
    assert!(!flat.contains("__magi"), "{}", check.stderr);
}

#[test]
fn classic_mac_cr_line_breaks_split_sections_and_number_lines() {
    let fx = fixture();
    write(
        &fx,
        "mac.csv",
        "Bank export\rAccount:,X\r\rid,name,amount\r1,a,10\r2,b,20\r3,c\r\rTotal:,30\r",
    );
    write(
        &fx,
        "mac.magi",
        "source s = csv(\"mac.csv\") { section: 2 ragged: true }\n\
         source t = csv(\"mac.csv\") { section: 3 header: false }\n\
         export s to \"out/s.csv\"\n\
         export t to \"out/t.csv\"\n",
    );
    fx.run_ok("mac.magi");
    assert_eq!(
        sorted(fx.csv("out/s.csv").project(&["id", "amount"])),
        lines!["1,10", "2,20", "3,"]
    );
    assert_eq!(
        fx.csv("out/t.csv").project(&["column_1", "column_2"]),
        lines!["Total:,30"]
    );
    // without `ragged`, the short line is named by its line in the file
    write(
        &fx,
        "strict.magi",
        "source s = csv(\"mac.csv\") { section: 2 }\nexport s to \"out/s.csv\"\n",
    );
    let check = fx.check("strict.magi");
    check.assert_code(1).assert_diagnostic("M212");
    assert!(
        check
            .stderr_flat()
            .contains("than line 4 (3 fields): line 7"),
        "{}",
        check.stderr
    );
}

#[test]
fn a_line_of_only_delimiters_separates_sections() {
    let fx = fixture();
    // what a spreadsheet writes for an empty row
    write(
        &fx,
        "c.csv",
        "Report,X\n,,\nid,name,amount\n1,a,10\n2,b,20\n , ,\nTotal:,30\n",
    );
    write(
        &fx,
        "c.magi",
        "source s = csv(\"c.csv\") { section: 2 }\n\
         source t = csv(\"c.csv\") { section: 3 header: false }\n\
         export s to \"out/s.csv\"\n\
         export t to \"out/t.csv\"\n",
    );
    fx.run_ok("c.magi");
    assert_eq!(
        fx.csv("out/s.csv").project(&["id", "amount"]),
        lines!["1,10", "2,20"]
    );
    assert_eq!(
        fx.csv("out/t.csv").project(&["column_1", "column_2"]),
        lines!["Total:,30"]
    );
}

#[test]
fn section_copies_are_removed_and_only_stale_magi_scratch_is_swept() {
    let fx = fixture();
    let tmp = fx.file("tmp");
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    let plant = |name: &str, files: &[&str]| {
        let d = tmp.join(name);
        std::fs::create_dir_all(&d).unwrap();
        for f in files {
            std::fs::write(d.join(f), "a\n").unwrap();
        }
        std::fs::File::open(&d).unwrap().set_modified(old).unwrap();
        d
    };
    // left behind by a MAGI process that was killed: its lock is free
    let stale = plant(
        "__magi_scratch-Ab12Cd34Ef56",
        &["magi.lock", "0-section-2.csv"],
    );
    // a running MAGI process holds its lock
    let live = plant(
        "__magi_scratch-Zz99Yy88Xx77",
        &["magi.lock", "0-section-2.csv"],
    );
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .open(live.join("magi.lock"))
        .unwrap();
    lock.lock().unwrap();
    // not MAGI's: a name MAGI never makes, and a directory holding other files
    let other = plant("__magi_scratch-999998-notmagi", &["magi.lock", "data.csv"]);
    let foreign = plant("__magi_scratch-Qq11Ww22Ee33", &["magi.lock", "notes.txt"]);
    write(
        &fx,
        "s.magi",
        "source a = csv(\"statement.csv\") { section: 2 }\n\
         source b = csv(\"statement.csv\") { section: 2 all_text: true }\n\
         export a to \"out/a.csv\"\n\
         export b to \"out/b.csv\"\n",
    );
    let tmp_text = tmp.display().to_string();
    fx.magi_env(
        &["run", "--quiet", "s.magi"],
        &[("TMPDIR", tmp_text.as_str())],
    )
    .assert_code(0);
    assert_eq!(fx.csv("out/a.csv").rows.len(), 3);
    drop(lock);
    let mut left: Vec<_> = std::fs::read_dir(&tmp)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    left.sort();
    let mut kept = vec![live, other, foreign];
    kept.sort();
    assert!(!stale.exists());
    assert_eq!(left, kept);
}

#[test]
fn csv_blanks_are_unicode_whitespace_as_for_excel() {
    let fx = fixture();
    // tab-only, no-break-space-only and space-only cells are blank
    write(
        &fx,
        "ws.csv",
        "region,amount\nNorth,1\n\t,\u{a0}\n\u{a0},3\n\" \",4\n",
    );
    write(
        &fx,
        "ws.magi",
        "source s = csv(\"ws.csv\") { fill_down: region row_number: n }\n\
         export s to \"out/ws.csv\"\n\
         export s.rejects to \"out/rejects.csv\"\n",
    );
    let check = fx.check("ws.magi");
    check.assert_code(0).assert_no_diagnostic("M201");
    fx.run_ok("ws.magi");
    assert_eq!(
        sorted(fx.csv("out/ws.csv").project(&["n", "region", "amount"])),
        lines!["1,North,1", "2,North,", "3,North,3", "4,North,4"]
    );
    assert!(fx.csv("out/rejects.csv").rows.is_empty());
}

#[test]
fn m212_for_a_file_of_sections_leads_with_the_sections() {
    let fx = fixture();
    write(&fx, "one.csv", "Name,Value\nx,1\ny,2\n\nsummary\n");
    write(
        &fx,
        "h.magi",
        "source s = csv(\"one.csv\")\nexport s to \"out/s.csv\"\n",
    );
    let check = fx.check("h.magi");
    check.assert_code(1).assert_diagnostic("M212");
    let flat = check.stderr_flat();
    assert!(
        flat.contains("blank lines split it into 2 sections") && flat.contains("`section: 1`"),
        "{}",
        check.stderr
    );
    assert!(!flat.contains("no delimiter"), "{}", check.stderr);
}

#[test]
fn a_source_error_does_not_make_its_mapping_unused() {
    let fx = fixture();
    let program = |file: &str| {
        format!(
            "mapping m {{ \"a\" => \"A\"  otherwise => original }}\n\
             source s = csv(\"{file}\")\n\
             dataset d = s |> derive {{ k = map(Account, m) }}\n\
             export d to \"out/d.csv\"\n"
        )
    };
    write(&fx, "bad.magi", &program("missing.csv"));
    fx.check("bad.magi")
        .assert_code(1)
        .assert_diagnostic("M200")
        .assert_no_diagnostic("M005");
    // an unused mapping is still reported
    write(
        &fx,
        "unused.magi",
        "mapping m { \"a\" => \"A\" }\nsource s = csv(\"gl.csv\")\nexport s to \"out/s.csv\"\n",
    );
    fx.check("unused.magi")
        .assert_code(0)
        .assert_diagnostic("M005");
}

// ---------------------------------------------------------------------------------------------
// Excel sections

/// Sheet `GL`: title rows 1-2, blank row 3, the table in rows 4-9 (header, account section rows,
/// detail rows), row 10 holding only spaces, and a `Report Total` row 11.
fn report_workbook(fx: &Fixture) {
    let mut wb = rust_xlsxwriter::Workbook::new();
    let ws = wb.add_worksheet();
    ws.set_name("GL").unwrap();
    let rows: [(u32, [&str; 3]); 9] = [
        (0, ["Harborview", "", ""]),
        (1, ["GL Detail", "", ""]),
        (3, ["Account", "Line", "Amount"]),
        (4, ["1010 Cash", "", ""]),
        (5, ["", "L1", "10.5"]),
        (6, ["", "L2", "20"]),
        (7, ["1020 Savings", "", ""]),
        (8, ["", "L3", "30"]),
        (10, ["Report Total", "", "60.5"]),
    ];
    for (r, cells) in rows {
        for (c, value) in cells.into_iter().enumerate() {
            match value.parse::<f64>() {
                _ if value.is_empty() => {}
                Ok(n) => {
                    ws.write_number(r, c as u16, n).unwrap();
                }
                Err(_) => {
                    ws.write_string(r, c as u16, value).unwrap();
                }
            }
        }
    }
    ws.write_string(9, 1, "   ").unwrap();
    wb.save(fx.file("report.xlsx")).unwrap();
}

#[test]
fn an_excel_section_reads_the_table_between_blank_rows() {
    let fx = fixture();
    report_workbook(&fx);
    let program = |place: &str| {
        format!(
            "source gl = excel(\"report.xlsx\") {{ sheet: \"GL\" {place} fill_down: Account row_number: n }}\n\
             export gl to \"out/gl.csv\"\n"
        )
    };
    write(&fx, "s.magi", &program("section: 2"));
    let check = fx.check("s.magi");
    check.assert_code(0);
    assert!(check.codes().is_empty(), "{}", check.stderr);
    fx.run_ok("s.magi");
    let columns = ["n", "Account", "Line", "Amount"];
    let section = sorted(fx.csv("out/gl.csv").project(&columns));
    assert_eq!(
        section,
        lines![
            "1,1010 Cash,,",
            "2,1010 Cash,L1,10.5",
            "3,1010 Cash,L2,20.0",
            "4,1020 Savings,,",
            "5,1020 Savings,L3,30.0"
        ]
    );
    // the same rows as the explicit range
    write(&fx, "r.magi", &program("range: \"A4:C9\""));
    fx.run_ok("r.magi");
    assert_eq!(sorted(fx.csv("out/gl.csv").project(&columns)), section);
}

#[test]
fn an_excel_section_the_sheet_does_not_have_is_m213_and_conflicts_with_range() {
    let fx = fixture();
    report_workbook(&fx);
    write(
        &fx,
        "m.magi",
        "source gl = excel(\"report.xlsx\") { sheet: \"GL\" section: 4 }\nexport gl to \"out/gl.csv\"\n",
    );
    let check = fx.check("m.magi");
    check.assert_code(1).assert_diagnostic("M213");
    let flat = check.stderr_flat();
    assert!(
        flat.contains("3 sections")
            && flat.contains("section 1: rows 1-2")
            && flat.contains("section 2: rows 4-9 (A4:C9)")
            && flat.contains("section 3: row 11"),
        "{}",
        check.stderr
    );
    for other in ["range: \"A4:C9\"", "header_row: 4"] {
        write(
            &fx,
            "c.magi",
            &format!(
                "source gl = excel(\"report.xlsx\") {{ sheet: \"GL\" section: 2 {other} }}\nexport gl to \"out/gl.csv\"\n"
            ),
        );
        fx.check("c.magi").assert_code(1).assert_diagnostic("M007");
    }
}

// ---------------------------------------------------------------------------------------------
// second audit: shapes, continuation, delimiters, escapes, Excel blanks, blank strings

#[test]
fn m212_names_the_misshapen_line_of_a_table_split_by_an_empty_row() {
    let fx = fixture();
    // one table: an emptied record (`,,`) splits it, and line 7 has an extra field
    write(
        &fx,
        "t.csv",
        "id,name,amt\n1,a,1\n2,b,2\n,,\n3,c,3\n4,d,4\n5,e,5,EXTRA\n6,f,6\n",
    );
    write(
        &fx,
        "t.magi",
        "source s = csv(\"t.csv\")\nexport s to \"out/s.csv\"\n",
    );
    let check = fx.check("t.magi");
    check.assert_code(1).assert_diagnostic("M212");
    let flat = check.stderr_flat();
    assert!(flat.contains("line 7"), "{}", check.stderr);
    assert!(!flat.contains("section:"), "{}", check.stderr);
}

#[test]
fn a_section_warns_when_the_next_section_has_the_tables_width() {
    let fx = fixture();
    write(
        &fx,
        "d.csv",
        "pre\n\nid,name,amt\n1,a,5\n,,\n2,x,6\n\ntrail\n",
    );
    write(
        &fx,
        "d.magi",
        "source s = csv(\"d.csv\") { section: 2 }\nexport s to \"out/s.csv\"\n",
    );
    let check = fx.check("d.magi");
    check.assert_code(0).assert_diagnostic("M201");
    assert!(
        check
            .stderr_flat()
            .contains("section 3 (line 6) has the table's 3 fields"),
        "{}",
        check.stderr
    );
    // a trailer of another width is not a continuation
    write(
        &fx,
        "b.magi",
        "source s = csv(\"statement.csv\") { section: 2 all_text: true }\nexport s to \"out/s.csv\"\n",
    );
    fx.check("b.magi")
        .assert_code(0)
        .assert_no_diagnostic("M201");
}

#[test]
fn the_delimiter_is_guessed_from_the_table_not_a_title_with_commas() {
    let fx = fixture();
    write(
        &fx,
        "de.csv",
        "Firma GmbH, Hamburg, Kontoauszug;;\nKonto:;DE12 3456;\n;;\nDatum;Betrag;Text\n01.07.2026;\"1.234,56\";ABC\n02.07.2026;\"-10,00\";DEF\n;;\nSaldo:;;\"1.224,56\"\n",
    );
    write(
        &fx,
        "de.magi",
        "source s = csv(\"de.csv\") { section: 2 all_text: true }\nexport s to \"out/s.csv\"\n",
    );
    fx.run_ok("de.magi");
    // `project` joins with `,`: the amounts keep their decimal commas
    assert_eq!(
        fx.csv("out/s.csv").project(&["Datum", "Betrag"]),
        lines!["01.07.2026,1.234,56", "02.07.2026,-10,00"]
    );
    // M213 names the delimiter it split fields at
    write(
        &fx,
        "m.magi",
        "source s = csv(\"de.csv\") { section: 9 }\nexport s to \"out/s.csv\"\n",
    );
    let check = fx.check("m.magi");
    check.assert_code(1).assert_diagnostic("M213");
    assert!(
        check.stderr_flat().contains("fields split at `;`"),
        "{}",
        check.stderr
    );
}

#[test]
fn a_section_whose_end_depends_on_backslash_escapes_is_refused() {
    let fx = fixture();
    // `\"` then a blank line inside the quoted value: with `\` escapes the record runs to line 6
    write(
        &fx,
        "bs.csv",
        "pre\n\nid,name,amt\n1,\"a \\\",\n\nb\",5\n2,x,6\n\ntrail\n",
    );
    write(
        &fx,
        "bs.magi",
        "source s = csv(\"bs.csv\") { section: 2 }\nexport s to \"out/s.csv\"\n",
    );
    let check = fx.check("bs.magi");
    check.assert_code(1).assert_diagnostic("M212");
    assert!(
        check.stderr_flat().contains("cannot tell where section 2"),
        "{}",
        check.stderr
    );
    // backslash escapes that do not move a section's end are read as DuckDB reads them
    write(
        &fx,
        "ok.csv",
        "pre\n\nid,name,amt\n1,\"a \\\"q\\\" b\",5\n2,x,6\n\ntrail\n",
    );
    write(
        &fx,
        "ok.magi",
        "source s = csv(\"ok.csv\") { section: 2 }\nexport s to \"out/ok.csv\"\n",
    );
    fx.run_ok("ok.magi");
    assert_eq!(fx.csv("out/ok.csv").rows.len(), 2);
}

/// Sheet `S`: `id`/`amt` rows (`amt` has a no-break-space-only cell), a row holding only
/// `blank`, then a `Total` row.
fn blank_row_workbook(fx: &Fixture, blank: &str) {
    let mut wb = rust_xlsxwriter::Workbook::new();
    let ws = wb.add_worksheet();
    ws.set_name("S").unwrap();
    ws.write_string(0, 0, "id").unwrap();
    ws.write_string(0, 1, "amt").unwrap();
    ws.write_number(1, 0, 1).unwrap();
    ws.write_number(1, 1, 10.5).unwrap();
    ws.write_number(2, 0, 2).unwrap();
    ws.write_string(2, 1, "\u{a0}").unwrap();
    ws.write_number(3, 0, 3).unwrap();
    ws.write_number(3, 1, 30).unwrap();
    ws.write_string(4, 0, blank).unwrap();
    ws.write_string(5, 0, "Total").unwrap();
    ws.write_number(5, 1, 40.5).unwrap();
    wb.save(fx.file("blank.xlsx")).unwrap();
}

#[test]
fn excel_whitespace_only_rows_end_the_data_and_cells_are_blank() {
    for blank in ["  ", "\u{a0}"] {
        let fx = fixture();
        blank_row_workbook(&fx, blank);
        write(
            &fx,
            "x.magi",
            "source s = excel(\"blank.xlsx\") { sheet: \"S\" }\nexport s to \"out/s.csv\"\n",
        );
        let check = fx.check("x.magi");
        check.assert_code(0);
        let flat = check.stderr_flat();
        // the Total row below the whitespace-only row is not data
        assert!(
            flat.contains("data ends at blank row 5"),
            "{}",
            check.stderr
        );
        // the no-break-space cell does not make `amt` text
        assert!(!flat.contains("column amt"), "{}", check.stderr);
        fx.run_ok("x.magi");
        assert_eq!(
            sorted(fx.csv("out/s.csv").project(&["id", "amt"])),
            lines!["1,10.5", "2,", "3,30.0"]
        );
    }
}

#[test]
fn an_excel_section_whose_title_touches_the_table_is_reported() {
    let fx = fixture();
    let mut wb = rust_xlsxwriter::Workbook::new();
    let ws = wb.add_worksheet();
    ws.set_name("S").unwrap();
    ws.write_string(0, 0, "GL Detail").unwrap();
    for (c, h) in ["id", "amt", "memo"].iter().enumerate() {
        ws.write_string(1, c as u16, *h).unwrap();
    }
    for r in 2..6u32 {
        ws.write_number(r, 0, f64::from(r)).unwrap();
        ws.write_number(r, 1, 1.5).unwrap();
        ws.write_string(r, 2, "m").unwrap();
    }
    wb.save(fx.file("t.xlsx")).unwrap();
    write(
        &fx,
        "t.magi",
        "source s = excel(\"t.xlsx\") { sheet: \"S\" section: 1 }\nexport s to \"out/s.csv\"\n",
    );
    let check = fx.check("t.magi");
    check.assert_code(0).assert_diagnostic("M201");
    assert!(
        check
            .stderr_flat()
            .contains("the table probably starts at row 2"),
        "{}",
        check.stderr
    );
}

#[test]
fn a_whitespace_only_string_is_missing_for_identity_checks() {
    let fx = fixture();
    write(&fx, "id.csv", "id,amt\nA1,1\nA1 ,2\n\u{a0},3\n");
    write(
        &fx,
        "id.magi",
        "source s = csv(\"id.csv\") { identity id schema { id: string amt: int } }\n\
         export s to \"out/s.csv\"\n",
    );
    fx.run("id.magi", &[])
        .assert_code(1)
        .assert_diagnostic("M211");
    // other text is kept as written: `A1` and `A1 ` are two identities
    write(&fx, "ok.csv", "id,amt\nA1,1\nA1 ,2\n");
    write(
        &fx,
        "ok.magi",
        "source s = csv(\"ok.csv\") { identity id schema { id: string amt: int } }\n\
         export s to \"out/ok.csv\"\n",
    );
    fx.run_ok("ok.magi");
    assert_eq!(fx.csv("out/ok.csv").rows.len(), 2);
}

// ---------------------------------------------------------------------------------------------
// fixed-width records

/// `lic.txt`: a header record (line 1), three 24-byte detail records in cp1252 (lines 2-4: a bad
/// `N` value on line 3, a bad date on line 4) and a trailer record with the count (line 5); the
/// header and trailer are shorter than the details.
fn fixed_files(fx: &Fixture) {
    write(
        fx,
        "layout.csv",
        "field,start,length,type\nkind,1,1,A\nname,2,6,A\ntag,8,4,N\nday,12,8,D\nfee,20,5,M\n",
    );
    write(
        fx,
        "trailer.csv",
        "field,start,length,type\nkind,1,1\ncount,2,7,N\n",
    );
    let mut bytes = b"H2026 EXPORT\r\n".to_vec();
    bytes.extend_from_slice(b"DMU\xD1OZ 00122026013101500\r\n");
    bytes.extend_from_slice(b"DSMITH 00X70000000004000\r\n");
    bytes.extend_from_slice(b"DLEE   00032026139900000\r\n");
    bytes.extend_from_slice(b"T0000003\r\n");
    std::fs::write(fx.file("lic.txt"), bytes).unwrap();
}

#[test]
fn fixed_width_records_are_cut_by_the_layout_and_typed_by_its_codes() {
    let fx = fixture();
    fixed_files(&fx);
    write(
        &fx,
        "f.magi",
        "source lic = fixed_width(\"lic.txt\") {\n\
             layout: \"layout.csv\"\n\
             encoding: \"cp1252\"\n\
             record: \"D\"\n\
         }\n\
         source tags = fixed_width(\"lic.txt\") {\n\
             layout: \"layout.csv\"\n\
             encoding: \"cp1252\"\n\
             record: \"D\"\n\
             row_number: n\n\
             fill_down: day\n\
             schema { tag: string }\n\
         }\n\
         source trailer = fixed_width(\"lic.txt\") { layout: \"trailer.csv\" record: \"T\" }\n\
         export lic to \"out/lic.csv\"\n\
         export lic.rejects to \"out/rejects.csv\"\n\
         export tags to \"out/tags.csv\"\n\
         export trailer to \"out/trailer.csv\"\n",
    );
    fx.run("f.magi", &[]).assert_code(0);
    // N is an int, D a date (00000000 is none), M cents with the point implied; a value that is
    // not one is null and listed in the rejects under its file line
    assert_eq!(
        fx.csv("out/lic.csv")
            .project(&["name", "tag", "day", "fee"]),
        lines!["LEE,3,,0.00", "MUÑOZ,12,2026-01-31,15.00", "SMITH,,,40.00"]
    );
    assert_eq!(
        sorted(fx.csv("out/rejects.csv").project(&["row", "column"])),
        lines!["3,tag", "4,day"]
    );
    // a declared type decides how the field is read: the text as written; a blank date is filled
    // from the record above, a bad one stays a reject
    assert_eq!(
        fx.csv("out/tags.csv").project(&["n", "tag", "day"]),
        lines!["3,0003,", "1,0012,2026-01-31", "2,00X7,2026-01-31"]
    );
    assert_eq!(fx.csv("out/trailer.csv").project(&["count"]), lines!["3"]);
}

#[test]
fn a_fixed_width_record_of_another_length_stops_the_run() {
    let fx = fixture();
    fixed_files(&fx);
    // line 3 is 23 bytes, one short of the others
    let mut bytes = b"DMU\xD1OZ 00122026013101500\r\n".to_vec();
    bytes.extend_from_slice(b"DSMITH 00X70000000004000\r\n");
    bytes.extend_from_slice(b"DLEE   0032026139900000\r\n");
    std::fs::write(fx.file("short.txt"), bytes).unwrap();
    let check = |source: &str| {
        write(
            &fx,
            "f.magi",
            &format!("{source}\nexport s to \"out/s.csv\"\n"),
        );
        let out = fx.check("f.magi");
        out.assert_code(1).assert_diagnostic("M200");
        // miette wraps long messages behind a `│` gutter
        out.stderr
            .split_whitespace()
            .filter(|w| *w != "│")
            .collect::<Vec<_>>()
            .join(" ")
    };
    let err = check(
        "source s = fixed_width(\"short.txt\") { layout: \"layout.csv\" encoding: \"cp1252\" }",
    );
    assert!(
        err.contains("line 3: the record is 23 bytes, the one on line 1 is 24"),
        "{err}"
    );
    // without `record:` the header is a record, shorter than the layout
    let err = check(
        "source s = fixed_width(\"lic.txt\") { layout: \"layout.csv\" encoding: \"cp1252\" }",
    );
    assert!(
        err.contains("line 1: the record is 12 bytes, the layout reads up to byte 24"),
        "{err}"
    );
    // the cp1252 byte is not UTF-8
    let err = check("source s = fixed_width(\"lic.txt\") { layout: \"layout.csv\" record: \"D\" }");
    assert!(
        err.contains("line 2: field `name` is not valid UTF-8 text"),
        "{err}"
    );
}
