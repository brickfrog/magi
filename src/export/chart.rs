//! `chart` exports: a relation drawn as SVG by charts-rs.
//!
//! charts-rs keeps values as `f32`, whose spacing is already 0.03125 at 2^18, so a printed value
//! (data label, pie percentage, waterfall label, tooltip) can show wrong cents for ordinary
//! six-figure amounts. Values here only drive geometry: never turn on labels or tooltips, or
//! put `{c}`, `{d}` or `{t}` in a label formatter.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use charts_rs::{BarChart, LineChart, PieChart, WaterfallChart};
use serde_json::{Value, json};

use super::{describe, ordered_query};
use crate::backend::sql::{self, Expr, Query, Select, TableRef, func, str_lit};
use crate::semantic::hir::{Chart, ChartAxis, ChartKind, Hir};
use crate::semantic::types::Type;

/// Roboto is charts-rs' default font; without a fallback, viewers lacking it draw symbols.
const FONT: &str = "Roboto, Helvetica, Arial, sans-serif";

/// A row that is drawn.
struct Row {
    /// The x value as text (dates as `YYYY-MM-DD`).
    x: String,
    /// The x value on a continuous (time or value) axis.
    x_num: f64,
    series: Option<String>,
    y: Option<f64>,
    total: bool,
}

fn cast(e: Expr, ty: &str) -> Expr {
    Expr::Cast {
        expr: Box::new(e),
        ty: ty.into(),
        try_: false,
    }
}

fn null(ty: &str) -> Expr {
    cast(Expr::Lit(sql::Lit::Null), ty)
}

/// Draw `chart` to an SVG file at `path`; returns `(rows read, rows left out)`. Rows are left
/// out when a value the chart needs is null.
pub fn write_svg(
    conn: &duckdb::Connection,
    hir: &Hir,
    chart: &Chart,
    path: &Path,
) -> Result<(u64, u64), String> {
    let (rows, read, left_out) = read_rows(conn, hir, chart)?;
    let fail = |e: charts_rs::Error| format!("chart `{}`: {e}", chart.name);
    let json = chart_json(chart, &rows)?.to_string();
    let svg = match chart.kind {
        ChartKind::Bar => BarChart::from_json(&json).map_err(fail)?.svg(),
        ChartKind::Line => LineChart::from_json(&json).map_err(fail)?.svg(),
        ChartKind::Pie => PieChart::from_json(&json).map_err(fail)?.svg(),
        ChartKind::Waterfall => WaterfallChart::from_json(&json).map_err(fail)?.svg(),
    }
    .map_err(fail)?;
    std::fs::write(path, svg).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok((read, left_out))
}

/// The rows to draw in export order, the number of rows read, and the number left out.
fn read_rows(
    conn: &duckdb::Connection,
    hir: &Hir,
    chart: &Chart,
) -> Result<(Vec<Row>, u64, u64), String> {
    let names: Vec<String> = describe(conn, &chart.relation)?
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    let x = sql::col(&chart.x);
    let x_label = match chart.x_ty {
        Type::Date => func("strftime", vec![x.clone(), str_lit("%Y-%m-%d")]),
        Type::Timestamp | Type::TimestampTz => func(
            "strftime",
            vec![cast(x.clone(), "TIMESTAMP"), str_lit("%Y-%m-%d %H:%M:%S")],
        ),
        // decimals keep their exact text
        _ => cast(x.clone(), "VARCHAR"),
    };
    let x_num = match chart.axis {
        ChartAxis::Time => cast(func("epoch", vec![x]), "DOUBLE"),
        ChartAxis::Value => cast(x, "DOUBLE"),
        ChartAxis::Category => null("DOUBLE"),
    };
    let series = match &chart.series {
        Some(s) => cast(sql::col(s), "VARCHAR"),
        None => null("VARCHAR"),
    };
    let total = match &chart.total {
        Some(t) => sql::col(t),
        None => null("BOOLEAN"),
    };
    let items = vec![
        (x_label, Some("x_label".to_string())),
        (x_num, Some("x_num".to_string())),
        (series, Some("series".to_string())),
        (cast(sql::col(&chart.y), "DOUBLE"), Some("y".to_string())),
        (total, Some("total".to_string())),
    ];
    // the outer projection keeps the inner order
    let q = Query::select(Select {
        items,
        from: Some((
            TableRef::Sub(Box::new(ordered_query(hir, &chart.relation, &names))),
            Some("t".into()),
        )),
        ..Select::default()
    });
    let mut stmt = conn
        .prepare(&sql::render_query(&q))
        .map_err(|e| e.to_string())?;
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    let (mut out, mut read, mut left_out) = (Vec::new(), 0u64, 0u64);
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        read += 1;
        let x: Option<String> = row.get(0).map_err(|e| e.to_string())?;
        let x_num: Option<f64> = row.get(1).map_err(|e| e.to_string())?;
        let series: Option<String> = row.get(2).map_err(|e| e.to_string())?;
        let y: Option<f64> = row.get(3).map_err(|e| e.to_string())?;
        let total = row.get::<_, Option<bool>>(4).map_err(|e| e.to_string())? == Some(true);
        let y_needed = match chart.kind {
            ChartKind::Pie => true,
            ChartKind::Waterfall => !total,
            ChartKind::Bar | ChartKind::Line => false,
        };
        let Some(x) = x else {
            left_out += 1;
            continue;
        };
        if (chart.series.is_some() && series.is_none()) || (y_needed && y.is_none()) {
            left_out += 1;
            continue;
        }
        if let Some(y) = y
            && y.abs() > f32::MAX as f64
        {
            return Err(format!("chart `{}`: {y} is too large to draw", chart.name));
        }
        out.push(Row {
            x,
            x_num: x_num.unwrap_or(f64::NAN),
            series,
            y,
            total,
        });
    }
    Ok((out, read, left_out))
}

