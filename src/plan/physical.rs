//! Physical planning: the ordered steps a run executes, after the optimisation decisions of
//! `plan::optimize` (MAGI decides where and what to execute; DuckDB decides how).

use std::collections::HashMap;

use crate::plan::optimize;
use crate::semantic::hir::{Hir, Materialization, Node};

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Stage {
        source: usize,
    },
    Materialize {
        dataset: usize,
        kind: Materialization,
    },
    Validate {
        validation: usize,
    },
    Reconcile {
        reconcile: usize,
    },
    Export {
        export: usize,
    },
}

#[derive(Debug, Clone, Default)]
pub struct PhysicalPlan {
    pub steps: Vec<Step>,
    /// Datasets that nothing needs (not executed).
    pub unused: Vec<String>,
}

/// Relation name -> the node that produces it.
pub fn producers(hir: &Hir) -> HashMap<String, Node> {
    let mut m = HashMap::new();
    for (i, s) in hir.sources.iter().enumerate() {
        m.insert(s.name.clone(), Node::Source(i));
        m.insert(format!("{}.rejects", s.name), Node::Source(i));
    }
    for (i, d) in hir.datasets.iter().enumerate() {
        m.insert(d.name.clone(), Node::Dataset(i));
    }
    for (i, r) in hir.reconciles.iter().enumerate() {
        for part in crate::reconcile::model::PARTS {
            m.insert(format!("{}.{part}", r.name), Node::Reconcile(i));
        }
    }
    for (i, v) in hir.validations.iter().enumerate() {
        m.insert(format!("{}.failures", v.target), Node::Validation(i));
        m.insert(format!("{}.checks", v.target), Node::Validation(i));
    }
    m
}

/// Relations a node reads.
pub fn inputs(hir: &Hir, node: &Node) -> Vec<String> {
    match node {
        Node::Source(_) => Vec::new(),
        Node::Dataset(i) => hir.datasets[*i].uses.clone(),
        Node::Validation(i) => vec![hir.validations[*i].target.clone()],
        Node::Reconcile(i) => {
            let r = &hir.reconciles[*i];
            vec![r.a.relation.clone(), r.b.relation.clone()]
        }
        Node::Export(i) => hir.exports[*i]
            .parts
            .iter()
            .map(|p| p.relation.clone())
            .collect(),
    }
}

pub fn build(hir: &Hir) -> PhysicalPlan {
    let needed = optimize::needed(hir);
    let kinds = optimize::materialization(hir, &needed);
    let mut steps = Vec::new();
    let mut unused = Vec::new();
    for n in &hir.order {
        if !needed.contains(n) {
            if let Node::Dataset(i) = n {
                unused.push(hir.datasets[*i].name.clone());
            }
            continue;
        }
        steps.push(match n {
            Node::Source(i) => Step::Stage { source: *i },
            Node::Dataset(i) => Step::Materialize {
                dataset: *i,
                kind: kinds[i],
            },
            Node::Validation(i) => Step::Validate { validation: *i },
            Node::Reconcile(i) => Step::Reconcile { reconcile: *i },
            Node::Export(i) => Step::Export { export: *i },
        });
    }
    PhysicalPlan { steps, unused }
}

