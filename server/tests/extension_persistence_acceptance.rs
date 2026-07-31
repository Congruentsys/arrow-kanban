// SPDX-License-Identifier: MIT
//! Command-extension PERSISTENCE acceptance battery (E3 3c-ii).
//!
//! 3c-i proved the extension WRITE path: a namespaced verb family commits one
//! event per mutation through the same boundary as core. It deferred persistence —
//! on load an extension's events were durable in the log but not restored onto its
//! tables, there was no ext checkpoint, and the aggregate snapshot had no ext bind.
//! 3c-ii completes all three, and this battery proves them end to end:
//!
//!   (a) commit → `checkpoint` → reload via `restore` round-trips the ext rows;
//!   (b) a TORN ext checkpoint (seq sidecar dropped) replays from the log tail with
//!       NO doubling — RED-before-green by toggling the idempotent skip;
//!   (c) `AggregateSnapshot` binds the ext table at the commit seq, read via a
//!       downcast; a snapshot at seq N reflects exactly ext commits ≤ N (immutable);
//!   (d) a PRE-3cii store (ext events in the log, no ext checkpoint) restores by
//!       full-log replay.
//!
//! The demo extension below is DELIBERATELY generic — a `demo` namespace over a
//! real parquet-backed table with no workflow vocabulary — and it exercises the
//! seam from OUTSIDE the core crate, so it is itself evidence the persistence seam
//! carries no domain content. The `StorageBackend` is UNCHANGED throughout: the
//! ext checkpoint is a derived, engine-orchestrated post-commit step, and the ext
//! tail is replayed by the engine, never through the backend.

use arrow::array::{ArrayRef, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use arrow_kanban::crud::KanbanStore;
use arrow_kanban::relations::RelationsStore;
use arrow_kanban_server::actor;
use arrow_kanban_server::engine::{KanbanEngine, KanbanReply};
use arrow_kanban_server::events::MutationEvent;
use arrow_kanban_server::extension::{EngineExtension, ExtCommand, ExtVerbSpec, ExtensionSnapshot};
use arrow_kanban_server::health::HealthGate;
use arrow_kanban_server::snapshot::AggregateSnapshot;
use arrow_kanban_server::storage::{ParquetBackend, Seq};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

// ── The parquet-backed demo extension ────────────────────────────────────────

/// One row of the demo table.
#[derive(Clone)]
struct DemoRow {
    id: String,
    title: String,
}

/// A minimal extension: `ext.demo.propose` (mutation) appends a row; `ext.demo.list`
/// (read) returns the rows. The table is a real parquet checkpoint (id + title
/// columns), so this drives the whole persistence seam — no `pr`/`review`/`ci`
/// vocabulary, a generic table, so the seam is proven vocabulary-neutral.
///
/// `rows` is an `Arc<Vec<_>>` so [`snapshot`](EngineExtension::snapshot) is an O(1)
/// Arc-clone and a mutation copies-on-write ([`Arc::make_mut`]) away from any live
/// snapshot — the same "immutable point-in-time value" property the core tables have.
struct PersistentDemoExtension {
    rows: Arc<Vec<DemoRow>>,
    next_id: u64,
    /// When false, `replay` does NOT skip an already-present row — the RED control
    /// for the torn-checkpoint idempotence test (a re-presented tail then doubles).
    dedup_replay: bool,
    /// When false, `checkpoint` writes nothing — models a pre-3cii store whose ext
    /// events live only in the log (and are restored by a full-log replay).
    checkpoint_enabled: bool,
}

/// The demo table file names — RUNTIME strings owned by the extension, NEVER a
/// public constant (the F5 structural test pins that the core names only its four
/// tables). The `demo.checkpoint` sidecar carries the seq the parquet reflects.
const DEMO_PARQUET: &str = "demo"; // save_named_batches writes `<dir>/demo.parquet`
const DEMO_SEQ_SIDECAR: &str = "demo.checkpoint";

impl PersistentDemoExtension {
    fn new() -> Self {
        PersistentDemoExtension {
            rows: Arc::new(Vec::new()),
            next_id: 1,
            dedup_replay: true,
            checkpoint_enabled: true,
        }
    }
    /// Idempotent-skip DISABLED — the RED control for (b).
    fn without_dedup() -> Self {
        PersistentDemoExtension {
            dedup_replay: false,
            ..Self::new()
        }
    }
    /// Checkpoint DISABLED — models a pre-3cii store for (d).
    fn without_checkpoint() -> Self {
        PersistentDemoExtension {
            checkpoint_enabled: false,
            ..Self::new()
        }
    }

    fn demo_schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::Utf8, false),
            Field::new("title", DataType::Utf8, false),
        ]))
    }
}

