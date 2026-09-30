//! DuckDB backend: lowers logical plans and typed expressions to the SQL AST. DuckDB defines
//! MAGI's canonical execution semantics.

use crate::ast::{BinaryOp, UnaryOp};
use crate::backend::sql::{
    self, Body, Expr, Join, Lit as SLit, OrderBy, Query, Select, TableRef, func, str_lit,
};
use crate::plan::logical::{JoinType, LogicalPlan};
use crate::semantic::hir::{AggFunc, Lit, TExpr, TExprKind, WinFunc};
use crate::semantic::types::Type;

/// Aliases used for operator inputs: slot 0 / slot 1.
const SINGLE: [Option<&str>; 2] = [None, None];
const JOINED: [Option<&str>; 2] = [Some("l"), Some("r")];

pub fn expr(e: &TExpr) -> Expr {
    lower(e, &SINGLE)
}

fn norm_text(x: Expr) -> Expr {
    // lower(trim(collapse whitespace))
    func(
        "lower",
        vec![func(
            "trim",
            vec![func(
                "regexp_replace",
                vec![x, str_lit(r"\s+"), str_lit(" "), str_lit("g")],
            )],
        )],
    )
}

/// `body`, or null when either text is blank after trimming.
fn unless_blank(x: Expr, y: Expr, body: Expr) -> Expr {
    let blank = |e: Expr| sql::bin("=", func("trim", vec![e]), str_lit(""));
    Expr::Case {
        whens: vec![(
            sql::bin("OR", blank(x), blank(y)),
            Expr::Lit(sql::Lit::Null),
        )],
        otherwise: Some(Box::new(body)),
    }
}

fn words(x: Expr) -> Expr {
    func(
        "string_split",
        vec![
            func(
                "trim",
                vec![func(
                    "regexp_replace",
                    vec![x, str_lit(r"\s+"), str_lit(" "), str_lit("g")],
                )],
            ),
            str_lit(" "),
        ],
    )
}

fn lower(e: &TExpr, a: &[Option<&str>; 2]) -> Expr {
    let l = |x: &TExpr| lower(x, a);
    let args = |xs: &[TExpr]| xs.iter().map(|x| lower(x, a)).collect::<Vec<_>>();
    match &e.kind {
        TExprKind::Column { slot, name } => Expr::Col {
            table: a[*slot as usize].map(str::to_string),
            name: name.clone(),
        },
        TExprKind::Literal(lit) => Expr::Lit(match lit {
            Lit::Int(v) => SLit::Int(*v),
            Lit::Decimal(d) => SLit::Decimal(d.clone()),
            Lit::Str(s) => SLit::Str(s.clone()),
            Lit::Bool(b) => SLit::Bool(*b),
            Lit::Null => SLit::Null,
            Lit::Date(d) => SLit::Date(d.clone()),
        }),
        TExprKind::List(items) => Expr::List(args(items)),
        TExprKind::Unary {
            op: UnaryOp::Not,
            expr,
        } => Expr::Not(Box::new(l(expr))),
        TExprKind::Unary {
            op: UnaryOp::Neg,
            expr,
        } => Expr::Neg(Box::new(l(expr))),
        TExprKind::Binary { op, left, right } => {
            let sym = match op {
                BinaryOp::Or => "OR",
                BinaryOp::And => "AND",
                BinaryOp::Eq => "=",
                BinaryOp::NotEq => "<>",
                BinaryOp::Lt => "<",
                BinaryOp::Le => "<=",
                BinaryOp::Gt => ">",
                BinaryOp::Ge => ">=",
                BinaryOp::Add => "+",
                BinaryOp::Sub => "-",
                BinaryOp::Mul => "*",
                BinaryOp::Div => "/",
                BinaryOp::Mod => "%",
            };
            // DuckDB `/` on integers is already floating-point division
            sql::bin(sym, l(left), l(right))
        }
        TExprKind::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(l(expr)),
            negated: *negated,
        },
        TExprKind::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(l(expr)),
            list: args(list),
            negated: *negated,
        },
        TExprKind::Case { arms, otherwise } => Expr::Case {
            whens: arms.iter().map(|(w, t)| (l(w), l(t))).collect(),
            otherwise: otherwise.as_ref().map(|o| Box::new(l(o))),
        },
        TExprKind::TryCast { expr, ty } => Expr::Cast {
            expr: Box::new(l(expr)),
            ty: ty.duckdb_name(),
            try_: true,
        },
        TExprKind::Native { name, args: xs } => func(name, args(xs)),
        TExprKind::Agg { func: f, arg } => {
            let arg = arg.as_ref().map(|x| (l(x), x.ty.ty));
            agg(*f, arg, e.ty.ty)
        }
        TExprKind::Window {
            func: f,
            arg,
            partition,
            order,
            filter,
        } => {
            let name = match f {
                WinFunc::RowNumber => "row_number",
                WinFunc::Rank => "rank",
                WinFunc::DenseRank => "dense_rank",
                WinFunc::Count => "count",
                WinFunc::Max => "max",
                WinFunc::Min => "min",
                WinFunc::Lead => "lead",
            };
            let call = Expr::Func {
                name: name.into(),
                args: match (f, arg) {
                    (_, Some(x)) => vec![l(x)],
                    (WinFunc::Count, None) => vec![Expr::Star { table: None }],
                    _ => Vec::new(),
                },
                distinct: false,
                filter: filter.as_ref().map(|x| Box::new(l(x))),
            };
            Expr::Window {
                func: Box::new(call),
                partition: args(partition),
                order: order
                    .iter()
                    .map(|o| OrderBy {
                        expr: l(&o.expr),
                        desc: o.desc,
                    })
                    .collect(),
            }
        }
        TExprKind::Call {
            func: name,
            args: xs,
        } => call(name, xs, a, e.ty.ty),
    }
}

