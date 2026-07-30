// SPDX-License-Identifier: MIT
//! Relations — cross-item and cross-board links.
//!
//! Supports predicates: implements, spawns, blocks, related_to.

use crate::schema::{rel_col, relations_schema};
use arrow::array::{Array, BooleanArray, RecordBatch, StringArray, TimestampMillisecondArray};
use std::sync::Arc;

/// Errors from relation operations.
#[derive(Debug, thiserror::Error)]
pub enum RelationError {
    #[error("Arrow error: {0}")]
    Arrow(#[from] arrow::error::ArrowError),

    #[error("Relation not found: {0} → {1} ({2})")]
    NotFound(String, String, String),
}

pub type Result<T> = std::result::Result<T, RelationError>;

/// The relations store — holds relation RecordBatches.
///
/// `Clone` is an O(#batches) Arc-pointer copy: the batches' columns are `Arc`'d
/// Arrow arrays (clone bumps refcounts, never copies data) and the schema is an
/// `Arc`. This lets an [`AggregateSnapshot`](../../arrow_kanban_server/snapshot/struct.AggregateSnapshot.html)
/// bind the relations table to a point-in-time seq cheaply.
#[derive(Clone)]
pub struct RelationsStore {
    batches: Vec<RecordBatch>,
    schema: Arc<arrow::datatypes::Schema>,
}

impl RelationsStore {
    pub fn new() -> Self {
        RelationsStore {
            batches: Vec::new(),
            schema: relations_schema(),
        }
    }

    /// Load pre-built relation batches (e.g., from migration or Parquet).
    pub fn load(&mut self, batches: Vec<RecordBatch>) {
        self.batches.extend(batches);
    }

    /// Add a relation between two items.
    pub fn add_relation(
        &mut self,
        source_id: &str,
        target_id: &str,
        predicate: &str,
    ) -> Result<String> {
        let rel_id = uuid::Uuid::new_v4().to_string();
        let now_ms = chrono::Utc::now().timestamp_millis();

        let batch = RecordBatch::try_new(
            self.schema.clone(),
            vec![
                Arc::new(StringArray::from(vec![rel_id.as_str()])),
                Arc::new(StringArray::from(vec![source_id])),
                Arc::new(StringArray::from(vec![target_id])),
                Arc::new(StringArray::from(vec![predicate])),
                Arc::new(TimestampMillisecondArray::from(vec![now_ms]).with_timezone("UTC")),
                Arc::new(BooleanArray::from(vec![false])),
            ],
        )?;

        self.batches.push(batch);
        Ok(rel_id)
    }

    /// Remove a relation (logical delete).
    pub fn remove_relation(
        &mut self,
        source_id: &str,
        target_id: &str,
        predicate: &str,
    ) -> Result<()> {
        for (batch_idx, batch) in self.batches.iter().enumerate() {
            let sources = batch
                .column(rel_col::SOURCE_ID)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("source_id");
            let targets = batch
                .column(rel_col::TARGET_ID)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("target_id");
            let predicates = batch
                .column(rel_col::PREDICATE)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("predicate");
            let deleted = batch
                .column(rel_col::DELETED)
                .as_any()
                .downcast_ref::<BooleanArray>()
                .expect("deleted");

            for i in 0..batch.num_rows() {
                if sources.value(i) == source_id
                    && targets.value(i) == target_id
                    && predicates.value(i) == predicate
                    && !deleted.value(i)
                {
                    // Rebuild with deleted=true for this row
                    let mut columns: Vec<Arc<dyn Array>> = Vec::new();
                    for col_idx in 0..batch.num_columns() {
                        if col_idx == rel_col::DELETED {
                            let mut new_deleted: Vec<bool> =
                                (0..batch.num_rows()).map(|j| deleted.value(j)).collect();
                            new_deleted[i] = true;
                            columns.push(Arc::new(BooleanArray::from(new_deleted)));
                        } else {
                            columns.push(batch.column(col_idx).clone());
                        }
                    }
                    let new_batch = RecordBatch::try_new(self.schema.clone(), columns)?;
                    self.batches[batch_idx] = new_batch;
                    return Ok(());
                }
            }
        }

        Err(RelationError::NotFound(
            source_id.to_string(),
            target_id.to_string(),
            predicate.to_string(),
        ))
    }