/// An immutable, O(1)-cloneable bind of the demo table for the aggregate snapshot.
struct DemoSnapshot {
    rows: Arc<Vec<DemoRow>>,
}
impl ExtensionSnapshot for DemoSnapshot {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl EngineExtension for PersistentDemoExtension {
    fn classify(&self, verb: &str) -> Option<ExtVerbSpec> {
        match verb {
            "ext.demo.propose" => Some(ExtVerbSpec {
                namespace: "demo".into(),
                is_mutation: true,
            }),
            "ext.demo.list" => Some(ExtVerbSpec {
                namespace: "demo".into(),
                is_mutation: false,
            }),
            _ => None,
        }
    }

    fn apply(&mut self, verb: &str, payload: &[u8]) -> Result<KanbanReply, KanbanReply> {
        match verb {
            "ext.demo.propose" => {
                let req: serde_json::Value = serde_json::from_slice(payload).map_err(|e| {
                    KanbanReply::error(&format!("Invalid JSON: {e}"), "INVALID_PAYLOAD")
                })?;
                let title = req
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let id = format!("DEMO-{}", self.next_id);
                self.next_id += 1;
                // Copy-on-write append: clones the Vec only if a snapshot still
                // holds the old Arc, so a held snapshot stays immutable.
                Arc::make_mut(&mut self.rows).push(DemoRow {
                    id: id.clone(),
                    title: title.clone(),
                });
                Ok(KanbanReply::Value(json!({ "id": id, "title": title })))
            }
            "ext.demo.list" => {
                let items: Vec<_> = self
                    .rows
                    .iter()
                    .map(|r| json!({ "id": r.id, "title": r.title }))
                    .collect();
                Ok(KanbanReply::Value(json!({ "items": items })))
            }
            other => Err(KanbanReply::error(
                &format!("Unknown command: {other}"),
                "UNKNOWN_COMMAND",
            )),
        }
    }

    fn derive_event(&self, cmd: &ExtCommand, reply: &KanbanReply) -> Option<MutationEvent> {
        if !cmd.is_mutation {
            return None;
        }
        let kind = cmd.verb.rsplit('.').next().unwrap_or_default().to_string();
        Some(MutationEvent::Extension {
            namespace: cmd.namespace.clone(),
            kind,
            payload: reply.to_value(),
        })
    }

    fn replay(
        &mut self,
        namespace: &str,
        kind: &str,
        payload: &serde_json::Value,
    ) -> Result<(), String> {
        if namespace == "demo" && kind == "propose" {
            let id = payload
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or("replay: missing id")?
                .to_string();
            let title = payload
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // Idempotent skip — mirrors the core store's has_comment / has_relation
            // guards. A torn checkpoint re-presents the log tail; without this skip
            // (dedup_replay=false, the RED control) the re-applied row would double.
            if self.dedup_replay && self.rows.iter().any(|r| r.id == id) {
                return Ok(());
            }
            // Keep the mint counter ahead of any replayed id.
            if let Some(n) = id.strip_prefix("DEMO-").and_then(|n| n.parse::<u64>().ok()) {
                self.next_id = self.next_id.max(n + 1);
            }
            Arc::make_mut(&mut self.rows).push(DemoRow { id, title });
        }
        Ok(())
    }

