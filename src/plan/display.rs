//! Human-readable rendering of typed expressions for `magi explain` and `magi plan`.

use crate::ast::UnaryOp;
use crate::semantic::hir::{Lit, TExpr, TExprKind, WinFunc};

/// Render with single-input column names (slot 1 columns are prefixed `r.`).
pub fn expr(e: &TExpr) -> String {
    render(e, &["", "r."])
}

fn render(e: &TExpr, p: &[&str]) -> String {
    let r = |x: &TExpr| render(x, p);
    match &e.kind {
        TExprKind::Column { slot, name } => {
            let prefix = p.get(*slot as usize).copied().unwrap_or("?.");
            if is_plain(name) {
                format!("{prefix}{name}")
            } else {
                format!("{prefix}`{name}`")
            }
        }
        TExprKind::Literal(l) => match l {
            Lit::Int(v) => v.to_string(),
            Lit::Decimal(d) => d.clone(),
            Lit::Str(s) => format!("{s:?}"),
            Lit::Bool(b) => b.to_string(),
            Lit::Null => "null".into(),
            Lit::Date(d) => format!("date({d:?})"),
        },
        TExprKind::Call { func, args } => format!("{func}({})", join(args, p)),
        TExprKind::Native { name, args } => format!("duckdb.{name}({})", join(args, p)),
        TExprKind::Agg { func, arg } => match arg {
            Some(a) => format!("{}({})", func.name(), r(a)),
            None => format!("{}()", func.name()),
        },
        TExprKind::Unary {
            op: UnaryOp::Not,
            expr,
        } => format!("not {}", paren(expr, p)),
        TExprKind::Unary {
            op: UnaryOp::Neg,
            expr,
        } => format!("-{}", paren(expr, p)),
        TExprKind::Binary { op, left, right } => {
            format!("{} {} {}", paren(left, p), op.symbol(), paren(right, p))
        }
        TExprKind::IsNull { expr, negated } => {
            format!(
                "{} is {}null",
                paren(expr, p),
                if *negated { "not " } else { "" }
            )
        }
        TExprKind::InList {
            expr,
            list,
            negated,
        } => {
            format!(
                "{} {}in [{}]",
                paren(expr, p),
                if *negated { "not " } else { "" },
                join(list, p)
            )
        }
        TExprKind::Case { arms, otherwise } => {
            let mut s = String::from("case { ");
            for (w, t) in arms {
                s += &format!("{} => {}, ", r(w), r(t));
            }
            if let Some(o) = otherwise {
                s += &format!("otherwise => {} ", r(o));
            }
            s + "}"
        }
        TExprKind::TryCast { expr, ty } => format!("try_cast({} as {ty})", r(expr)),
        TExprKind::List(items) => format!("[{}]", join(items, p)),
        TExprKind::Window {
            func,
            arg,
            partition,
            order,
            filter,
        } => {
            let name = match func {
                WinFunc::RowNumber => "row_number",
                WinFunc::Rank => "rank",
                WinFunc::DenseRank => "dense_rank",
                WinFunc::Count => "count",
                WinFunc::Sum => "sum",
            };
            let mut s = format!("{name}({})", arg.as_ref().map(|a| r(a)).unwrap_or_default());
            if let Some(f) = filter {
                s += &format!(" filter ({})", r(f));
            }
            s += " over (";
            if !partition.is_empty() {
                s += &format!("partition by {}", join(partition, p));
            }
            if !order.is_empty() {
                if !partition.is_empty() {
                    s += " ";
                }
                s += "order by ";
                s += &order
                    .iter()
                    .map(|o| format!("{}{}", r(&o.expr), if o.desc { " desc" } else { "" }))
                    .collect::<Vec<_>>()
                    .join(", ");
            }
            s + ")"
        }
    }
}

fn join(xs: &[TExpr], p: &[&str]) -> String {
    xs.iter()
        .map(|x| render(x, p))
        .collect::<Vec<_>>()
        .join(", ")
}

fn paren(e: &TExpr, p: &[&str]) -> String {
    match &e.kind {
        TExprKind::Binary { .. } => format!("({})", render(e, p)),
        _ => render(e, p),
    }
}

fn is_plain(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}
