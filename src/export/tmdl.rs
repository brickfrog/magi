//! Power BI semantic model generation: a `model` declaration becomes a
//! TMDL folder (`<name>.SemanticModel/definition/...`) whose tables import the Parquet files the
//! program exports. MAGI describes what the model is; DuckDB prepared the data. Parquet keeps the
//! column types and nulls, so Power BI sees exactly the values `magi run` wrote.
//!
//! Metrics are translated to DAX only when the meaning is the same in both engines: plain
//! aggregates over table columns, combined with arithmetic. Null handling follows SQL: `count(x)`
//! becomes `COUNTA` (non-null values of any type), `count_distinct(x)` becomes
//! `DISTINCTCOUNTNOBLANK` (distinct non-null values), `count()` becomes `COUNTROWS`. Measures
//! name the model's columns, so a column renamed by `dimension` is referenced by its new name.
//! In Power BI a measure is evaluated in the report's filter context, so its value for a slice
//! equals the MAGI aggregate over the same rows, with one exception: VertiPaq compares text
//! ignoring case, so `count_distinct`, `min` and `max` over text can differ (M607).
//!
//! Relationships are named `<from table>.<from column> to <to table>.<to column>`. Power BI
//! allows one active filter path between two tables: relationships are activated in declaration
//! order, and one that would add a second path is written with `isActive: false` (usable from
//! DAX through `USERELATIONSHIP`).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::ast::{BinaryOp, UnaryOp};
use crate::semantic::hir::{AggFunc, Lit, Model, ModelRelationship, ModelTable, TExpr, TExprKind};
use crate::semantic::types::Type;

/// TMDL object name: quoted unless it is a plain identifier.
fn name(n: &str) -> String {
    let plain = n
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plain {
        n.to_string()
    } else {
        format!("'{}'", n.replace('\'', "''"))
    }
}

fn dax_table(t: &str) -> String {
    format!("'{}'", t.replace('\'', "''"))
}

fn dax_column(t: &str, c: &str) -> String {
    format!("{}[{}]", dax_table(t), c.replace(']', "]]"))
}

/// Power BI data type of a column. `Unknown` never reaches here: model columns need a known
/// type (M606).
pub fn data_type(t: Type) -> &'static str {
    match t {
        Type::Bool => "boolean",
        Type::Int => "int64",
        // Power BI's fixed decimal is a 64-bit integer scaled by 10^4: at most 4 decimal places
        // and ±922,337,203,685,477.5807, so at most 14 integer digits always fit; anything wider
        // is a double
        Type::Decimal(p, s) if s <= 4 && p.saturating_sub(s) <= 14 => "decimal",
        Type::Decimal(..) | Type::Float => "double",
        Type::Date | Type::Timestamp | Type::TimestampTz => "dateTime",
        _ => "string",
    }
}

pub fn format_string(format: &str) -> String {
    match format {
        "currency" => "\\$#,0.00;(\\$#,0.00);\\$#,0.00".into(),
        "percent" => "0.00%".into(),
        "integer" => "#,0".into(),
        "decimal" => "#,0.00".into(),
        other => other.into(),
    }
}