    fn checkpoint(&self, dir: &Path, seq: Seq) -> Result<Vec<String>, String> {
        if !self.checkpoint_enabled {
            // A pre-3cii store: the ext events are durable in the log, and no ext
            // checkpoint is written — restore is by full-log replay.
            return Ok(Vec::new());
        }
        // (1) The demo table parquet, written atomically (tmp+rename) by the lib's
        // crash-safe helper — the same primitive the core checkpoint uses.
        let schema = Self::demo_schema();
        let ids: Vec<&str> = self.rows.iter().map(|r| r.id.as_str()).collect();
        let titles: Vec<&str> = self.rows.iter().map(|r| r.title.as_str()).collect();
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(ids)) as ArrayRef,
                Arc::new(StringArray::from(titles)) as ArrayRef,
            ],
        )
        .map_err(|e| e.to_string())?;
        let batches = vec![batch];
        arrow_kanban::parquet_store::save_named_batches(
            &[(DEMO_PARQUET, &batches, schema.as_ref())],
            dir,
        )
        .map_err(|e| e.to_string())?;
        // (2) The seq sidecar (atomic tmp+rename), recording the seq the parquet
        // reflects — the ext analogue of the core `_checkpoint.manifest` seq.
        let path = dir.join(DEMO_SEQ_SIDECAR);
        let tmp = dir.join(format!("{DEMO_SEQ_SIDECAR}.tmp"));
        std::fs::write(&tmp, seq.to_string()).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
        Ok(vec![
            format!("{DEMO_PARQUET}.parquet"),
            DEMO_SEQ_SIDECAR.into(),
        ])
    }

    fn restore(&mut self, dir: &Path) -> Result<Seq, String> {
        // Load the parquet rows (empty/absent → a fresh table).
        let restored = arrow_kanban::parquet_store::restore_named_batches(dir, &[DEMO_PARQUET])
            .map_err(|e| e.to_string())?;
        let mut rows = Vec::new();
        for (_name, parts) in restored {
            for batch in parts {
                let ids = batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .ok_or("demo restore: id column is not Utf8")?;
                let titles = batch
                    .column(1)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .ok_or("demo restore: title column is not Utf8")?;
                for i in 0..batch.num_rows() {
                    rows.push(DemoRow {
                        id: ids.value(i).to_string(),
                        title: titles.value(i).to_string(),
                    });
                }
            }
        }
        self.next_id = rows
            .iter()
            .filter_map(|r| {
                r.id.strip_prefix("DEMO-")
                    .and_then(|n| n.parse::<u64>().ok())
            })
            .max()
            .map(|m| m + 1)
            .unwrap_or(1);
        self.rows = Arc::new(rows);
        // The seq the parquet reflects. Absent (a torn or never-written sidecar) →
        // 0, so the engine replays the WHOLE ext tail from the log.
        let seq = std::fs::read_to_string(dir.join(DEMO_SEQ_SIDECAR))
            .ok()
            .and_then(|s| s.trim().parse::<Seq>().ok())
            .unwrap_or(0);
        Ok(seq)
    }

    fn snapshot(&self) -> Option<Arc<dyn ExtensionSnapshot>> {
        // O(1): an Arc-pointer clone of the current rows, no row data copied.
        Some(Arc::new(DemoSnapshot {
            rows: self.rows.clone(),
        }))
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).unwrap_or(serde_json::Value::Null)
}

fn demo_engine(root: &Path, ext: PersistentDemoExtension) -> KanbanEngine {
    KanbanEngine::with_extension(
        KanbanStore::new(),
        RelationsStore::new(),
        root.to_path_buf(),
        HealthGate::new(),
        Box::new(ext),
    )
}

fn call(engine: &mut KanbanEngine, verb: &str, payload: serde_json::Value) -> serde_json::Value {
    json(&engine.dispatch(
        &format!("kanban.cmd.{verb}"),
        payload.to_string().as_bytes(),
    ))
}

fn list_len(engine: &mut KanbanEngine) -> usize {
    call(engine, "ext.demo.list", json!({}))["items"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0)
}