/// Human-readable execution plan for `magi plan`.
pub fn describe(hir: &Hir, plan: &PhysicalPlan) -> Vec<String> {
    let mut lines = Vec::new();
    for step in &plan.steps {
        match step {
            Step::Stage { source } => {
                let s = &hir.sources[*source];
                let contract = match &s.declared {
                    Some(cols) => format!("; apply declared schema ({} columns)", cols.len()),
                    None => "; infer column types".to_string(),
                };
                lines.push(format!(
                    "Load source {} from {}{contract}",
                    s.name,
                    s.kind.describe()
                ));
            }
            Step::Materialize { dataset, kind } => {
                let d = &hir.datasets[*dataset];
                let how = match kind {
                    Materialization::Table => "materialize as table",
                    Materialization::View => "inline as view",
                };
                let mut line = format!(
                    "Build dataset {} from {} ({how})",
                    d.name,
                    d.uses.join(", ")
                );
                if d.backend_specific {
                    line += " [DuckDB-specific SQL]";
                }
                lines.push(line);
            }
            Step::Validate { validation } => {
                let v = &hir.validations[*validation];
                let (mut r, mut e, mut w) = (0, 0, 0);
                for c in &v.checks {
                    match c.severity {
                        crate::ast::CheckSeverity::Require => r += 1,
                        crate::ast::CheckSeverity::Expect => e += 1,
                        crate::ast::CheckSeverity::Warn => w += 1,
                    }
                }
                lines.push(format!("Validate {} ({r} required, {e} expected, {w} warning checks; a failed `require` stops the run)", v.target));
            }
            Step::Reconcile { reconcile } => {
                let r = &hir.reconciles[*reconcile];
                lines.push(format!(
                    "Reconcile {} = {} with {} ({}, consume {}, identity {} / {})",
                    r.name,
                    r.a.relation,
                    r.b.relation,
                    r.cardinality.name(),
                    r.consume.name(),
                    identity_text(&r.a),
                    identity_text(&r.b),
                ));
                use crate::reconcile::model::{Cardinality, Consume};
                let after = match r.consume {
                    Consume::Both => "matched rows leave later tiers",
                    Consume::A => "matched A rows leave later tiers",
                    Consume::B => "matched B rows leave later tiers",
                    Consume::None => {
                        "rows stay available to later tiers (a matched pair is not matched again)"
                    }
                };
                let rounds = if r.cardinality == Cardinality::OneToOne {
                    " (repeated until stable: one round per link of a preference chain)"
                } else {
                    ""
                };
                for (i, t) in r.tiers.iter().enumerate() {
                    let select = if let Some(sub) = &t.subset {
                        let side = if sub.slot == 0 { "a" } else { "b" };
                        let other = if sub.slot == 0 { "b" } else { "a" };
                        let unit = if t.is_rollup() { "group" } else { "row" };
                        if let Some(st) = sub.sum_target() {
                            format!(
                                "enumerate subsets of up to {} {side} rows per {other} {unit}, \
                                 extending a subset only while `{}` can still hold (by the \
                                 largest and smallest sums that members after its last one can \
                                 add in the places it has left); the run stops \
                                 when the subsets examined exceed max_subsets {}; a unit matches \
                                 its single best subset, units whose best subsets tie or share a \
                                 row are reported as ambiguous",
                                sub.max_items, sub.requires[st.require].text, sub.max_subsets,
                            )
                        } else {
                            format!(
                                "enumerate subsets of up to {} {side} rows per {other} {unit} \
                                 (a unit with n members has at most C(n, 1) + ... + C(n, {}) of them; \
                                 the run stops before enumerating when all units together have more \
                                 than max_subsets {}, i.e. one unit may have at most {} members); \
                                 a unit matches its single best subset, units whose best subsets tie \
                                 or share a row are reported as ambiguous",
                                sub.max_items,
                                sub.max_items,
                                sub.max_subsets,
                                crate::reconcile::lower::subset::largest_unit(
                                    sub.max_items,
                                    sub.max_subsets
                                ),
                            )
                        }
                    } else if t.is_rollup() {
                        format!("select one-to-one group matches{rounds}")
                    } else if t.rank.is_empty() {
                        format!(
                            "select {}{rounds}; equally good candidates are reported as ambiguous",
                            r.cardinality.name()
                        )
                    } else {
                        format!(
                            "select {} by rank{rounds}; ties are reported as ambiguous",
                            r.cardinality.name()
                        )
                    };
                    lines.push(format!(
                        "  tier {} {}: generate candidates ({}); {select}; {after}",
                        i + 1,
                        t.name,
                        t.description().split_once(": ").map(|x| x.1).unwrap_or("")
                    ));
                }
                lines.push(format!("  produce {}.matches, .unmatched_a, .unmatched_b, .candidates, .ambiguous, .summary", r.name));
            }
            Step::Export { export } => {
                let e = &hir.exports[*export];
                let rels: Vec<String> = e.parts.iter().map(|p| p.relation.clone()).collect();
                lines.push(format!("Export {} to {}", rels.join(", "), e.display_path));
            }
        }
    }
    lines
}

fn identity_text(side: &crate::reconcile::model::Side) -> String {
    match &side.identity {
        crate::reconcile::model::Identity::Declared(ids) => ids.join("+"),
        crate::reconcile::model::Identity::Synthetic => "synthetic".into(),
    }
}
