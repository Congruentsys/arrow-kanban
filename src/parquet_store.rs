// SPDX-License-Identifier: MIT
//! Plain-parquet persistence backend.
//!
//! Implements the small persistence surface the kanban uses —
//! `save_named_batches` / `restore_named_batches` and the graph-native commit log
//! (`Commit` / `CommitsTable` / `persist_commits` / `restore_commits`) — over
//! vanilla Apache Arrow + Parquet.
//!
//! Design goals:
//! - **Atomic writes** — each named dataset is written to `<name>.parquet.tmp`
//!   then `rename`d over `<name>.parquet` (POSIX atomic same-dir rename), so a
//!   crash mid-write never corrupts a live snapshot (it leaves a stale `.tmp`).
//! - **Named RecordBatch datasets** — one Parquet file per logical table
//!   (`items`, `runs`, `relations`, …), the same on-disk layout the CLI expects.
//! - **Commit log as JSON** — the graph-native audit trail is a plain JSON array.

use arrow::array::RecordBatch;
use arrow::datatypes::Schema;
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use std::fs;
use std::path::Path;
use std::sync::Arc;

/// Errors from the plain-parquet persistence backend.
///
/// The `#[from]` conversions let `persist::PersistError` absorb it via `#[from]`.
#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Parquet error: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),

    #[error("Arrow error: {0}")]
    Arrow(#[from] arrow::error::ArrowError),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

type Result<T> = std::result::Result<T, SaveError>;

/// Commit-log filename (leading `_` keeps it distinct from data tables).
const COMMITS_FILE: &str = "_commits.json";

/// Write a Parquet file atomically: full write to a sibling `.tmp`, then rename.
fn write_parquet_atomic(path: &Path, schema: &Schema, batches: &[RecordBatch]) -> Result<()> {
    let tmp_path = path.with_extension("parquet.tmp");
    {
        let file = fs::File::create(&tmp_path)?;
        let schema_ref = Arc::new(schema.clone());
        let mut writer = ArrowWriter::try_new(file, schema_ref, None)?;
        for batch in batches {
            writer.write(batch)?;
        }
        // `close` flushes the footer and syncs the file handle.
        writer.close()?;
    }
    // Same-directory rename is atomic on POSIX filesystems.
    fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Save a set of named RecordBatch datasets to Parquet files under `save_dir`.
///
/// Each `(name, batches, schema)` entry is written to `save_dir/<name>.parquet`
/// atomically. An empty batch list still writes a valid schema-only Parquet file
/// (so a subsequent restore sees an empty table rather than a missing one).
pub fn save_named_batches(
    entries: &[(&str, &[RecordBatch], &Schema)],
    save_dir: &Path,
) -> Result<()> {
    fs::create_dir_all(save_dir)?;
    for (name, batches, schema) in entries {
        let path = save_dir.join(format!("{name}.parquet"));
        write_parquet_atomic(&path, schema, batches)?;
    }
    Ok(())
}

/// Restore the named RecordBatch datasets that exist under `dir`.
///
/// Missing files are skipped silently (a fresh board has no snapshots yet), so the
/// returned vec contains only the datasets actually present, each as `(name, batches)`.
pub fn restore_named_batches(
    dir: &Path,
    names: &[&str],
) -> Result<Vec<(String, Vec<RecordBatch>)>> {
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let path = dir.join(format!("{name}.parquet"));
        if !path.exists() {
            continue;
        }
        let file = fs::File::open(&path)?;
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
        let mut batches = Vec::new();
        for batch in reader {
            batches.push(batch?);
        }
        out.push((name.to_string(), batches));
    }
    Ok(out)
}

/// A single graph-native commit in the audit trail.
///
/// Plain scalar fields — serialized as JSON in the commit log.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Commit {
    pub commit_id: String,
    pub parent_ids: Vec<String>,
    pub timestamp_ms: i64,
    pub message: String,
    pub author: String,
}

/// An append-only log of commits (the queryable audit trail).
#[derive(Debug, Clone, Default)]
pub struct CommitsTable {
    commits: Vec<Commit>,
}

impl CommitsTable {
    /// Create an empty commit log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a commit to the log.
    pub fn append(&mut self, commit: Commit) {
        self.commits.push(commit);
    }

    /// All commits, oldest-first.
    pub fn all(&self) -> &[Commit] {
        &self.commits
    }