/// Downcast the aggregate snapshot's ext bind to the demo table and read its rows.
fn demo_rows(snap: &AggregateSnapshot) -> Vec<DemoRow> {
    let ext = snap
        .ext
        .as_ref()
        .expect("the aggregate snapshot binds the ext table");
    let demo = ext
        .as_any()
        .downcast_ref::<DemoSnapshot>()
        .expect("the ext bind downcasts to the consumer's own DemoSnapshot type");
    (*demo.rows).clone()
}

fn store_dir(root: &Path) -> std::path::PathBuf {
    arrow_kanban::persist::data_dir(root).expect("store dir")
}

// ── (a) commit → checkpoint → reload via restore round-trips ─────────────────

#[test]
fn a_commit_checkpoint_reload_round_trips_the_demo_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    // Commit two proposes through a persistence extension; each post-commit
    // checkpoint writes the demo parquet + seq sidecar.
    {
        let mut e = demo_engine(root, PersistentDemoExtension::new());
        assert_eq!(
            call(&mut e, "ext.demo.propose", json!({ "title": "alpha" }))["id"],
            "DEMO-1"
        );
        assert_eq!(
            call(&mut e, "ext.demo.propose", json!({ "title": "beta" }))["id"],
            "DEMO-2"
        );
        assert_eq!(e.committed_seq(), 2, "two ext commits landed");
    }

    // The checkpoint files exist under the store dir.
    let sd = store_dir(root);
    assert!(
        sd.join("demo.parquet").exists(),
        "the post-commit checkpoint wrote the demo table parquet"
    );
    assert!(
        sd.join(DEMO_SEQ_SIDECAR).exists(),
        "the post-commit checkpoint wrote the seq sidecar"
    );

    // Reopen with a FRESH extension and restore: rows come from the checkpoint
    // (ext_seq == 2, so events_since(2) is empty — a pure checkpoint round-trip).
    let mut e2 = demo_engine(root, PersistentDemoExtension::new());
    e2.restore_extension().expect("restore");
    let l = call(&mut e2, "ext.demo.list", json!({}));
    let items = l["items"].as_array().expect("items array");
    assert_eq!(
        items.len(),
        2,
        "both demo rows restored from the checkpoint: {l}"
    );
    let titles: Vec<&str> = items.iter().map(|i| i["title"].as_str().unwrap()).collect();
    assert!(
        titles.contains(&"alpha") && titles.contains(&"beta"),
        "the restored rows carry the right titles: {titles:?}"
    );
}

// ── (b) torn ext checkpoint → log-tail replay, no doubling (RED-before-green) ─

#[test]
fn a_torn_ext_checkpoint_replays_from_the_log_without_doubling() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    // Three proposes → demo.parquet (3 rows) + seq sidecar (seq 3).
    {
        let mut e = demo_engine(root, PersistentDemoExtension::new());
        for t in ["a", "b", "c"] {
            call(&mut e, "ext.demo.propose", json!({ "title": t }));
        }
        assert_eq!(e.committed_seq(), 3, "three ext commits landed");
    }

    // Simulate the TORN checkpoint: the parquet reflects seq 3, but the seq sidecar
    // TRAILS — drop it so restore reads 0 and the whole (already-checkpointed) tail
    // is replayed over the populated table (the strongest torn case).
    std::fs::remove_file(store_dir(root).join(DEMO_SEQ_SIDECAR)).expect("drop the seq sidecar");

    // RED-evidence guard: the tail the load will replay is genuinely the 3 ext
    // events — so the counts below exercise idempotency, not an empty replay.
    let tail = ParquetBackend::read_events_since(root, 0).expect("events_since");
    assert_eq!(
        tail.len(),
        3,
        "all three ext events are in the replayed tail"
    );
    assert!(
        tail.iter().all(|c| c.event.is_extension()),
        "the tail is all extension events"
    );

    // RED: WITHOUT the idempotent skip, the torn-checkpoint replay re-applies the
    // whole tail on top of the restored parquet rows and DOUBLES them (3 → 6).
    let mut red = demo_engine(root, PersistentDemoExtension::without_dedup());
    red.restore_extension().expect("restore (red)");
    assert_eq!(
        list_len(&mut red),
        6,
        "without the idempotent skip, the re-presented tail doubles the rows — the hazard"
    );

    // GREEN: WITH the skip, restore replays the SAME tail with NO doubling (stays 3).
    let mut green = demo_engine(root, PersistentDemoExtension::new());
    green.restore_extension().expect("restore (green)");
    assert_eq!(
        list_len(&mut green),
        3,
        "the idempotent replay skips already-present rows — no doubling on a torn checkpoint"
    );
}