/// Translate a metric to DAX. Column slots index `tables`, and columns are referenced by their
/// model (display) name; `count()` counts rows of `home`.
pub fn dax(e: &TExpr, tables: &[ModelTable], home: &str) -> Result<String, String> {
    let rec = |x: &TExpr| dax(x, tables, home);
    Ok(match &e.kind {
        TExprKind::Literal(l) => match l {
            Lit::Int(v) => v.to_string(),
            Lit::Decimal(d) => d.clone(),
            Lit::Str(s) => format!("\"{}\"", s.replace('"', "\"\"")),
            Lit::Bool(b) => if *b { "TRUE()" } else { "FALSE()" }.to_string(),
            Lit::Null => "BLANK()".into(),
            Lit::Date(d) => {
                let p: Vec<&str> = d.split('-').collect();
                format!(
                    "DATE({}, {}, {})",
                    p[0],
                    p[1].trim_start_matches('0'),
                    p[2].trim_start_matches('0')
                )
            }
        },
        TExprKind::Agg { func, arg } => {
            let col = |a: &TExpr| {
                match &a.kind {
                TExprKind::Column { slot, name } => {
                    let table = &tables[*slot as usize];
                    let display = table
                        .columns
                        .iter()
                        .find(|c| &c.source == name)
                        .map_or(name.as_str(), |c| c.display.as_str());
                    Ok(dax_column(&table.name, display))
                }
                _ => Err("aggregates in metrics must read a column directly (derive the value in a dataset first)".to_string()),
            }
            };
            match (func, arg) {
                (AggFunc::Count, None) => format!("COUNTROWS({})", dax_table(home)),
                // SQL `count(x)` counts non-null values of any type; DAX COUNT rejects booleans
                (AggFunc::Count, Some(a)) => format!("COUNTA({})", col(a)?),
                // SQL `COUNT(DISTINCT x)` ignores nulls; DISTINCTCOUNT would count BLANK
                (AggFunc::CountDistinct, Some(a)) => format!("DISTINCTCOUNTNOBLANK({})", col(a)?),
                (AggFunc::Sum, Some(a)) => format!("SUM({})", col(a)?),
                (AggFunc::Mean, Some(a)) => format!("AVERAGE({})", col(a)?),
                // DAX MIN and MAX reject true/false columns
                (AggFunc::Min | AggFunc::Max, Some(a)) if a.ty.ty == Type::Bool => {
                    return Err(format!(
                        "DAX `{}` does not accept boolean columns; derive a 0/1 column with `to_int` in the dataset and aggregate that",
                        if matches!(func, AggFunc::Min) {
                            "MIN"
                        } else {
                            "MAX"
                        }
                    ));
                }
                (AggFunc::Min, Some(a)) => format!("MIN({})", col(a)?),
                (AggFunc::Max, Some(a)) => format!("MAX({})", col(a)?),
                (f, _) => return Err(format!("`{}` has no equivalent DAX measure", f.name())),
            }
        }
        TExprKind::Binary {
            op: BinaryOp::Div,
            left,
            right,
        } => format!("DIVIDE({}, {})", rec(left)?, rec(right)?),
        TExprKind::Binary { op, left, right } => {
            let sym = match op {
                BinaryOp::Add => "+",
                BinaryOp::Sub => "-",
                BinaryOp::Mul => "*",
                _ => {
                    return Err(format!(
                        "operator `{}` is not supported in metrics",
                        op.symbol()
                    ));
                }
            };
            format!("({} {sym} {})", rec(left)?, rec(right)?)
        }
        TExprKind::Unary {
            op: UnaryOp::Neg,
            expr,
        } => format!("-({})", rec(expr)?),
        TExprKind::Column { .. } => {
            return Err("a metric must aggregate its columns, e.g. `sum(sales.amount)`".into());
        }
        _ => return Err("only aggregates, numbers and + - * / are supported in metrics".into()),
    })
}

fn description(out: &mut String, indent: &str, text: Option<&str>) {
    if let Some(d) = text {
        for line in d.lines() {
            let _ = writeln!(out, "{indent}/// {line}");
        }
    }
}

/// Deepest directory containing every path.
fn common_dir(paths: &[&Path]) -> PathBuf {
    let mut dir = paths
        .first()
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .unwrap_or_default();
    while !paths.iter().all(|p| p.starts_with(&dir)) {
        if !dir.pop() {
            break;
        }
    }
    dir
}

/// Relationship object name, unique per model: `<from table>.<from column> to <to table>.<to column>`.
pub fn relationship_name(r: &ModelRelationship) -> String {
    format!(
        "{}.{} to {}.{}",
        r.from_table, r.from_column, r.to_table, r.to_column
    )
}

/// Why a relationship is written inactive: filters already flow from `from` to `to` through the
/// active relationships `path` (indices into the model's relationships). `from == to` means the
/// relationship would close a cycle, and `path` is the rest of that cycle.
#[derive(Debug, Clone, PartialEq)]
pub struct Inactive {
    pub from: String,
    pub to: String,
    pub path: Vec<usize>,
}

/// Active relationships, indices into `relationships`, that carry filters from table `a` to
/// table `b` (empty when `a == b`). A relationship filters from its `to` (one) side to its
/// `from` (many) side.
fn filter_path(
    relationships: &[ModelRelationship],
    active: &[usize],
    a: &str,
    b: &str,
) -> Option<Vec<usize>> {
    // breadth-first, remembering the relationship that reached each table
    let mut reached: Vec<(&str, Option<(usize, usize)>)> = vec![(a, None)];
    let mut next = 0;
    while next < reached.len() {
        let (table, _) = reached[next];
        if table == b {
            let mut path = Vec::new();
            let mut at = next;
            while let (_, Some((via, prev))) = reached[at] {
                path.push(via);
                at = prev;
            }
            path.reverse();
            return Some(path);
        }
        for &i in active {
            let r = &relationships[i];
            if r.to_table == table && !reached.iter().any(|(t, _)| *t == r.from_table) {
                reached.push((&r.from_table, Some((i, next))));
            }
        }
        next += 1;
    }
    None
}