    /// Whether a non-deleted edge `source --predicate--> target` already exists.
    ///
    /// Used by crash-recovery replay to make replaying a committed `relation.add`
    /// idempotent: a checkpoint marker that trails the parquet by one commit can
    /// re-present an already-checkpointed edge, and [`add_relation`](Self::add_relation)
    /// appends a fresh row every time, so a naive replay would double the edge.
    pub fn has_relation(&self, source_id: &str, target_id: &str, predicate: &str) -> bool {
        for batch in &self.batches {
            let sources = batch
                .column(rel_col::SOURCE_ID)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("source_id");
            let targets = batch
                .column(rel_col::TARGET_ID)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("target_id");
            let predicates = batch
                .column(rel_col::PREDICATE)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("predicate");
            let deleted = batch
                .column(rel_col::DELETED)
                .as_any()
                .downcast_ref::<BooleanArray>()
                .expect("deleted");
            for i in 0..batch.num_rows() {
                if !deleted.value(i)
                    && sources.value(i) == source_id
                    && targets.value(i) == target_id
                    && predicates.value(i) == predicate
                {
                    return true;
                }
            }
        }
        false
    }

    /// Query all relations for an item (as source OR target).
    pub fn query_relations(&self, item_id: &str) -> Vec<RecordBatch> {
        let mut results = Vec::new();

        for batch in &self.batches {
            let sources = batch
                .column(rel_col::SOURCE_ID)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("source_id");
            let targets = batch
                .column(rel_col::TARGET_ID)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("target_id");
            let deleted = batch
                .column(rel_col::DELETED)
                .as_any()
                .downcast_ref::<BooleanArray>()
                .expect("deleted");

            for i in 0..batch.num_rows() {
                if deleted.value(i) {
                    continue;
                }
                if sources.value(i) == item_id || targets.value(i) == item_id {
                    results.push(batch.slice(i, 1));
                }
            }
        }

        results
    }

    /// Source IDs of every non-deleted edge `source --predicate--> target_id` (edges pointing AT
    /// `target_id` with exactly this predicate), sorted + deduped. A campaign's member
    /// voyages are the sources of `partOf` edges to it — `incoming_by_predicate(campaign_id, "partOf")`.
    pub fn incoming_by_predicate(&self, target_id: &str, predicate: &str) -> Vec<String> {
        let mut sources = Vec::new();
        for batch in &self.batches {
            let src = batch
                .column(rel_col::SOURCE_ID)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("source_id");
            let tgt = batch
                .column(rel_col::TARGET_ID)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("target_id");
            let pred = batch
                .column(rel_col::PREDICATE)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("predicate");
            let deleted = batch
                .column(rel_col::DELETED)
                .as_any()
                .downcast_ref::<BooleanArray>()
                .expect("deleted");
            for i in 0..batch.num_rows() {
                if !deleted.value(i) && tgt.value(i) == target_id && pred.value(i) == predicate {
                    sources.push(src.value(i).to_string());
                }
            }
        }
        sources.sort();
        sources.dedup();
        sources
    }

    /// Access the relations schema.
    pub fn schema(&self) -> &arrow::datatypes::Schema {
        &self.schema
    }

    /// Access the underlying relation batches.
    pub fn batches(&self) -> &[RecordBatch] {
        &self.batches
    }