fn agg(f: AggFunc, arg: Option<(Expr, Type)>, result: Type) -> Expr {
    let simple = |name: &str, x: Expr| func(name, vec![x]);
    match (f, arg) {
        (AggFunc::Count, None) => func("count", vec![Expr::Star { table: None }]),
        (AggFunc::Count, Some((x, _))) => simple("count", x),
        (AggFunc::CountDistinct, Some((x, _))) => Expr::Func {
            name: "count".into(),
            args: vec![x],
            distinct: true,
            filter: None,
        },
        (AggFunc::Sum, Some((x, Type::Int))) => Expr::Cast {
            expr: Box::new(simple("sum", x)),
            ty: "BIGINT".into(),
            try_: false,
        },
        (AggFunc::Sum, Some((x, _))) => {
            let s = simple("sum", x);
            if let Type::Decimal(p, sc) = result {
                Expr::Cast {
                    expr: Box::new(s),
                    ty: format!("DECIMAL({p},{sc})"),
                    try_: false,
                }
            } else {
                s
            }
        }
        (AggFunc::Mean, Some((x, _))) => simple("avg", x),
        (AggFunc::Min, Some((x, _))) => simple("min", x),
        (AggFunc::Max, Some((x, _))) => simple("max", x),
        (AggFunc::Any, Some((x, _))) => simple("bool_or", x),
        (AggFunc::All, Some((x, _))) => simple("bool_and", x),
        (AggFunc::Missing, Some((x, ty))) => {
            let missing = if ty == Type::String {
                sql::bin(
                    "OR",
                    Expr::IsNull {
                        expr: Box::new(x.clone()),
                        negated: false,
                    },
                    sql::bin("=", func("trim", vec![x]), str_lit("")),
                )
            } else {
                Expr::IsNull {
                    expr: Box::new(x),
                    negated: false,
                }
            };
            func(
                "avg",
                vec![Expr::Case {
                    whens: vec![(missing, Expr::Lit(SLit::Decimal("1.0".into())))],
                    otherwise: Some(Box::new(Expr::Lit(SLit::Decimal("0.0".into())))),
                }],
            )
        }
        (f, None) => func(f.name(), Vec::new()),
    }
}

