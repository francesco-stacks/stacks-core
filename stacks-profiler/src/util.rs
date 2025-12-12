use std::collections::HashMap;

use crate::{ProfileStats, SpanId};

/// Trait for analyzing and flattening profiling trees.
pub trait Flatten {
    /// Flattens the hierarchy into a list, aggregating stats by SpanId.
    ///
    /// This transforms a Tree View into a Flat/Bottom-Up View.
    /// * **Tree View:** Preserves context (e.g., "Tx1 -> Logic" is separate from "Tx2 -> Logic").
    /// * **Flat View:** Aggregates by function (e.g., "Logic" shows total time across all Txs).
    ///
    /// The resulting list is sorted by Wall Time (descending).
    fn flatten(&self) -> Vec<ProfileStats>;
}

impl Flatten for Vec<ProfileStats> {
    fn flatten(&self) -> Vec<ProfileStats> {
        let mut map: HashMap<SpanId, ProfileStats> = HashMap::new();

        // We use a stack of references to traverse without moving the originals
        let mut stack: Vec<&ProfileStats> = self.iter().collect();

        while let Some(node) = stack.pop() {
            // 1. Merge this node into the global map.
            // We must Clone the node because 'flatten' creates a new aggregated view
            // separate from the original tree.
            map.entry(node.id.clone())
                .and_modify(|existing| {
                    // We ignore the returned recycling vec because we are in analysis mode
                    let _ = existing.merge(node.clone());
                })
                .or_insert_with(|| node.clone());

            // 2. Add children to stack to visit them as well.
            // This ensures that children also become top-level entries in the flat list.
            for child in &node.children {
                stack.push(child);
            }
        }

        // Convert to sorted vec
        let mut flat_results: Vec<ProfileStats> = map.into_values().collect();
        // Sort by Wall Time descending
        flat_results.sort_by(|a, b| b.wall_time_ns.cmp(&a.wall_time_ns));
        flat_results
    }
}

impl Flatten for ProfileStats {
    fn flatten(&self) -> Vec<ProfileStats> {
        // Treat this single node as a root of a tree and flatten it.
        vec![self.clone()].flatten()
    }
}