// ── (c) AggregateSnapshot binds the ext table at the commit seq ──────────────

#[tokio::test]
async fn the_aggregate_snapshot_binds_the_demo_table_at_the_commit_seq() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (handle, _join) = actor::spawn(
        demo_engine(dir.path(), PersistentDemoExtension::new()),
        |_e| {},
    );

    // Fresh: seq 0, the ext table is bound and empty.
    let s0 = handle.snapshot();
    assert_eq!(s0.seq(), 0, "a fresh handle is at seq 0");
    assert_eq!(
        demo_rows(&s0).len(),
        0,
        "the initial snapshot binds an empty demo table"
    );

    // Propose → seq 1; the snapshot binds the demo table at that seq.
    handle
        .dispatch("kanban.cmd.ext.demo.propose", br#"{"title":"first"}"#)
        .await;
    let s1 = handle.snapshot();
    assert_eq!(
        s1.seq(),
        1,
        "the snapshot advanced to the durable ext-commit seq"
    );
    let r1 = demo_rows(&s1);
    assert_eq!(
        r1.len(),
        1,
        "the seq-1 snapshot reflects the first demo commit"
    );
    assert_eq!(r1[0].title, "first");

    // Propose → seq 2.
    handle
        .dispatch("kanban.cmd.ext.demo.propose", br#"{"title":"second"}"#)
        .await;
    let s2 = handle.snapshot();
    assert_eq!(s2.seq(), 2);
    assert_eq!(
        demo_rows(&s2).len(),
        2,
        "the seq-2 snapshot reflects both demo commits"
    );

    // The held seq-1 snapshot is an IMMUTABLE value: it still binds exactly one row
    // (ext commits ≤ 1) and never sees the seq-2 commit — copy-on-write worked.
    assert_eq!(
        demo_rows(&s1).len(),
        1,
        "a held snapshot binds exactly the ext commits ≤ its seq — later commits never mutate it"
    );
}

// ── (d) a pre-3cii store (ext events in log, no ext checkpoint) full-replays ──

#[test]
fn a_pre_3cii_store_with_no_ext_checkpoint_restores_by_full_log_replay() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    // Engine 1: checkpoint DISABLED — the 3c-i shape: ext events are committed to
    // the log, but no ext checkpoint is ever written.
    {
        let mut e = demo_engine(root, PersistentDemoExtension::without_checkpoint());
        call(&mut e, "ext.demo.propose", json!({ "title": "one" }));
        call(&mut e, "ext.demo.propose", json!({ "title": "two" }));
        assert_eq!(e.committed_seq(), 2, "two ext commits landed");
    }

    // The pre-3cii on-disk shape: NO ext checkpoint, but the ext events ARE durable.
    let sd = store_dir(root);
    assert!(
        !sd.join("demo.parquet").exists(),
        "a pre-3cii store has NO ext checkpoint parquet"
    );
    assert!(
        !sd.join(DEMO_SEQ_SIDECAR).exists(),
        "a pre-3cii store has NO ext seq sidecar"
    );
    assert_eq!(
        ParquetBackend::read_events_since(root, 0)
            .expect("events_since")
            .len(),
        2,
        "the ext events are durable in the commit log"
    );

    // Engine 2: a real persistence extension restores by FULL-log replay (ext_seq
    // reads 0 — no sidecar — so the whole ext tail is replayed onto the tables).
    let mut e2 = demo_engine(root, PersistentDemoExtension::new());
    e2.restore_extension().expect("restore");
    assert_eq!(
        list_len(&mut e2),
        2,
        "a pre-3cii store's ext tables restore by replaying the whole event log"
    );
}
