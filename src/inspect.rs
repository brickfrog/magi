//! `magi show` and `magi profile`: a relation's rows or per-column statistics as aligned text.
//! They print data values on purpose, like `magi diff --rows`.

use crate::backend::compare::{self, Res, db};
use crate::backend::sql::{self, Body, Expr, Query, Select, TableRef};
use crate::export;
use crate::semantic::hir::Hir;
use crate::semantic::types::{ColType, Type};

/// Longest cell printed, in characters; longer values end with `…`.
const MAX_CELL: usize = 40;

/// A value as printed: null is empty, other values are written as in a CSV file (`""` is the
/// empty string), with line breaks escaped and long values shortened.
fn cell(v: Option<&str>) -> String {
    let Some(s) = v else {
        return String::new();
    };
    let s = compare::csv_field(s)
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    if s.chars().count() > MAX_CELL {
        s.chars().take(MAX_CELL - 1).chain(['…']).collect()
    } else {
        s
    }
}

/// Left-aligned columns separated by two spaces, one line per row after the header.
fn table(header: &[String], rows: &[Vec<String>]) -> String {
    let width = |i: usize| {
        rows.iter()
            .map(|r| r[i].chars().count())
            .chain([header[i].chars().count()])
            .max()
            .unwrap_or(0)
    };
    let widths: Vec<usize> = (0..header.len()).map(width).collect();
    let mut out = String::new();
    for row in std::iter::once(header).chain(rows.iter().map(Vec::as_slice)) {
        let line = row
            .iter()
            .zip(&widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect::<Vec<_>>()
            .join("  ");
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// The first `rows` rows of `relation` in export order (`all` is every column, which breaks
/// ties as in exports), reduced to `columns`, then how many rows there are.
pub fn show(
    conn: &duckdb::Connection,
    hir: &Hir,
    relation: &str,
    all: &[String],
    columns: &[String],
    rows: usize,
) -> Res<String> {
    let total = compare::count(conn, &format!("SELECT * FROM {}", sql::ident(relation)))?;
    let mut ordered = export::ordered_query(hir, relation, all);
    if let Body::Select(s) = &mut ordered.body {
        s.limit = Some(rows as u64);
    }
    let items = columns
        .iter()
        .map(|c| {
            let text = Expr::Cast {
                expr: Box::new(sql::col(c)),
                ty: "VARCHAR".into(),
                try_: false,
            };
            (text, Some(c.clone()))
        })
        .collect();
    // the outer projection keeps the inner order
    let q = Query::select(Select {
        items,
        from: Some((TableRef::Sub(Box::new(ordered)), Some("t".into()))),
        ..Select::default()
    });
    let mut stmt = db(conn.prepare(&sql::render_query(&q)))?;
    let fetched = db(stmt.query_map([], |r| {
        (0..columns.len())
            .map(|i| r.get::<_, Option<String>>(i))
            .collect::<Result<Vec<_>, _>>()
    }))?;
    let mut cells = Vec::new();
    for row in fetched {
        cells.push(db(row)?.iter().map(|v| cell(v.as_deref())).collect());
    }
    let shown = cells.len() as i64;
    let mut out = table(columns, &cells);
    if shown < total {
        out.push_str(&format!("({shown} of {total} rows)\n"));
    } else {
        out.push_str(&format!("({total} {})\n", compare::plural(total, "row")));
    }
    Ok(out)
}

/// Per-column statistics of `relation` (`all`: its `DESCRIBE` name/type pairs): nulls, distinct
/// values, minimum, maximum, and the sum of integer and decimal columns (never of floats, whose
/// sums are not reproducible).
pub fn profile(
    conn: &duckdb::Connection,
    hir: &Hir,
    relation: &str,
    all: &[(String, String)],
) -> Res<String> {
    let rel = hir.relation(relation);
    let types: Vec<ColType> = all
        .iter()
        .map(|(name, duck)| {
            // a column MAGI does not type (e.g. a `duckdb.` call) takes the type DuckDB reports
            match rel.and_then(|r| r.column(name)).map(|c| c.ty) {
                Some(ty) if ty.ty != Type::Unknown => ty,
                declared => ColType::new(
                    Type::from_duckdb(duck),
                    declared.is_none_or(|ty| ty.nullable),
                ),
            }
        })
        .collect();
    let null = "CAST(NULL AS VARCHAR)".to_string();
    let mut items = vec!["count(*)".to_string()];
    for ((name, _), ty) in all.iter().zip(&types) {
        let c = sql::ident(name);
        let opaque = matches!(ty.ty, Type::Json | Type::Binary | Type::Unknown);
        let distinct = if opaque {
            format!("CAST({c} AS VARCHAR)")
        } else {
            c.clone()
        };
        items.push(format!("count(*) - count({c})"));
        items.push(format!("count(DISTINCT {distinct})"));
        if opaque {
            items.push(null.clone());
            items.push(null.clone());
        } else {
            items.push(format!("CAST(min({c}) AS VARCHAR)"));
            items.push(format!("CAST(max({c}) AS VARCHAR)"));
        }
        items.push(if matches!(ty.ty, Type::Int | Type::Decimal(..)) {
            format!("CAST(sum({c}) AS VARCHAR)")
        } else {
            null.clone()
        });
    }
    let q = format!("SELECT {} FROM {}", items.join(", "), sql::ident(relation));
    let (n, stats) = db(conn.query_row(&q, [], |r| {
        let n: i64 = r.get(0)?;
        let mut stats = Vec::with_capacity(all.len());
        for i in 0..all.len() {
            let at = 1 + i * 5;
            stats.push((
                r.get::<_, i64>(at)?,
                r.get::<_, i64>(at + 1)?,
                r.get::<_, Option<String>>(at + 2)?,
                r.get::<_, Option<String>>(at + 3)?,
                r.get::<_, Option<String>>(at + 4)?,
            ));
        }
        Ok((n, stats))
    }))?;
    let header: Vec<String> = ["column", "type", "nulls", "distinct", "min", "max", "sum"]
        .map(String::from)
        .to_vec();
    let rows: Vec<Vec<String>> = all
        .iter()
        .zip(&types)
        .zip(stats)
        .map(|(((name, _), ty), (nulls, distinct, min, max, sum))| {
            vec![
                name.clone(),
                ty.to_string(),
                nulls.to_string(),
                distinct.to_string(),
                cell(min.as_deref()),
                cell(max.as_deref()),
                cell(sum.as_deref()),
            ]
        })
        .collect();
    Ok(format!(
        "{relation}: {n} {}\n{}",
        compare::plural(n, "row"),
        table(&header, &rows)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_escape_line_breaks_and_shorten_long_values() {
        assert_eq!(cell(None), "");
        assert_eq!(cell(Some("")), "\"\"");
        assert_eq!(cell(Some("a\nb")), "\"a\\nb\"");
        let long = "x".repeat(50);
        let shown = cell(Some(&long));
        assert_eq!(shown.chars().count(), MAX_CELL);
        assert!(shown.ends_with('…'));
    }
}
