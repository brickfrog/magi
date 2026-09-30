//! MAGI-level optimisation. MAGI decides *what* runs and *where* results live;
//! once relations are inside DuckDB, DuckDB's optimizer handles projection pruning, filter
//! pushdown and join ordering inside every generated query.
//!
//! - Unused dataset elimination: only nodes that exports and validations need are executed.
//! - Materialization: a dataset read by exactly one other dataset (and nothing else) is inlined
//!   as a view; everything else is materialized once as a table.

use std::collections::{HashMap, HashSet};

use crate::plan::physical::{inputs, producers};
use crate::semantic::hir::{Hir, Materialization, Node};

/// Nodes that must run: everything exports and validations depend on. A program without
/// exports runs completely (a run that writes nothing is only useful for its checks).
pub fn needed(hir: &Hir) -> HashSet<Node> {
    let prod = producers(hir);
    let roots: Vec<Node> = if hir.order.iter().any(|n| matches!(n, Node::Export(_))) {
        hir.order
            .iter()
            .filter(|n| matches!(n, Node::Export(_) | Node::Validation(_)))
            .cloned()
            .collect()
    } else {
        hir.order.clone()
    };
    let mut needed = HashSet::new();
    let mut stack = roots;
    while let Some(n) = stack.pop() {
        if !needed.insert(n.clone()) {
            continue;
        }
        for input in inputs(hir, &n) {
            if let Some(p) = prod.get(&input) {
                stack.push(p.clone());
            }
        }
    }
    needed
}

/// How each needed dataset is materialized.
pub fn materialization(hir: &Hir, needed: &HashSet<Node>) -> HashMap<usize, Materialization> {
    let mut readers: HashMap<String, usize> = HashMap::new();
    let mut read_by_non_dataset: HashSet<String> = HashSet::new();
    for n in hir.order.iter().filter(|n| needed.contains(n)) {
        for input in inputs(hir, n) {
            *readers.entry(input.clone()).or_default() += 1;
            if !matches!(n, Node::Dataset(_)) {
                read_by_non_dataset.insert(input);
            }
        }
    }
    hir.datasets
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let inline = readers.get(&d.name) == Some(&1) && !read_by_non_dataset.contains(&d.name);
            (
                i,
                if inline {
                    Materialization::View
                } else {
                    Materialization::Table
                },
            )
        })
        .collect()
}