/// Tables that filter `t` through the active relationships (`t` first), or that `t` filters
/// (`downstream`).
fn filter_reach<'r>(
    relationships: &'r [ModelRelationship],
    active: &[usize],
    t: &'r str,
    downstream: bool,
) -> Vec<&'r str> {
    let mut out = vec![t];
    let mut next = 0;
    while next < out.len() {
        for &i in active {
            let r = &relationships[i];
            let (near, far) = if downstream {
                (&r.to_table, &r.from_table)
            } else {
                (&r.from_table, &r.to_table)
            };
            if near == out[next] && !out.contains(&far.as_str()) {
                out.push(far);
            }
        }
        next += 1;
    }
    out
}

/// Power BI allows one active filter path between two tables. Relationships are activated in
/// declaration order; one that would add a second path between two tables (a second
/// relationship between the same tables, a triangle such as sales→customers→regions next to
/// sales→regions, or a cycle) is written inactive. For each relationship, why it is inactive.
pub fn inactive(relationships: &[ModelRelationship]) -> Vec<Option<Inactive>> {
    let mut active: Vec<usize> = Vec::new();
    let mut out = Vec::new();
    for (i, r) in relationships.iter().enumerate() {
        // the new relationship carries filters from `to_table` to `from_table`: every table that
        // filters `to_table` would reach every table `from_table` filters
        let sources = filter_reach(relationships, &active, &r.to_table, false);
        let targets = filter_reach(relationships, &active, &r.from_table, true);
        let conflict = sources.iter().find_map(|&x| {
            targets.iter().find_map(|&y| {
                let path = filter_path(relationships, &active, x, y)?;
                let path = if x == y {
                    // a cycle: x reaches `to_table`, `from_table` reaches x
                    let mut p = filter_path(relationships, &active, x, &r.to_table)?;
                    p.extend(filter_path(relationships, &active, &r.from_table, x)?);
                    p
                } else {
                    path
                };
                Some(Inactive {
                    from: x.to_string(),
                    to: y.to_string(),
                    path,
                })
            })
        });
        if conflict.is_none() {
            active.push(i);
        }
        out.push(conflict);
    }
    out
}

/// Whether a metric compares text values (`count_distinct`, `min`, `max` over a text column),
/// which VertiPaq does ignoring case while DuckDB does not.
pub fn compares_text(e: &TExpr) -> bool {
    let mut found = false;
    e.walk(&mut |x| {
        if let TExprKind::Agg {
            func: AggFunc::CountDistinct | AggFunc::Min | AggFunc::Max,
            arg: Some(a),
        } = &x.kind
        {
            found |= a.ty.ty == Type::String;
        }
    });
    found
}

/// DAX that makes a measure filter through an inactive relationship.
pub fn use_relationship(r: &ModelRelationship) -> String {
    format!(
        "USERELATIONSHIP({}, {})",
        dax_column(&r.from_table, &r.from_column),
        dax_column(&r.to_table, &r.to_column)
    )
}