fn call(name: &str, xs: &[TExpr], a: &[Option<&str>; 2], result: Type) -> Expr {
    let arg = |i: usize| lower(&xs[i], a);
    let all = || xs.iter().map(|x| lower(x, a)).collect::<Vec<_>>();
    let cast = |x: Expr, ty: &str, try_: bool| Expr::Cast {
        expr: Box::new(x),
        ty: ty.to_string(),
        try_,
    };
    // DuckDB takes INTEGER for these arguments and does not cast MAGI's BIGINT ints implicitly
    // (small literals already are INTEGER)
    let int32 = |i: usize| match &xs[i].kind {
        TExprKind::Literal(Lit::Int(v)) if i32::try_from(*v).is_ok() => arg(i),
        _ => cast(arg(i), "INTEGER", false),
    };
    match name {
        "lower" | "upper" | "trim" | "strip_accents" | "replace" | "contains" | "starts_with"
        | "length" | "concat" | "coalesce" | "nullif" | "abs" | "floor" | "ceil" | "least"
        | "greatest" | "year" | "month" | "day" | "quarter" | "levenshtein" | "regexp_matches" => {
            func(name, all())
        }
        "lpad" | "rpad" => func(name, vec![arg(0), int32(1), arg(2)]),
        "round" if xs.len() == 2 => func("round", vec![arg(0), int32(1)]),
        "round" => func("round", all()),
        "ends_with" => func("suffix", all()),
        "substr" => func("substring", all()),
        "regexp_replace" => func("regexp_replace", vec![arg(0), arg(1), arg(2), str_lit("g")]),
        "regexp_extract" => {
            let group = if xs.len() == 3 { int32(2) } else { sql::int(0) };
            func(
                "nullif",
                vec![
                    func("regexp_extract", vec![arg(0), arg(1), group]),
                    str_lit(""),
                ],
            )
        }
        "day_of_week" => func("isodow", all()),
        "if" => Expr::Case {
            whens: vec![(arg(0), arg(1))],
            otherwise: Some(Box::new(arg(2))),
        },
        "to_string" => cast(arg(0), "VARCHAR", false),
        "to_int" => cast(arg(0), "BIGINT", true),
        "to_float" => cast(arg(0), "DOUBLE", true),
        "to_date" => cast(arg(0), "DATE", true),
        "to_bool" => cast(arg(0), "BOOLEAN", true),
        "parse_number" => {
            let t = func("trim", vec![arg(0)]);
            let thousands = func(
                "regexp_matches",
                vec![t.clone(), str_lit(r"^[+-]?\d{1,3}(,\d{3})+(\.\d+)?$")],
            );
            let cleaned = Expr::Case {
                whens: vec![(
                    thousands,
                    func("replace", vec![t.clone(), str_lit(","), str_lit("")]),
                )],
                otherwise: Some(Box::new(t)),
            };
            cast(cleaned, &result.duckdb_name(), true)
        }
        "parse_date" => cast(
            func("try_strptime", vec![func("trim", vec![arg(0)]), arg(1)]),
            "DATE",
            false,
        ),
        "parse_timestamp" => func("try_strptime", vec![func("trim", vec![arg(0)]), arg(1)]),
        "make_date" => cast(
            func(
                "try_strptime",
                vec![
                    func(
                        "printf",
                        vec![str_lit("%04d-%02d-%02d"), arg(0), arg(1), arg(2)],
                    ),
                    str_lit("%Y-%m-%d"),
                ],
            ),
            "DATE",
            false,
        ),
        "days_between" => func(
            "abs",
            vec![func("date_diff", vec![str_lit("day"), arg(0), arg(1)])],
        ),
        "date_diff" => func("date_diff", vec![str_lit("day"), arg(0), arg(1)]),
        "add_days" => {
            if xs[0].ty.ty == Type::Date {
                sql::bin("+", arg(0), cast(arg(1), "INTEGER", false))
            } else {
                sql::bin(
                    "+",
                    arg(0),
                    func("to_days", vec![cast(arg(1), "INTEGER", false)]),
                )
            }
        }
        // blank text has no similarity to anything: null (fails `>=`, ranks last)
        "similarity" => unless_blank(
            arg(0),
            arg(1),
            func(
                "jaro_winkler_similarity",
                vec![norm_text(arg(0)), norm_text(arg(1))],
            ),
        ),
        "token_similarity" => {
            let set = |x: Expr| {
                func(
                    "list_distinct",
                    vec![func("string_split", vec![norm_text(x), str_lit(" ")])],
                )
            };
            let (sa, sb) = (set(arg(0)), set(arg(1)));
            let inter = func(
                "len",
                vec![func("list_intersect", vec![sa.clone(), sb.clone()])],
            );
            let union = func(
                "len",
                vec![func(
                    "list_distinct",
                    vec![func("list_concat", vec![sa, sb])],
                )],
            );
            unless_blank(
                arg(0),
                arg(1),
                sql::bin(
                    "/",
                    cast(inter, "DOUBLE", false),
                    func("nullif", vec![union, sql::int(0)]),
                ),
            )
        }
        "is_null" => Expr::IsNull {
            expr: Box::new(arg(0)),
            negated: false,
        },
        "replace_words" => {
            let TExprKind::List(pairs) = &xs[1].kind else {
                unreachable!("replace_words pairs")
            };
            let w = sql::col("w");
            let whens = pairs
                .chunks(2)
                .map(|p| (sql::bin("=", w.clone(), lower(&p[0], a)), lower(&p[1], a)))
                .collect();
            let body = Expr::Case {
                whens,
                otherwise: Some(Box::new(w)),
            };
            func(
                "array_to_string",
                vec![
                    func(
                        "list_transform",
                        vec![
                            words(arg(0)),
                            Expr::Lambda {
                                param: "w".into(),
                                body: Box::new(body),
                            },
                        ],
                    ),
                    str_lit(" "),
                ],
            )
        }
        other => unreachable!("function `{other}` has no DuckDB lowering"),
    }
}

