//! Column lineage: where a column's values come from, derived from the
//! expression DAG recorded during analysis.

use std::collections::HashSet;

use crate::semantic::hir::{Lineage, LineageId};

/// Render the lineage of one column as an indented tree. Shared sub-trees are printed once;
/// later occurrences are marked `(see above)`.
pub fn render(lineage: &Lineage, root: LineageId) -> String {
    let mut out = String::new();
    let mut seen = HashSet::new();
    walk(lineage, root, 0, &mut seen, &mut out);
    out
}

fn walk(
    lineage: &Lineage,
    id: LineageId,
    depth: usize,
    seen: &mut HashSet<LineageId>,
    out: &mut String,
) {
    let node = &lineage.nodes[id];
    let pad = if depth == 0 {
        String::new()
    } else {
        format!("{}└─ ", "   ".repeat(depth - 1))
    };
    if !seen.insert(id) && !node.deps.is_empty() {
        out.push_str(&format!("{pad}{} (see above)\n", node.label));
        return;
    }
    out.push_str(&format!("{pad}{}\n", node.label));
    for &d in &node.deps {
        walk(lineage, d, depth + 1, seen, out);
    }
}