/// charts-rs options for `chart` drawing `rows`.
fn chart_json(chart: &Chart, rows: &[Row]) -> Result<Value, String> {
    let mut j = json!({
        "width": chart.width,
        "height": chart.height,
        "font_family": FONT,
        "empty_text": "no rows",
    });
    if let Some(t) = &chart.title {
        j["title_text"] = json!(t);
    }
    let axis_titles = |j: &mut Value| {
        j["x_axis_title"] = json!(chart.x);
        j["y_axis_configs"] = json!([{ "axis_title": chart.y }]);
    };
    let two_rows = |x: &str, series: Option<&str>| {
        let in_series = match (series, &chart.series) {
            (Some(s), Some(_)) => format!(" in series `{s}`"),
            _ => String::new(),
        };
        format!("chart `{}`: two rows for `{x}`{in_series}", chart.name)
    };
    match chart.kind {
        ChartKind::Bar | ChartKind::Line => {
            // series in first-appearance order; without `series`, one named after `y`
            let mut names: Vec<&str> = Vec::new();
            let mut index: HashMap<&str, usize> = HashMap::new();
            for r in rows {
                let s = r.series.as_deref().unwrap_or(&chart.y);
                if !index.contains_key(s) {
                    index.insert(s, names.len());
                    names.push(s);
                }
            }
            let series_of = |r: &Row| index[r.series.as_deref().unwrap_or(&chart.y)];
            let series_list: Vec<Value> = if chart.axis == ChartAxis::Category {
                let mut categories: Vec<&str> = Vec::new();
                let mut cat_index: HashMap<&str, usize> = HashMap::new();
                for r in rows {
                    if !cat_index.contains_key(r.x.as_str()) {
                        cat_index.insert(&r.x, categories.len());
                        categories.push(&r.x);
                    }
                }
                // `None`: no row; `Some(None)`: a row with a null y
                let mut data = vec![vec![None::<Option<f64>>; categories.len()]; names.len()];
                for r in rows {
                    let cell = &mut data[series_of(r)][cat_index[r.x.as_str()]];
                    if cell.is_some() {
                        return Err(two_rows(&r.x, r.series.as_deref()));
                    }
                    *cell = Some(r.y);
                }
                j["x_axis_data"] = json!(categories);
                names
                    .iter()
                    .zip(data)
                    .map(|(name, d)| {
                        let d: Vec<Option<f64>> = d.into_iter().map(Option::flatten).collect();
                        json!({ "name": name, "data": d })
                    })
                    .collect()
            } else {
                j["x_axis_type"] = json!(if chart.axis == ChartAxis::Time {
                    "time"
                } else {
                    "value"
                });
                let mut points: Vec<Vec<&Row>> = vec![Vec::new(); names.len()];
                for r in rows {
                    points[series_of(r)].push(r);
                }
                let mut list = Vec::with_capacity(names.len());
                for (name, mut ps) in names.iter().zip(points) {
                    ps.sort_by(|a, b| a.x_num.total_cmp(&b.x_num));
                    if let Some(w) = ps.windows(2).find(|w| w[0].x_num == w[1].x_num) {
                        return Err(two_rows(&w[1].x, w[1].series.as_deref()));
                    }
                    let xs: Vec<f64> = ps.iter().map(|r| r.x_num).collect();
                    let ys: Vec<Option<f64>> = ps.iter().map(|r| r.y).collect();
                    list.push(json!({ "name": name, "x_values": xs, "data": ys }));
                }
                list
            };
            j["series_list"] = json!(series_list);
            j["legend_show"] = json!(chart.series.is_some());
            axis_titles(&mut j);
        }
        ChartKind::Pie => {
            let mut seen: HashSet<&str> = HashSet::new();
            let mut list = Vec::with_capacity(rows.len());
            for r in rows {
                if !seen.insert(&r.x) {
                    return Err(two_rows(&r.x, None));
                }
                let y = r.y.unwrap_or_default();
                if y < 0.0 {
                    return Err(format!(
                        "chart `{}`: pie slice `{}` is negative ({y})",
                        chart.name, r.x
                    ));
                }
                list.push(json!({ "name": r.x, "data": [y] }));
            }
            j["series_list"] = json!(list);
            j["rose_type"] = json!(false);
            j["inner_radius"] = json!(0);
            // slice names only: percentages would print f32 values
            j["series_label_formatter"] = json!("{a}");
        }
        ChartKind::Waterfall => {
            let xs: Vec<&str> = rows.iter().map(|r| r.x.as_str()).collect();
            let data: Vec<Value> = rows
                .iter()
                .map(|r| json!([r.y.unwrap_or(0.0), r.total]))
                .collect();
            j["x_axis_data"] = json!(xs);
            j["data"] = json!(data);
            j["label_show"] = json!(false);
            axis_titles(&mut j);
        }
    }
    Ok(j)
}
