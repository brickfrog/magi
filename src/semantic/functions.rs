//! MAGI's deliberately small portable function library: signatures and result
//! types. Backend lowering lives in `backend::duckdb`. Backend-only functions are reachable
//! through the `duckdb.` namespace and are untyped.
//!
//! Functions that need literal arguments or mapping names (`parse_date`, `to_decimal`, `map`,
//! `replace_words`, `date`, `today`) are handled by the resolver before these generic rules.

use crate::semantic::hir::AggFunc;
use crate::semantic::types::{ColType, Type};

pub enum Resolved {
    Scalar(&'static str, ColType),
    Agg(AggFunc, ColType),
}

pub struct FnError {
    pub message: String,
    /// Argument the error is about, if any.
    pub arg: Option<usize>,
}

fn err(message: impl Into<String>, arg: Option<usize>) -> FnError {
    FnError {
        message: message.into(),
        arg,
    }
}

/// Every callable name (for "did you mean").
pub const ALL: &[&str] = &[
    "lower",
    "upper",
    "trim",
    "strip_accents",
    "replace",
    "regexp_replace",
    "regexp_extract",
    "regexp_matches",
    "contains",
    "starts_with",
    "ends_with",
    "length",
    "substr",
    "lpad",
    "rpad",
    "concat",
    "coalesce",
    "if",
    "nullif",
    "abs",
    "round",
    "floor",
    "ceil",
    "least",
    "greatest",
    "to_string",
    "to_int",
    "to_float",
    "to_decimal",
    "to_date",
    "to_bool",
    "parse_date",
    "parse_timestamp",
    "parse_number",
    "date",
    "today",
    "year",
    "month",
    "day",
    "quarter",
    "day_of_week",
    "days_between",
    "date_diff",
    "add_days",
    "similarity",
    "token_similarity",
    "levenshtein",
    "is_null",
    "map",
    "replace_words",
    "sum",
    "mean",
    "avg",
    "min",
    "max",
    "count",
    "count_distinct",
    "missing",
    "any",
    "all",
];

pub fn is_aggregate(name: &str) -> bool {
    matches!(
        name,
        "sum"
            | "mean"
            | "avg"
            | "min"
            | "max"
            | "count"
            | "count_distinct"
            | "missing"
            | "any"
            | "all"
    )
}

fn any_nullable(args: &[ColType]) -> bool {
    args.iter().any(|a| a.nullable)
}

fn arity(name: &str, args: &[ColType], min: usize, max: usize) -> Result<(), FnError> {
    if args.len() < min || args.len() > max {
        let expected = if min == max {
            format!("{min}")
        } else if max == usize::MAX {
            format!("at least {min}")
        } else {
            format!("{min} to {max}")
        };
        return Err(err(
            format!(
                "`{name}` takes {expected} argument{}, got {}",
                if expected == "1" { "" } else { "s" },
                args.len()
            ),
            None,
        ));
    }
    Ok(())
}

fn want(
    name: &str,
    args: &[ColType],
    i: usize,
    ok: impl Fn(Type) -> bool,
    what: &str,
) -> Result<(), FnError> {
    let t = args[i].ty;
    if t.is_unknown() || t == Type::Null || ok(t) {
        Ok(())
    } else {
        Err(err(
            format!("argument {} of `{name}` must be {what}, found {t}", i + 1),
            Some(i),
        ))
    }
}

fn string(t: Type) -> bool {
    t == Type::String
}
fn numeric(t: Type) -> bool {
    t.is_numeric()
}
fn int(t: Type) -> bool {
    t == Type::Int
}
fn temporal(t: Type) -> bool {
    t.is_temporal()
}
fn boolean(t: Type) -> bool {
    t == Type::Bool
}

fn unify_all(name: &str, args: &[ColType]) -> Result<Type, FnError> {
    let mut ty = Type::Null;
    for (i, a) in args.iter().enumerate() {
        ty = Type::unify(ty, a.ty).ok_or_else(|| {
            err(
                format!(
                    "arguments of `{name}` have incompatible types ({ty} and {})",
                    a.ty
                ),
                Some(i),
            )
        })?;
    }
    Ok(ty)
}

pub fn resolve(name: &str, args: &[ColType]) -> Result<Resolved, FnError> {
    use Resolved::{Agg, Scalar};
    let n = any_nullable(args);
    let s = |f: &'static str, ty: Type, nullable: bool| Ok(Scalar(f, ColType::new(ty, nullable)));
    match name {
        "lower" | "upper" | "trim" | "strip_accents" => {
            arity(name, args, 1, 1)?;
            want(name, args, 0, string, "a string")?;
            let f = match name {
                "lower" => "lower",
                "upper" => "upper",
                "strip_accents" => "strip_accents",
                _ => "trim",
            };
            s(f, Type::String, n)
        }
        "replace" | "regexp_replace" => {
            arity(name, args, 3, 3)?;
            for i in 0..3 {
                want(name, args, i, string, "a string")?;
            }
            s(
                if name == "replace" {
                    "replace"
                } else {
                    "regexp_replace"
                },
                Type::String,
                n,
            )
        }
        "regexp_extract" => {
            arity(name, args, 2, 3)?;
            want(name, args, 0, string, "a string")?;
            want(name, args, 1, string, "a string")?;
            if args.len() == 3 {
                want(name, args, 2, int, "an int")?;
            }
            // no match => null
            s("regexp_extract", Type::String, true)
        }
        "regexp_matches" | "contains" | "starts_with" | "ends_with" => {
            arity(name, args, 2, 2)?;
            want(name, args, 0, string, "a string")?;
            want(name, args, 1, string, "a string")?;
            let f = match name {
                "regexp_matches" => "regexp_matches",
                "contains" => "contains",
                "starts_with" => "starts_with",
                _ => "ends_with",
            };
            s(f, Type::Bool, n)
        }
        "length" => {
            arity(name, args, 1, 1)?;
            want(name, args, 0, string, "a string")?;
            s("length", Type::Int, n)
        }
        "substr" => {
            arity(name, args, 2, 3)?;
            want(name, args, 0, string, "a string")?;
            want(name, args, 1, int, "an int")?;
            if args.len() == 3 {
                want(name, args, 2, int, "an int")?;
            }
            s("substr", Type::String, n)
        }
        "lpad" | "rpad" => {
            arity(name, args, 3, 3)?;
            want(name, args, 0, string, "a string")?;
            want(name, args, 1, int, "an int")?;
            want(name, args, 2, string, "a string")?;
            s(
                if name == "lpad" { "lpad" } else { "rpad" },
                Type::String,
                n,
            )
        }
        "concat" => {
            arity(name, args, 1, usize::MAX)?;
            // null arguments are skipped, so the result is never null
            s("concat", Type::String, false)
        }
        "coalesce" => {
            arity(name, args, 1, usize::MAX)?;
            let ty = unify_all(name, args)?;
            s("coalesce", ty, args.iter().all(|a| a.nullable))
        }
        "if" => {
            arity(name, args, 3, 3)?;
            want(name, args, 0, boolean, "a bool")?;
            let ty =
                unify_all(name, &args[1..]).map_err(|e| err(e.message, e.arg.map(|i| i + 1)))?;
            s("if", ty, args[1].nullable || args[2].nullable)
        }
        "nullif" => {
            arity(name, args, 2, 2)?;
            let ty = unify_all(name, args)?;
            s("nullif", ty, true)
        }
        "abs" | "floor" | "ceil" => {
            arity(name, args, 1, 1)?;
            want(name, args, 0, numeric, "a number")?;
            let f = match name {
                "abs" => "abs",
                "floor" => "floor",
                _ => "ceil",
            };
            s(f, args[0].ty, n)
        }
        "round" => {
            arity(name, args, 1, 2)?;
            want(name, args, 0, numeric, "a number")?;
            if args.len() == 2 {
                want(name, args, 1, int, "an int")?;
            }
            s("round", args[0].ty, n)
        }
        "least" | "greatest" => {
            arity(name, args, 1, usize::MAX)?;
            let ty = unify_all(name, args)?;
            s(if name == "least" { "least" } else { "greatest" }, ty, n)
        }
        "to_string" => {
            arity(name, args, 1, 1)?;
            s("to_string", Type::String, n)
        }
        "to_int" => {
            arity(name, args, 1, 1)?;
            s("to_int", Type::Int, true)
        }
        "to_float" => {
            arity(name, args, 1, 1)?;
            s("to_float", Type::Float, true)
        }
        "to_date" => {
            arity(name, args, 1, 1)?;
            s("to_date", Type::Date, true)
        }
        "to_bool" => {
            arity(name, args, 1, 1)?;
            s("to_bool", Type::Bool, true)
        }
        "parse_number" => {
            arity(name, args, 1, 1)?;
            want(name, args, 0, string, "a string")?;
            s("parse_number", Type::Decimal(38, 6), true)
        }
        "year" | "month" | "day" | "quarter" | "day_of_week" => {
            arity(name, args, 1, 1)?;
            want(name, args, 0, temporal, "a date or timestamp")?;
            let f = match name {
                "year" => "year",
                "month" => "month",
                "day" => "day",
                "quarter" => "quarter",
                _ => "day_of_week",
            };
            s(f, Type::Int, n)
        }
        "days_between" | "date_diff" => {
            arity(name, args, 2, 2)?;
            want(name, args, 0, temporal, "a date or timestamp")?;
            want(name, args, 1, temporal, "a date or timestamp")?;
            s(
                if name == "days_between" {
                    "days_between"
                } else {
                    "date_diff"
                },
                Type::Int,
                n,
            )
        }
        "add_days" => {
            arity(name, args, 2, 2)?;
            want(name, args, 0, temporal, "a date or timestamp")?;
            want(name, args, 1, int, "an int")?;
            s("add_days", args[0].ty, n)
        }
        "similarity" | "token_similarity" => {
            arity(name, args, 2, 2)?;
            want(name, args, 0, string, "a string")?;
            want(name, args, 1, string, "a string")?;
            s(
                if name == "similarity" {
                    "similarity"
                } else {
                    "token_similarity"
                },
                Type::Float,
                n,
            )
        }
        "levenshtein" => {
            arity(name, args, 2, 2)?;
            want(name, args, 0, string, "a string")?;
            want(name, args, 1, string, "a string")?;
            s("levenshtein", Type::Int, n)
        }
        "is_null" => {
            arity(name, args, 1, 1)?;
            s("is_null", Type::Bool, false)
        }
        // ---- aggregates
        "sum" => {
            arity(name, args, 1, 1)?;
            want(name, args, 0, numeric, "a number")?;
            let ty = match args[0].ty {
                Type::Decimal(_, s) => Type::Decimal(38, s),
                Type::Null => Type::Int,
                t => t,
            };
            Ok(Agg(AggFunc::Sum, ColType::nullable(ty)))
        }
        "mean" | "avg" => {
            arity(name, args, 1, 1)?;
            want(name, args, 0, numeric, "a number")?;
            Ok(Agg(AggFunc::Mean, ColType::nullable(Type::Float)))
        }
        "min" | "max" => {
            arity(name, args, 1, 1)?;
            let f = if name == "min" {
                AggFunc::Min
            } else {
                AggFunc::Max
            };
            Ok(Agg(f, ColType::nullable(args[0].ty)))
        }
        "count" => {
            arity(name, args, 0, 1)?;
            Ok(Agg(AggFunc::Count, ColType::required(Type::Int)))
        }
        "count_distinct" => {
            arity(name, args, 1, 1)?;
            Ok(Agg(AggFunc::CountDistinct, ColType::required(Type::Int)))
        }
        "missing" => {
            arity(name, args, 1, 1)?;
            Ok(Agg(AggFunc::Missing, ColType::nullable(Type::Float)))
        }
        "any" | "all" => {
            arity(name, args, 1, 1)?;
            want(name, args, 0, boolean, "a bool")?;
            Ok(Agg(
                if name == "any" {
                    AggFunc::Any
                } else {
                    AggFunc::All
                },
                ColType::nullable(Type::Bool),
            ))
        }
        _ => Err(err(format!("unknown function `{name}`"), None)),
    }
}
