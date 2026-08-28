// SPDX-License-Identifier: MIT
//! Persistence — load and save KanbanStore from/to Parquet files.
//!
//! Delegates to `parquet_store::save_named_batches()` for crash-safe
//! atomic Parquet persistence (WAL + atomic tmp+rename).

use crate::comments::CommentsStore;
use crate::crud::KanbanStore;
use crate::parquet_store::{restore_named_batches, save_named_batches};
use crate::relations::RelationsStore;
use arrow::array::{RecordBatch, new_null_array};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Errors from persistence operations.
#[derive(Debug, thiserror::Error)]
pub enum PersistError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Parquet error: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),

    #[error("Arrow error: {0}")]
    Arrow(#[from] arrow::error::ArrowError),

    #[error("Save error: {0}")]
    Save(#[from] crate::parquet_store::SaveError),
}

pub type Result<T> = std::result::Result<T, PersistError>;

/// The data directory for Arrow-kanban state.
const DATA_DIR: &str = ".arrow-kanban";

/// Normalize a RecordBatch to match the target schema.
///
/// If the batch has fewer columns than the target, null columns are appended.
/// This handles schema evolution: old Parquet files with fewer columns can be
/// loaded by new code with additional columns.
fn normalize_batch(
    batch: &RecordBatch,
    target_schema: &arrow::datatypes::Schema,
) -> Result<RecordBatch> {
    let batch_cols = batch.num_columns();
    let target_cols = target_schema.fields().len();

    if batch_cols >= target_cols {
        return Ok(batch.clone());
    }

    let num_rows = batch.num_rows();
    let mut columns: Vec<Arc<dyn arrow::array::Array>> = Vec::with_capacity(target_cols);

    for i in 0..batch_cols {
        columns.push(batch.column(i).clone());
    }

    for i in batch_cols..target_cols {
        let field = target_schema.field(i);
        columns.push(new_null_array(field.data_type(), num_rows));
    }

    Ok(RecordBatch::try_new(
        Arc::new(target_schema.clone()),
        columns,
    )?)
}

fn normalize_batches(
    batches: Vec<RecordBatch>,
    target_schema: &arrow::datatypes::Schema,
) -> Result<Vec<RecordBatch>> {
    batches
        .iter()
        .map(|b| normalize_batch(b, target_schema))
        .collect()
}

/// Get the data directory path, creating it if necessary.
pub fn data_dir(root: &Path) -> Result<PathBuf> {
    let dir = root.join(DATA_DIR);
    if !dir.exists() {
        fs::create_dir_all(&dir)?;
    }
    Ok(dir)
}

/// Load a KanbanStore from Parquet files in the data directory.
///
/// Auto-normalizes old Parquet files with fewer columns to the current schema.
pub fn load_store(root: &Path) -> Result<KanbanStore> {
    let dir = data_dir(root)?;
    let mut store = KanbanStore::new();

    let results = restore_named_batches(&dir, &["items", "runs", "item_comments"])?;
    for (name, batches) in results {
        match name.as_str() {
            "items" => {
                let normalized = normalize_batches(batches, store.items_schema())?;
                store.load_items(normalized);
            }
            "runs" => {
                let normalized = normalize_batches(batches, store.runs_schema())?;
                store.load_runs(normalized);
            }
            "item_comments" => {
                let normalized = normalize_batches(batches, store.comments_schema())?;
                store.load_comments(normalized);
            }
            _ => {}
        }
    }

    // Migrate legacy comments from runs table (only if no comments loaded yet)
    if store.comments_batches().is_empty() {
        let mut migrator = CommentsStore::new();
        migrator.migrate_from_runs(store.runs_batches());
        if !migrator.is_empty() {
            store.load_comments(migrator.batches().to_vec());
        }
    }

    Ok(store)
}

/// Load a RelationsStore from a Parquet file in the data directory.
pub fn load_relations(root: &Path) -> Result<RelationsStore> {
    let dir = data_dir(root)?;
    let mut store = RelationsStore::new();

    let results = restore_named_batches(&dir, &["relations"])?;
    for (name, batches) in results {
        if name == "relations" {
            let normalized = normalize_batches(batches, store.schema())?;
            store.load(normalized);
        }
    }

    Ok(store)
}

/// Save a KanbanStore to Parquet files in the data directory.
///
/// Uses `parquet_store::save_named_batches()` for crash-safe atomic writes.
pub fn save_store(root: &Path, store: &KanbanStore) -> Result<()> {
    let dir = data_dir(root)?;
    save_named_batches(
        &[
            ("items", store.items_batches(), store.items_schema()),
            ("runs", store.runs_batches(), store.runs_schema()),
            (
                "item_comments",
                store.comments_batches(),
                store.comments_schema(),
            ),
        ],
        &dir,
    )?;
    Ok(())
}

/// Save a RelationsStore to a Parquet file in the data directory.
pub fn save_relations(root: &Path, store: &RelationsStore) -> Result<()> {
    let dir = data_dir(root)?;
    save_named_batches(&[("relations", store.batches(), store.schema())], &dir)?;
    Ok(())
}

/// Save all kanban data atomically (items + runs + relations).
///
/// Single WAL covers all three datasets — better crash safety than
/// calling `save_store` + `save_relations` separately.
pub fn save_all(root: &Path, store: &KanbanStore, relations: &RelationsStore) -> Result<()> {
    let dir = data_dir(root)?;
    save_named_batches(
        &[
            ("items", store.items_batches(), store.items_schema()),
            ("runs", store.runs_batches(), store.runs_schema()),
            (
                "item_comments",
                store.comments_batches(),
                store.comments_schema(),
            ),
            ("relations", relations.batches(), relations.schema()),
        ],
        &dir,
    )?;
    Ok(())
}

/// Load all kanban data (items + runs + relations).
///
/// Auto-normalizes old Parquet files with fewer columns to the current schema.
pub fn load_all(root: &Path) -> Result<(KanbanStore, RelationsStore)> {
    let dir = data_dir(root)?;
    let mut store = KanbanStore::new();
    let mut relations = RelationsStore::new();

    let results = restore_named_batches(&dir, &["items", "runs", "item_comments", "relations"])?;
    for (name, batches) in results {
        match name.as_str() {
            "items" => {
                let normalized = normalize_batches(batches, store.items_schema())?;
                store.load_items(normalized);
            }
            "runs" => {
                let normalized = normalize_batches(batches, store.runs_schema())?;
                store.load_runs(normalized);
            }
            "item_comments" => {
                let normalized = normalize_batches(batches, store.comments_schema())?;
                store.load_comments(normalized);
            }
            "relations" => {
                let normalized = normalize_batches(batches, relations.schema())?;
                relations.load(normalized);
            }
            _ => {}
        }
    }

    // Migrate legacy comments from runs table (only if no comments loaded yet)
    if store.comments_batches().is_empty() {
        let mut migrator = CommentsStore::new();
        migrator.migrate_from_runs(store.runs_batches());
        if !migrator.is_empty() {
            store.load_comments(migrator.batches().to_vec());
        }
    }

    Ok((store, relations))
}

// ── Experiment Runs Persistence ─────────────────────────────────────────

/// Load experiment runs from Parquet.
pub fn load_experiment_runs(root: &Path) -> crate::experiment_runs::ExperimentRunStore {
    let dir = match data_dir(root) {
        Ok(d) => d,
        Err(_) => return crate::experiment_runs::ExperimentRunStore::new(),
    };

    match restore_named_batches(&dir, &["experiment_runs"]) {
        Ok(results) => {
            for (name, batches) in results {
                if name == "experiment_runs" {
                    return crate::experiment_runs::ExperimentRunStore::from_batches(batches);
                }
            }
            crate::experiment_runs::ExperimentRunStore::new()
        }
        Err(_) => crate::experiment_runs::ExperimentRunStore::new(),
    }
}

/// Save experiment runs to Parquet.
pub fn save_experiment_runs(
    root: &Path,
    store: &crate::experiment_runs::ExperimentRunStore,
) -> Result<()> {
    let dir = data_dir(root)?;
    let batches = store.batches();

    if batches.is_empty() {
        return Ok(());
    }

    let schema = crate::schema::experiment_runs_schema();
    save_named_batches(&[("experiment_runs", batches, &schema)], &dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crud::CreateItemInput;
    use crate::item_type::ItemType;

    #[test]
    fn test_save_and_load_roundtrip() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let root = dir.path();

        // Create store with items
        let mut store = KanbanStore::new();
        let id = store
            .create_item(&CreateItemInput {
                embedding: None,
                title: "Test Expedition".to_string(),
                item_type: ItemType::Expedition,
                priority: Some("high".to_string()),
                assignee: Some("M5".to_string()),
                tags: vec!["v14".to_string()],
                related: vec![],
                depends_on: vec![],
                body: None,
            })
            .expect("create item");

        store
            .update_status(&id, "in_progress", Some("M5"), false, None)
            .expect("update status");

        // Save
        save_store(root, &store).expect("save store");

        // Load
        let loaded = load_store(root).expect("load store");

        assert_eq!(loaded.active_item_count(), 1);
        let item = loaded.get_item(&id).expect("get item");
        assert_eq!(item.num_rows(), 1);
    }

    #[test]
    fn test_load_empty_dir() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let store = load_store(dir.path()).expect("load empty");
        assert_eq!(store.active_item_count(), 0);
    }

    #[test]
    fn issue40_old_parquet_without_attempt_count_loads_and_requeues() {
        // Simulate a PRE-#40 snapshot: an items.parquet whose schema ends at
        // priority_rank (18 columns). normalize_batch must pad the missing
        // attempt_count with nulls, which read as 0 — and a requeue must work.
        use crate::schema::{items_col, items_schema};
        use arrow::array::{
            Array, BooleanArray, ListBuilder, RecordBatch, StringArray, StringBuilder,
            TimestampMillisecondArray,
        };
        use std::sync::Arc;

        let full = items_schema();
        assert_eq!(
            full.fields().len(),
            items_col::EMBEDDING + 1,
            "embedding is now the LAST column (EX-8693) — attempt_count is no longer trailing, \
             so this test's 'old' fixture (built as all-columns-before-attempt_count) exercises \
             the SAME generic pad-with-nulls path for two missing trailing columns at once"
        );
        let old_schema =
            arrow::datatypes::Schema::new(full.fields()[..items_col::ATTEMPT_COUNT].to_vec());

        let mut tags = ListBuilder::new(StringBuilder::new());
        tags.values().append_value("kanban-v2");
        tags.append(true);
        let mut related = ListBuilder::new(StringBuilder::new());
        related.append(true);
        let mut depends = ListBuilder::new(StringBuilder::new());
        depends.append(true);

        let old_batch = RecordBatch::try_new(
            Arc::new(old_schema.clone()),
            vec![
                Arc::new(StringArray::from(vec!["CH-100"])),
                Arc::new(StringArray::from(vec!["Pre-#40 item"])),
                Arc::new(StringArray::from(vec!["chore"])),
                Arc::new(StringArray::from(vec!["in_progress"])),
                Arc::new(StringArray::from(vec![None::<&str>])),
                Arc::new(
                    TimestampMillisecondArray::from(vec![1710374400000i64]).with_timezone("UTC"),
                ),
                Arc::new(StringArray::from(vec![Some("Air")])),
                Arc::new(StringArray::from(vec!["development"])),
                Arc::new(tags.finish()),
                Arc::new(related.finish()),
                Arc::new(depends.finish()),
                Arc::new(StringArray::from(vec![None::<&str>])),
                Arc::new(StringArray::from(vec![None::<&str>])),
                Arc::new(BooleanArray::from(vec![false])),
                Arc::new(StringArray::from(vec![None::<&str>])),
                Arc::new(StringArray::from(vec![None::<&str>])),
                Arc::new(TimestampMillisecondArray::from(vec![None::<i64>]).with_timezone("UTC")),
                Arc::new(arrow::array::Int32Array::from(vec![None::<i32>])),
            ],
        )
        .expect("old-schema batch");

        let dir = tempfile::tempdir().expect("create tempdir");
        let ddir = data_dir(dir.path()).expect("data dir");
        save_named_batches(
            &[("items", std::slice::from_ref(&old_batch), &old_schema)],
            &ddir,
        )
        .expect("save old-schema parquet");

        let mut loaded = load_store(dir.path()).expect("load old parquet under new schema");
        let item = loaded.get_item("CH-100").expect("item present");
        assert_eq!(
            item.num_columns(),
            items_col::EMBEDDING + 1,
            "padded to the full current schema (20) — both attempt_count and embedding are missing"
        );
        assert!(
            item.column(items_col::ATTEMPT_COUNT).is_null(0),
            "pad is null, not 0 — provenance of 'predates the column' is preserved"
        );
        assert!(
            item.column(items_col::EMBEDDING).is_null(0),
            "embedding pad is null too — same generic trailing-column fill, EX-8693"
        );
        assert_eq!(loaded.attempt_count("CH-100").expect("readable"), 0);

        let out = loaded
            .requeue_item(
                "CH-100",
                Some("Air"),
                None,
                "old item fails too",
                3,
                "dead-letter",
            )
            .expect("requeue works on migrated item");
        assert_eq!(out.attempts, 1);
    }

    #[test]
    fn test_data_dir_created() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let ddir = data_dir(dir.path()).expect("data dir");
        assert!(ddir.exists());
        assert!(ddir.ends_with(".arrow-kanban"));
    }

    #[test]
    fn test_save_all_and_load_all() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let root = dir.path();

        let mut store = KanbanStore::new();
        store
            .create_item(&CreateItemInput {
                embedding: None,
                title: "Test".to_string(),
                item_type: ItemType::Expedition,
                priority: None,
                assignee: None,
                tags: vec![],
                related: vec![],
                depends_on: vec![],
                body: None,
            })
            .expect("create item");

        let relations = RelationsStore::new();
        save_all(root, &store, &relations).expect("save all");

        let (loaded_store, _loaded_rels) = load_all(root).expect("load all");
        assert_eq!(loaded_store.active_item_count(), 1);
    }
}
