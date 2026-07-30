// SPDX-License-Identifier: MIT
//! Dependency graph relation handlers.

use super::error_response;
use crate::engine::KanbanReply;
use arrow_kanban::relations::RelationsStore;
use serde::Deserialize;

#[derive(Deserialize)]
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

// ── Relation Query ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RelationQueryRequest {
    item_id: String,
}

pub(crate) fn handle_relation_query_typed(
    req: RelationQueryRequest,
    relations: &RelationsStore,
) -> Result<KanbanReply, Vec<u8>> {
    let results = relations.query_relations(&req.item_id);

    Ok(KanbanReply::Value(serde_json::json!({
        "item_id": req.item_id,
        "count": results.iter().map(|b| b.num_rows()).sum::<usize>(),
    })))
}