fn m_string(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Prefix of the scratch folders `write` builds a definition in; only MAGI creates them.
const SCRATCH: &str = ".definition.magi-";

/// Write `<dir>/<model>.SemanticModel/`. The tables' files are read through a `DataFolder`
/// parameter whose default is `data_folder` (or the directory holding all exported files), so
/// the model can be pointed at a copy of the data from Power BI.
///
/// Power BI loads every file under `definition/`, so the definition is built in a scratch folder
/// and swapped in whole: tables and relationships dropped from the program disappear, and a
/// failed write leaves the previous definition intact. Other files in the folder (Power BI
/// Desktop's `.pbi/` cache and settings, `diagramLayout.json`, `.platform`) are kept.
pub fn write(model: &Model, dir: &Path, data_folder: Option<&str>) -> Result<Vec<PathBuf>, String> {
    let root = dir.join(format!("{}.SemanticModel", model.name));
    let files = definition(model, data_folder);
    std::fs::create_dir_all(&root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;
    // scratch folders left by an interrupted run
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(SCRATCH)
                && entry.file_type().is_ok_and(|t| t.is_dir())
            {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    let pbism = root.join("definition.pbism");
    std::fs::write(
        &pbism,
        "{\n  \"version\": \"4.0\",\n  \"settings\": {}\n}\n",
    )
    .map_err(|e| format!("cannot write {}: {e}", pbism.display()))?;

    let pid = std::process::id();
    let scratch = root.join(format!("{SCRATCH}new-{pid}"));
    let build = || -> Result<(), String> {
        for (relative, text) in &files {
            let path = scratch.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
            }
            std::fs::write(&path, text)
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        }
        Ok(())
    };
    if let Err(e) = build() {
        let _ = std::fs::remove_dir_all(&scratch);
        return Err(e);
    }
    let def = root.join("definition");
    let old = root.join(format!("{SCRATCH}old-{pid}"));
    let replaced = std::fs::symlink_metadata(&def).is_ok();
    if replaced && let Err(e) = std::fs::rename(&def, &old) {
        let _ = std::fs::remove_dir_all(&scratch);
        return Err(format!("cannot replace {}: {e}", def.display()));
    }
    if let Err(e) = std::fs::rename(&scratch, &def) {
        if replaced {
            let _ = std::fs::rename(&old, &def);
        }
        let _ = std::fs::remove_dir_all(&scratch);
        return Err(format!("cannot write {}: {e}", def.display()));
    }
    if replaced {
        // a file or symlink named `definition` is removed itself, never followed
        let _ = std::fs::remove_dir_all(&old).or_else(|_| std::fs::remove_file(&old));
    }
    let mut written = vec![pbism];
    written.extend(files.into_iter().map(|(relative, _)| def.join(relative)));
    Ok(written)
}

/// Default format string of a measure whose value has type `ty`.
pub fn measure_format(ty: Type) -> String {
    match ty {
        Type::Int => format_string("integer"),
        Type::Date => "yyyy-mm-dd".into(),
        Type::Timestamp | Type::TimestampTz => "yyyy-mm-dd hh:nn:ss".into(),
        _ => format_string("decimal"),
    }
}

/// The files of `definition/`, relative to it.
fn definition(model: &Model, data_folder: Option<&str>) -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    let paths: Vec<&Path> = model.tables.iter().map(|t| t.path.as_path()).collect();
    let folder = common_dir(&paths);
    // Power BI resolves files from its own working directory: the default must be absolute
    let absolute = std::fs::canonicalize(&folder)
        .or_else(|_| std::path::absolute(&folder))
        .unwrap_or_else(|_| folder.clone());
    let mut folder_text = data_folder
        .map(str::to_string)
        .unwrap_or_else(|| absolute.display().to_string());
    if !folder_text.ends_with(['/', '\\']) {
        folder_text.push(if folder_text.contains('\\') {
            '\\'
        } else {
            '/'
        });
    }

    files.push((
        PathBuf::from("database.tmdl"),
        format!(
            "database {}\n\tcompatibilityLevel: 1567\n",
            name(&model.name)
        ),
    ));

    let mut m = String::from(
        "model Model\n\tculture: en-US\n\tdefaultPowerBIDataSourceVersion: powerBI_V3\n\tsourceQueryCulture: en-US\n\n",
    );
    for t in &model.tables {
        let _ = writeln!(m, "ref table {}", name(&t.name));
    }
    files.push((PathBuf::from("model.tmdl"), m));
    files.push((
        PathBuf::from("expressions.tmdl"),
        format!(
            "/// Folder holding the files MAGI exported for this model\nexpression DataFolder = {} meta [IsParameterQuery=true, Type=\"Text\", IsParameterQueryRequired=true]\n",
            m_string(&folder_text)
        ),
    ));

    if !model.relationships.is_empty() {
        let inactive = inactive(&model.relationships);
        let mut r = String::new();
        for (rel, inactive) in model.relationships.iter().zip(&inactive) {
            let _ = writeln!(r, "relationship {}", name(&relationship_name(rel)));
            if inactive.is_some() {
                let _ = writeln!(r, "\tisActive: false");
            }
            let _ = writeln!(
                r,
                "\tfromColumn: {}.{}",
                name(&rel.from_table),
                name(&rel.from_column)
            );
            let _ = writeln!(
                r,
                "\ttoColumn: {}.{}\n",
                name(&rel.to_table),
                name(&rel.to_column)
            );
        }
        files.push((PathBuf::from("relationships.tmdl"), r));
    }

    for t in &model.tables {
        let mut s = String::new();
        description(&mut s, "", t.description.as_deref());
        let _ = writeln!(s, "table {}\n", name(&t.name));
        for metric in model.metrics.iter().filter(|x| x.table == t.name) {
            description(&mut s, "\t", metric.description.as_deref());
            let _ = writeln!(s, "\tmeasure {} = {}", name(&metric.name), metric.dax);
            let _ = writeln!(s, "\t\tformatString: {}\n", metric.format);
        }
        for c in &t.columns {
            description(&mut s, "\t", c.description.as_deref());
            let _ = writeln!(s, "\tcolumn {}", name(&c.display));
            let _ = writeln!(s, "\t\tdataType: {}", data_type(c.ty));
            if c.hidden {
                let _ = writeln!(s, "\t\tisHidden");
            }
            if c.ty == Type::Date {
                let _ = writeln!(s, "\t\tformatString: yyyy-mm-dd");
            }
            let _ = writeln!(s, "\t\tsummarizeBy: none");
            let _ = writeln!(s, "\t\tsourceColumn: {}\n", c.source);
        }
        let relative = t
            .path
            .strip_prefix(&folder)
            .unwrap_or(&t.path)
            .display()
            .to_string();
        // model tables are exported to Parquet (M601), which carries column types and nulls
        let file = format!("DataFolder & {}", m_string(&relative));
        let reader = format!("Parquet.Document(File.Contents({file}))");
        let _ = writeln!(s, "\tpartition {} = m", name(&t.name));
        let _ = writeln!(s, "\t\tmode: import");
        let _ = writeln!(
            s,
            "\t\tsource =\n\t\t\t\tlet\n\t\t\t\t\tSource = {reader}\n\t\t\t\tin\n\t\t\t\t\tSource"
        );
        files.push((PathBuf::from("tables").join(format!("{}.tmdl", t.name)), s));
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `many -> one` relationships by table name.
    fn rels(pairs: &[(&str, &str)]) -> Vec<ModelRelationship> {
        pairs
            .iter()
            .map(|(from, to)| ModelRelationship {
                from_table: from.to_string(),
                from_column: "k".into(),
                to_table: to.to_string(),
                to_column: "k".into(),
            })
            .collect()
    }

    fn inactive_at(pairs: &[(&str, &str)]) -> Vec<usize> {
        inactive(&rels(pairs))
            .iter()
            .enumerate()
            .filter_map(|(i, w)| w.as_ref().map(|_| i))
            .collect()
    }

    #[test]
    fn only_relationships_adding_a_second_filter_path_are_inactive() {
        // snowflake and two facts sharing two dimensions: one path between any two tables
        assert!(inactive_at(&[("sales", "customers"), ("customers", "regions")]).is_empty());
        assert!(
            inactive_at(&[
                ("sales", "dates"),
                ("sales", "products"),
                ("budget", "dates"),
                ("budget", "products"),
            ])
            .is_empty()
        );
        // same pair, either direction
        assert_eq!(inactive_at(&[("a", "b"), ("a", "b")]), [1]);
        assert_eq!(inactive_at(&[("a", "b"), ("b", "a")]), [1]);
        // triangle, in any declaration order: the last one declared loses
        assert_eq!(inactive_at(&[("s", "c"), ("c", "r"), ("s", "r")]), [2]);
        assert_eq!(inactive_at(&[("s", "r"), ("s", "c"), ("c", "r")]), [2]);
        // longer diamond: d filters a through b and through c
        assert_eq!(
            inactive_at(&[("a", "b"), ("b", "d"), ("a", "c"), ("c", "d")]),
            [3]
        );
        // a cycle of three
        let r = rels(&[("a", "b"), ("b", "c"), ("c", "a")]);
        let why = inactive(&r);
        assert_eq!(
            why[2],
            Some(Inactive {
                from: "a".into(),
                to: "a".into(),
                path: vec![1, 0]
            })
        );
    }

    #[test]
    fn inactive_relationships_name_the_active_path() {
        let why = inactive(&rels(&[("s", "c"), ("s", "r"), ("c", "r")]));
        assert_eq!(
            why[2],
            Some(Inactive {
                from: "r".into(),
                to: "s".into(),
                path: vec![1]
            })
        );
    }
}
