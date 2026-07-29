// SPDX-License-Identifier: MIT
//! Generic graph traversal for the board's dependency/relation graph.
//!
//! The kanban's roadmap / critical-path / blockers views walk the item
//! dependency graph with a breadth-first search over an adjacency list — a
//! generic graph algorithm implemented here over plain Arrow.

use arrow::array::{Array, ListArray, RecordBatch, StringArray};
use std::collections::{HashMap, HashSet, VecDeque};

/// Direction of traversal relative to a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Follow edges where the node is the source (find its targets/dependencies).
    Forward,
    /// Follow edges where the node is the target (find its sources/blockers).
    Reverse,
}

/// A node reached during traversal, with its BFS depth from the start node.
#[derive(Debug, Clone)]
pub struct TraversalNode {
    pub id: String,
    pub depth: usize,
}

/// Build an adjacency map from a RecordBatch whose `list_col` is a `ListArray`
/// of target IDs per row (e.g. an item's `depends_on` / `related` list).
///
/// `Forward` maps `id → [targets]`; `Reverse` maps each `target → [id]`.
/// Rows with a null list are skipped. Repeated calls can accumulate into the
/// same map by merging the returned entries.
pub fn build_adjacency_from_list(
    batch: &RecordBatch,
    id_col: usize,
    list_col: usize,
    direction: Direction,
) -> HashMap<String, Vec<String>> {
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();

    if batch.num_rows() == 0 {
        return adj;
    }

    let Some(ids) = batch.column(id_col).as_any().downcast_ref::<StringArray>() else {
        return adj;
    };
    let Some(lists) = batch.column(list_col).as_any().downcast_ref::<ListArray>() else {
        return adj;
    };

    for i in 0..batch.num_rows() {
        if lists.is_null(i) {
            continue;
        }
        let values = lists.value(i);
        let Some(str_arr) = values.as_any().downcast_ref::<StringArray>() else {
            continue;
        };

        let id = ids.value(i);
        for j in 0..str_arr.len() {
            if str_arr.is_null(j) {
                continue;
            }
            let dep = str_arr.value(j);
            match direction {
                Direction::Forward => {
                    adj.entry(id.to_string()).or_default().push(dep.to_string());
                }
                Direction::Reverse => {
                    adj.entry(dep.to_string()).or_default().push(id.to_string());
                }
            }
        }
    }

    adj
}

/// Breadth-first traversal from `start_id` over a pre-built adjacency map, up to
/// `max_depth` hops.
///
/// Returns reached nodes in breadth-first order, **excluding** the start node,
/// each visited at most once (shortest-path depth).
pub fn bfs_with_adjacency(
    start_id: &str,
    adj: &HashMap<String, Vec<String>>,
    max_depth: usize,
) -> Vec<TraversalNode> {
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(start_id.to_string());
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();
    queue.push_back((start_id.to_string(), 0));
    let mut result: Vec<TraversalNode> = Vec::new();

    while let Some((current, depth)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }
        if let Some(neighbors) = adj.get(&current) {
            for neighbor in neighbors {
                if visited.insert(neighbor.clone()) {
                    result.push(TraversalNode {
                        id: neighbor.clone(),
                        depth: depth + 1,
                    });
                    queue.push_back((neighbor.clone(), depth + 1));
                }
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{ListBuilder, StringBuilder};
    use arrow::datatypes::{DataType, Field, Schema};
    use std::sync::Arc;

    /// Build a two-column batch: `id` (Utf8) + `deps` (List<Utf8>).
    fn make_items(rows: &[(&str, &[&str])]) -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Utf8, false),
            Field::new(
                "deps",
                DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
                true,
            ),
        ]));
        let ids: Vec<&str> = rows.iter().map(|(id, _)| *id).collect();
        let mut list_builder = ListBuilder::new(StringBuilder::new());
        for (_, deps) in rows {
            for d in *deps {
                list_builder.values().append_value(d);
            }
            list_builder.append(true);
        }
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(StringArray::from(ids)),
                Arc::new(list_builder.finish()),
            ],
        )
        .expect("build items batch")
    }

    #[test]
    fn forward_adjacency_and_bfs_chain() {
        // A -> B -> C ; A -> D
        let batch = make_items(&[("A", &["B", "D"]), ("B", &["C"]), ("C", &[]), ("D", &[])]);
        let adj = build_adjacency_from_list(&batch, 0, 1, Direction::Forward);
        let chain = bfs_with_adjacency("A", &adj, 10);
        let ids: Vec<&str> = chain.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"B") && ids.contains(&"C") && ids.contains(&"D"));
        // C is two hops from A.
        let c = chain.iter().find(|n| n.id == "C").expect("C reached");
        assert_eq!(c.depth, 2);
    }

    #[test]
    fn reverse_adjacency_finds_blockers() {
        // A depends on B: forward A->B; reverse gives B->A (B blocks A).
        let batch = make_items(&[("A", &["B"]), ("B", &[])]);
        let adj = build_adjacency_from_list(&batch, 0, 1, Direction::Reverse);
        let chain = bfs_with_adjacency("B", &adj, 10);
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].id, "A");
    }

    #[test]
    fn max_depth_bounds_traversal() {
        // A -> B -> C -> D, depth 1 sees only B.
        let batch = make_items(&[("A", &["B"]), ("B", &["C"]), ("C", &["D"]), ("D", &[])]);
        let adj = build_adjacency_from_list(&batch, 0, 1, Direction::Forward);
        let chain = bfs_with_adjacency("A", &adj, 1);
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].id, "B");
    }

    #[test]
    fn cycle_is_visited_once() {
        // A -> B -> A (cycle) must terminate and visit each once.
        let batch = make_items(&[("A", &["B"]), ("B", &["A"])]);
        let adj = build_adjacency_from_list(&batch, 0, 1, Direction::Forward);
        let chain = bfs_with_adjacency("A", &adj, 100);
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].id, "B");
    }

    #[test]
    fn empty_batch_yields_empty_adjacency() {
        let batch = make_items(&[]);
        let adj = build_adjacency_from_list(&batch, 0, 1, Direction::Forward);
        assert!(adj.is_empty());
        assert!(bfs_with_adjacency("X", &adj, 10).is_empty());
    }
}