    /// Number of commits in the log.
    pub fn len(&self) -> usize {
        self.commits.len()
    }

    /// Whether the log is empty.
    pub fn is_empty(&self) -> bool {
        self.commits.is_empty()
    }
}

/// Persist the commit log as a JSON array under `dir/_commits.json` (atomic).
pub fn persist_commits(table: &CommitsTable, dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    let path = dir.join(COMMITS_FILE);
    let tmp_path = dir.join(format!("{COMMITS_FILE}.tmp"));
    let json = serde_json::to_string_pretty(table.all())?;
    fs::write(&tmp_path, json)?;
    fs::rename(&tmp_path, &path)?;
    Ok(())
}

/// Restore the commit log from `dir/_commits.json`, or `None` if absent.
pub fn restore_commits(dir: &Path) -> Result<Option<CommitsTable>> {
    let path = dir.join(COMMITS_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let data = fs::read_to_string(&path)?;
    let commits: Vec<Commit> = serde_json::from_str(&data)?;
    Ok(Some(CommitsTable { commits }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Int64Array, StringArray};
    use arrow::datatypes::{DataType, Field};

    fn sample_schema() -> Schema {
        Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("title", DataType::Utf8, false),
        ])
    }

    fn sample_batch(schema: &Schema) -> RecordBatch {
        RecordBatch::try_new(
            Arc::new(schema.clone()),
            vec![
                Arc::new(Int64Array::from(vec![1_i64, 2, 3])),
                Arc::new(StringArray::from(vec!["a", "b", "c"])),
            ],
        )
        .expect("build sample batch")
    }

    #[test]
    fn save_restore_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let schema = sample_schema();
        let batch = sample_batch(&schema);

        save_named_batches(
            &[("items", std::slice::from_ref(&batch), &schema)],
            dir.path(),
        )
        .expect("save");

        let restored = restore_named_batches(dir.path(), &["items", "runs"]).expect("restore");
        // "runs" file does not exist → only "items" comes back.
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].0, "items");
        assert_eq!(restored[0].1.len(), 1);
        assert_eq!(restored[0].1[0].num_rows(), 3);
    }

    #[test]
    fn save_empty_batches_is_restorable_empty_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let schema = sample_schema();

        save_named_batches(&[("items", &[], &schema)], dir.path()).expect("save empty");

        let restored = restore_named_batches(dir.path(), &["items"]).expect("restore");
        assert_eq!(restored.len(), 1, "empty dataset still produces a file");
        let total_rows: usize = restored[0].1.iter().map(|b| b.num_rows()).sum();
        assert_eq!(total_rows, 0);
    }

    #[test]
    fn restore_missing_dir_is_empty_not_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist");
        let restored = restore_named_batches(&missing, &["items"]).expect("restore missing");
        assert!(restored.is_empty());
    }

    #[test]
    fn atomic_write_leaves_no_tmp_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let schema = sample_schema();
        let batch = sample_batch(&schema);
        save_named_batches(&[("items", &[batch], &schema)], dir.path()).expect("save");
        assert!(dir.path().join("items.parquet").exists());
        assert!(
            !dir.path().join("items.parquet.tmp").exists(),
            "tmp file must be renamed away after a successful write"
        );
    }

    #[test]
    fn commits_roundtrip_and_ordering() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut table = CommitsTable::new();
        assert!(table.is_empty());
        table.append(Commit {
            commit_id: "c1".into(),
            parent_ids: vec![],
            timestamp_ms: 100,
            message: "first".into(),
            author: "arrow-kanban".into(),
        });
        table.append(Commit {
            commit_id: "c2".into(),
            parent_ids: vec!["c1".into()],
            timestamp_ms: 200,
            message: "second".into(),
            author: "arrow-kanban".into(),
        });

        persist_commits(&table, dir.path()).expect("persist commits");

        let restored = restore_commits(dir.path())
            .expect("restore commits")
            .expect("some table");
        assert_eq!(restored.len(), 2);
        assert_eq!(restored.all().last().expect("last").commit_id, "c2");
        assert_eq!(restored.all()[0].parent_ids.len(), 0);
        assert_eq!(restored.all()[1].parent_ids, vec!["c1".to_string()]);
    }

    #[test]
    fn restore_commits_absent_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(restore_commits(dir.path()).expect("restore").is_none());
    }
}
