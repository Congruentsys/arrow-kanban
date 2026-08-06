// SPDX-License-Identifier: MIT
//! Dependency graph relation handlers.

use super::error_response;
use crate::engine::KanbanReply;
use arrow_kanban::relations::RelationsStore;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct RelationAddRequest {
    source_id: String,
    target_id: String,
    predicate: String,
}

pub(crate) fn handle_relation_add_typed(
    req: RelationAddRequest,
    relations: &mut RelationsStore,
) -> Result<KanbanReply, Vec<u8>> {
    let rel_id = relations
        .add_relation(&req.source_id, &req.target_id, &req.predicate)
        .map_err(|e| error_response(&format!("{e}"), "RELATION_ADD_FAILED"))?;

    Ok(KanbanReply::Value(serde_json::json!({
        "relation_id": rel_id,
        "source": req.source_id,
        "target": req.target_id,
        "predicate": req.predicate,
    })))
}

// ── Relation Remove (CH-7319) ───────────────────────────────────────────────
//
// The deletion half `relation.add` shipped without. Measured need: a FALSE
// seeded edge (a pre-fix citation-scrape `leavesRemainder`) was UNDELETABLE
// through every surface — `--unrelate` requires the edge's SOURCE as the
// update subject (a proposal never is), and re-seeding replaces only with a
// non-empty seed set. This verb is the general fix: exact-triple removal.

#[derive(Clone, Serialize, Deserialize)]
pub struct RelationRemoveRequest {
    source_id: String,
    target_id: String,
    predicate: String,
}

pub(crate) fn handle_relation_remove_typed(
    req: RelationRemoveRequest,
    relations: &mut RelationsStore,
) -> Result<KanbanReply, Vec<u8>> {
    relations
        .remove_relation(&req.source_id, &req.target_id, &req.predicate)
        .map_err(|e| error_response(&format!("{e}"), "RELATION_REMOVE_FAILED"))?;

    Ok(KanbanReply::Value(serde_json::json!({
        "removed": true,
        "source": req.source_id,
        "target": req.target_id,
        "predicate": req.predicate,
    })))
}

// ── Relation Query ──────────────────────────────────────────────────────────

#[derive(Clone, Deserialize)]
pub struct RelationQueryRequest {
    item_id: String,
}

pub(crate) fn handle_relation_query_typed(
    req: RelationQueryRequest,
    relations: &RelationsStore,
) -> Result<KanbanReply, Vec<u8>> {
    // Issue #26: return THE EDGES, not a tally. A bare count is compatible with
    // an edge to anything, of any kind — the agent that just wrote an edge could
    // not confirm what it wrote (and burned 4 turns trying). `count` stays for
    // existing consumers.
    let results = relations.query_relations(&req.item_id);
    let edges: Vec<serde_json::Value> = results.iter().flat_map(relation_rows).collect();

    Ok(KanbanReply::Value(serde_json::json!({
        "item_id": req.item_id,
        "count": edges.len(),
        "relations": edges,
    })))
}

/// The non-deleted rows of a relations batch as `{relation_id, source, target, predicate}`.
/// (`query_relations` already filters deleted rows and slices per-hit, but reading the
/// flag here keeps this correct for any caller handing in a raw batch.)
fn relation_rows(batch: &arrow::array::RecordBatch) -> Vec<serde_json::Value> {
    use arrow::array::{Array, BooleanArray, StringArray};
    use arrow_kanban::schema::rel_col;
    let col = |i: usize| {
        batch
            .column(i)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("relations string column")
    };
    let (ids, sources, targets, predicates) = (
        col(rel_col::RELATION_ID),
        col(rel_col::SOURCE_ID),
        col(rel_col::TARGET_ID),
        col(rel_col::PREDICATE),
    );
    let deleted = batch
        .column(rel_col::DELETED)
        .as_any()
        .downcast_ref::<BooleanArray>()
        .expect("deleted column");
    (0..batch.num_rows())
        .filter(|&i| !deleted.value(i))
        .map(|i| {
            serde_json::json!({
                "relation_id": ids.value(i),
                "source": sources.value(i),
                "target": targets.value(i),
                "predicate": predicates.value(i),
            })
        })
        .collect()
}