// ---------------------------------------------------------------------------------------------
// plans

struct Builder {
    ctes: Vec<(String, Query)>,
    prefix: String,
}

impl Builder {
    fn push(&mut self, q: Query) -> TableRef {
        let name = format!("{}{}", self.prefix, self.ctes.len() + 1);
        self.ctes.push((name.clone(), q));
        TableRef::Named(name)
    }

    fn items(exprs: &[(TExpr, String)], aliases: [Option<&str>; 2]) -> Vec<(Expr, Option<String>)> {
        exprs
            .iter()
            .map(|(e, n)| (lower(e, &aliases), Some(n.clone())))
            .collect()
    }

    fn node(&mut self, p: &LogicalPlan) -> TableRef {
        let star = || vec![(Expr::Star { table: None }, None)];
        match p {
            LogicalPlan::Scan { table } => TableRef::Named(table.clone()),
            LogicalPlan::NativeSql { sql } => self.push(Query {
                ctes: Vec::new(),
                body: Body::Raw(sql.clone()),
            }),
            LogicalPlan::Project { input, exprs } => {
                let from = self.node(input);
                self.push(Query::select(Select {
                    items: Self::items(exprs, [None, None]),
                    from: Some((from, None)),
                    ..Select::default()
                }))
            }
            LogicalPlan::Filter { input, predicate } => {
                let from = self.node(input);
                self.push(Query::select(Select {
                    items: star(),
                    from: Some((from, None)),
                    where_: Some(expr(predicate)),
                    ..Select::default()
                }))
            }
            LogicalPlan::Join {
                left,
                right,
                kind,
                on,
                output,
            } => {
                let l = self.node(left);
                let r = self.node(right);
                let (kw, items) = match kind {
                    JoinType::Inner => ("JOIN", Self::items(output, JOINED)),
                    JoinType::Left => ("LEFT JOIN", Self::items(output, JOINED)),
                    JoinType::Right => ("RIGHT JOIN", Self::items(output, JOINED)),
                    JoinType::Full => ("FULL JOIN", Self::items(output, JOINED)),
                    JoinType::Semi => (
                        "SEMI JOIN",
                        vec![(
                            Expr::Star {
                                table: Some("l".into()),
                            },
                            None,
                        )],
                    ),
                    JoinType::Anti => (
                        "ANTI JOIN",
                        vec![(
                            Expr::Star {
                                table: Some("l".into()),
                            },
                            None,
                        )],
                    ),
                };
                let on = on
                    .as_ref()
                    .map(|c| lower(c, &JOINED))
                    .or(Some(Expr::Lit(SLit::Bool(true))));
                self.push(Query::select(Select {
                    items,
                    from: Some((l, Some("l".into()))),
                    joins: vec![Join {
                        kind: kw,
                        table: r,
                        alias: Some("r".into()),
                        on,
                    }],
                    ..Select::default()
                }))
            }
            LogicalPlan::Aggregate { input, group, aggs } => {
                let from = self.node(input);
                let mut items = Self::items(group, [None, None]);
                items.extend(Self::items(aggs, [None, None]));
                let group_by = (1..=group.len()).map(|i| sql::int(i as i64)).collect();
                self.push(Query::select(Select {
                    items,
                    from: Some((from, None)),
                    group_by,
                    ..Select::default()
                }))
            }
            LogicalPlan::Window { input, exprs } => {
                let from = self.node(input);
                let mut items = star();
                items.extend(Self::items(exprs, [None, None]));
                self.push(Query::select(Select {
                    items,
                    from: Some((from, None)),
                    ..Select::default()
                }))
            }
            LogicalPlan::Union { inputs } => {
                let parts = inputs
                    .iter()
                    .map(|i| {
                        Query::select(Select {
                            items: star(),
                            from: Some((self.node(i), None)),
                            ..Select::default()
                        })
                    })
                    .collect();
                self.push(Query {
                    ctes: Vec::new(),
                    body: Body::UnionAllByName(parts),
                })
            }
            LogicalPlan::Distinct { input } => {
                let from = self.node(input);
                self.push(Query::select(Select {
                    distinct: true,
                    items: star(),
                    from: Some((from, None)),
                    ..Select::default()
                }))
            }
            LogicalPlan::Sort { input, keys } => {
                let from = self.node(input);
                self.push(Query::select(Select {
                    items: star(),
                    from: Some((from, None)),
                    order_by: order(keys),
                    ..Select::default()
                }))
            }
            LogicalPlan::Limit { input, n } => {
                // keep ORDER BY and LIMIT in one SELECT so the kept rows are well defined
                if let LogicalPlan::Sort { input: inner, keys } = input.as_ref() {
                    let from = self.node(inner);
                    return self.push(Query::select(Select {
                        items: star(),
                        from: Some((from, None)),
                        order_by: order(keys),
                        limit: Some(*n),
                        ..Select::default()
                    }));
                }
                let from = self.node(input);
                self.push(Query::select(Select {
                    items: star(),
                    from: Some((from, None)),
                    limit: Some(*n),
                    ..Select::default()
                }))
            }
        }
    }
}

fn order(keys: &[crate::semantic::hir::OrderKey]) -> Vec<OrderBy> {
    keys.iter()
        .map(|k| OrderBy {
            expr: expr(&k.expr),
            desc: k.desc,
        })
        .collect()
}

/// Lower a logical plan to one query (a chain of CTEs).
pub fn query(p: &LogicalPlan) -> Query {
    let mut b = Builder {
        ctes: Vec::new(),
        prefix: "__magi_step_".into(),
    };
    let top = b.node(p);
    // an ordered result stays ordered: repeat the ORDER BY on the outer SELECT
    let order_by = match p {
        LogicalPlan::Sort { keys, .. } => order(keys),
        _ => Vec::new(),
    };
    if let (TableRef::Named(name), Some((last, _))) = (&top, b.ctes.last())
        && name == last
        && order_by.is_empty()
    {
        // inline the final CTE
        let (_, q) = b.ctes.pop().unwrap();
        return Query {
            ctes: b.ctes,
            body: q.body,
        };
    }
    Query {
        ctes: b.ctes,
        body: Body::Select(Box::new(Select {
            items: vec![(Expr::Star { table: None }, None)],
            from: Some((top, None)),
            order_by,
            ..Select::default()
        })),
    }
}