    /// Count active (non-deleted) relations.
    pub fn active_count(&self) -> usize {
        let mut count = 0;
        for batch in &self.batches {
            let deleted = batch
                .column(rel_col::DELETED)
                .as_any()
                .downcast_ref::<BooleanArray>()
                .expect("deleted");
            for i in 0..batch.num_rows() {
                if !deleted.value(i) {
                    count += 1;
                }
            }
        }
        count
    }
}

impl Default for RelationsStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_and_query_relation() {
        let mut store = RelationsStore::new();
        store
            .add_relation("EXP-1257", "VOY-145", "implements")
            .unwrap();

        let rels = store.query_relations("EXP-1257");
        assert_eq!(rels.len(), 1);

        // Also findable from target side
        let rels = store.query_relations("VOY-145");
        assert_eq!(rels.len(), 1);
    }

    #[test]
    fn test_multiple_relations() {
        let mut store = RelationsStore::new();
        store
            .add_relation("EXP-1257", "VOY-145", "implements")
            .unwrap();
        store
            .add_relation("EXP-1257", "EXP-1258", "blocks")
            .unwrap();
        store
            .add_relation("EXP-1260", "VOY-145", "implements")
            .unwrap();

        let rels = store.query_relations("EXP-1257");
        assert_eq!(rels.len(), 2); // implements + blocks

        let rels = store.query_relations("VOY-145");
        assert_eq!(rels.len(), 2); // two implements
    }

    #[test]
    fn test_remove_relation() {
        let mut store = RelationsStore::new();
        store
            .add_relation("EXP-1257", "VOY-145", "implements")
            .unwrap();
        assert_eq!(store.active_count(), 1);

        store
            .remove_relation("EXP-1257", "VOY-145", "implements")
            .unwrap();
        assert_eq!(store.active_count(), 0);

        // Removed relation should not appear in queries
        let rels = store.query_relations("EXP-1257");
        assert_eq!(rels.len(), 0);
    }

    #[test]
    fn test_remove_nonexistent_relation() {
        let mut store = RelationsStore::new();
        let err = store.remove_relation("EXP-1", "VOY-1", "blocks");
        assert!(err.is_err());
    }

    #[test]
    fn test_bidirectional_query() {
        let mut store = RelationsStore::new();
        store.add_relation("EXPR-131", "EXP-800", "spawns").unwrap();

        // Queryable from both sides
        assert_eq!(store.query_relations("EXPR-131").len(), 1);
        assert_eq!(store.query_relations("EXP-800").len(), 1);

        // Not found for unrelated item
        assert_eq!(store.query_relations("EXP-999").len(), 0);
    }

    /// campaign membership is DATA — the sources of `partOf` edges to the campaign, never
    /// a hardcoded list. `incoming_by_predicate` is exactly that resolution.
    #[test]
    fn incoming_by_predicate_resolves_campaign_members_from_partof_edges() {
        let mut store = RelationsStore::new();
        // Two voyages join campaign CA-1 via partOf; another joins CA-2.
        store.add_relation("VY-100", "CA-1", "partOf").unwrap();
        store.add_relation("VY-101", "CA-1", "partOf").unwrap();
        store.add_relation("VY-200", "CA-2", "partOf").unwrap();
        // A DIFFERENT predicate to CA-1 must NOT count as membership (only partOf does).
        store.add_relation("VY-102", "CA-1", "related").unwrap();

        // Members of CA-1 = exactly its partOf sources, sorted + deduped.
        assert_eq!(
            store.incoming_by_predicate("CA-1", "partOf"),
            vec!["VY-100".to_string(), "VY-101".to_string()]
        );
        assert_eq!(
            store.incoming_by_predicate("CA-2", "partOf"),
            vec!["VY-200".to_string()]
        );
        // A campaign with no partOf members resolves empty (renders empty, not an error).
        assert!(store.incoming_by_predicate("CA-9", "partOf").is_empty());
        // A removed membership stops counting.
        store.remove_relation("VY-100", "CA-1", "partOf").unwrap();
        assert_eq!(
            store.incoming_by_predicate("CA-1", "partOf"),
            vec!["VY-101".to_string()]
        );
    }
}
