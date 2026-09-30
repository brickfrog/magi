//! Logical plan IR: relational computation only. Pipelines and reconciliation both lower into
//! these operators; the DuckDB backend turns them into SQL.

use std::fmt::Write as _;

use crate::semantic::hir::{OrderKey, TExpr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinType {
    Inner,
    Left,
    Right,
    Full,
    /// Keep left rows that have a match (no right columns).
    Semi,
    /// Keep left rows that have no match (no right columns).
    Anti,
}

impl JoinType {
    pub fn name(self) -> &'static str {
        match self {
            JoinType::Inner => "inner",
            JoinType::Left => "left",
            JoinType::Right => "right",
            JoinType::Full => "full",
            JoinType::Semi => "semi",
            JoinType::Anti => "anti",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LogicalPlan {
    /// Read a materialized relation (MAGI relation name or internal table).
    Scan {
        table: String,
    },
    /// Compute exactly these output columns (expressions over input slot 0).
    Project {
        input: Box<LogicalPlan>,
        exprs: Vec<(TExpr, String)>,
    },
    Filter {
        input: Box<LogicalPlan>,
        predicate: TExpr,
    },
    /// `output` expressions reference slot 0 (left) and slot 1 (right). Semi/anti joins output
    /// the left columns unchanged and ignore `output`.
    Join {
        left: Box<LogicalPlan>,
        right: Box<LogicalPlan>,
        kind: JoinType,
        on: Option<TExpr>,
        output: Vec<(TExpr, String)>,
    },
    Aggregate {
        input: Box<LogicalPlan>,
        group: Vec<(TExpr, String)>,
        aggs: Vec<(TExpr, String)>,
    },
    /// All input columns plus these window-function columns.
    Window {
        input: Box<LogicalPlan>,
        exprs: Vec<(TExpr, String)>,
    },
    /// Bag union matched by column name.
    Union {
        inputs: Vec<LogicalPlan>,
    },
    Distinct {
        input: Box<LogicalPlan>,
    },
    Sort {
        input: Box<LogicalPlan>,
        keys: Vec<OrderKey>,
    },
    Limit {
        input: Box<LogicalPlan>,
        n: u64,
    },
    /// `native_sql` escape hatch (DuckDB dialect).
    NativeSql {
        sql: String,
    },
}

impl LogicalPlan {
    pub fn scan(table: impl Into<String>) -> Self {
        LogicalPlan::Scan {
            table: table.into(),
        }
    }
    pub fn project(self, exprs: Vec<(TExpr, String)>) -> Self {
        LogicalPlan::Project {
            input: Box::new(self),
            exprs,
        }
    }
    pub fn filter(self, predicate: TExpr) -> Self {
        LogicalPlan::Filter {
            input: Box::new(self),
            predicate,
        }
    }
    pub fn join(
        self,
        right: LogicalPlan,
        kind: JoinType,
        on: Option<TExpr>,
        output: Vec<(TExpr, String)>,
    ) -> Self {
        LogicalPlan::Join {
            left: Box::new(self),
            right: Box::new(right),
            kind,
            on,
            output,
        }
    }
    pub fn semi(self, right: LogicalPlan, on: TExpr) -> Self {
        self.join(right, JoinType::Semi, Some(on), Vec::new())
    }
    pub fn anti(self, right: LogicalPlan, on: TExpr) -> Self {
        self.join(right, JoinType::Anti, Some(on), Vec::new())
    }
    pub fn aggregate(self, group: Vec<(TExpr, String)>, aggs: Vec<(TExpr, String)>) -> Self {
        LogicalPlan::Aggregate {
            input: Box::new(self),
            group,
            aggs,
        }
    }
    pub fn window(self, exprs: Vec<(TExpr, String)>) -> Self {
        LogicalPlan::Window {
            input: Box::new(self),
            exprs,
        }
    }
    pub fn distinct(self) -> Self {
        LogicalPlan::Distinct {
            input: Box::new(self),
        }
    }
    pub fn sort(self, keys: Vec<OrderKey>) -> Self {
        LogicalPlan::Sort {
            input: Box::new(self),
            keys,
        }
    }

    /// Indented operator tree for `magi explain`.
    pub fn explain(&self) -> String {
        let mut out = String::new();
        self.explain_into(&mut out, 0);
        out
    }

    fn explain_into(&self, out: &mut String, depth: usize) {
        let pad = "  ".repeat(depth);
        let list = |xs: &[(TExpr, String)]| {
            xs.iter()
                .map(|(e, n)| {
                    let s = crate::plan::display::expr(e);
                    if s == format!("#{n}") || s == *n {
                        n.clone()
                    } else {
                        format!("{n} = {s}")
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        match self {
            LogicalPlan::Scan { table } => {
                let _ = writeln!(out, "{pad}Scan {table}");
            }
            LogicalPlan::Project { input, exprs } => {
                let _ = writeln!(out, "{pad}Project [{}]", list(exprs));
                input.explain_into(out, depth + 1);
            }
            LogicalPlan::Filter { input, predicate } => {
                let _ = writeln!(out, "{pad}Filter {}", crate::plan::display::expr(predicate));
                input.explain_into(out, depth + 1);
            }
            LogicalPlan::Join {
                left,
                right,
                kind,
                on,
                output,
            } => {
                let on = on
                    .as_ref()
                    .map(crate::plan::display::expr)
                    .unwrap_or_else(|| "true".into());
                if matches!(kind, JoinType::Semi | JoinType::Anti) {
                    let _ = writeln!(out, "{pad}Join {} on {on}", kind.name());
                } else {
                    let _ = writeln!(
                        out,
                        "{pad}Join {} on {on} -> [{}]",
                        kind.name(),
                        list(output)
                    );
                }
                left.explain_into(out, depth + 1);
                right.explain_into(out, depth + 1);
            }
            LogicalPlan::Aggregate { input, group, aggs } => {
                let _ = writeln!(
                    out,
                    "{pad}Aggregate by [{}] compute [{}]",
                    list(group),
                    list(aggs)
                );
                input.explain_into(out, depth + 1);
            }
            LogicalPlan::Window { input, exprs } => {
                let _ = writeln!(out, "{pad}Window [{}]", list(exprs));
                input.explain_into(out, depth + 1);
            }
            LogicalPlan::Union { inputs } => {
                let _ = writeln!(out, "{pad}Union (by name)");
                for i in inputs {
                    i.explain_into(out, depth + 1);
                }
            }
            LogicalPlan::Distinct { input } => {
                let _ = writeln!(out, "{pad}Distinct");
                input.explain_into(out, depth + 1);
            }
            LogicalPlan::Sort { input, keys } => {
                let keys = keys
                    .iter()
                    .map(|k| {
                        format!(
                            "{} {}",
                            crate::plan::display::expr(&k.expr),
                            if k.desc { "desc" } else { "asc" }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = writeln!(out, "{pad}Sort [{keys}]");
                input.explain_into(out, depth + 1);
            }
            LogicalPlan::Limit { input, n } => {
                let _ = writeln!(out, "{pad}Limit {n}");
                input.explain_into(out, depth + 1);
            }
            LogicalPlan::NativeSql { .. } => {
                let _ = writeln!(out, "{pad}NativeSql (DuckDB-specific)");
            }
        }
    }
}
