// SPDX-License-Identifier: MIT
//! Extension CORE-READ acceptance battery: `apply_with_core` hands an extension
//! a read-only view of the live core tables, and nothing else moved.
//!
//! Four claims, one arm each:
//! - (a) an extension implementing ONLY `apply` (the pre-widening shape) is
//!   dispatched through the defaulted `apply_with_core` and behaves exactly as
//!   before — reply and commit parity, no core type named anywhere in the impl;
//! - (b) an extension overriding `apply_with_core` reads real core state: an
//!   item created by a core verb is visible — id, title, status, and its typed
//!   relations — to the very next extension verb;
//! - (c) the view is LIVE at the dispatch: a core `move` between two extension
//!   probes changes the status the second probe reports;
//! - (d) an overriding mutation rides the SAME commit boundary as a plain
//!   `apply` mutation — exactly one event, seq advances by one.
//!
//! The probe extension's plain `apply` is a distinctively-marked failure, so if
//! the engine ever stopped dispatching through `apply_with_core`, arms (b)–(d)
//! would report `APPLY_FALLBACK` instead of data — the battery goes red on the
//! exact regression it exists to pin.

use arrow::array::StringArray;
use arrow_kanban::crud::KanbanStore;
use arrow_kanban::relations::RelationsStore;
use arrow_kanban::schema::items_col;
use arrow_kanban_server::engine::{KanbanEngine, KanbanReply};
use arrow_kanban_server::events::MutationEvent;
use arrow_kanban_server::extension::{CoreRead, EngineExtension, ExtCommand, ExtVerbSpec};
use arrow_kanban_server::health::HealthGate;
use arrow_kanban_server::lease::WriterLease;
use arrow_kanban_server::storage::StorageBackend;

fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).unwrap_or(serde_json::Value::Null)
}

fn call<B: StorageBackend, L: WriterLease>(
    engine: &mut KanbanEngine<B, L>,
    verb: &str,
    payload: serde_json::Value,
) -> serde_json::Value {
    json(&engine.dispatch(
        &format!("kanban.cmd.{verb}"),
        payload.to_string().as_bytes(),
    ))
}

// ── (a) the pre-widening shape: `apply` only, no core type named ─────────────

/// A minimal extension that implements ONLY `apply` — the exact shape every
/// impl had before the seam widened. Its whole body compiles without naming a
/// single core type, which is the "scratch demo stays free" property.
#[derive(Default)]
struct LegacyExtension {
    rows: Vec<String>,
}

impl EngineExtension for LegacyExtension {
    fn classify(&self, verb: &str) -> Option<ExtVerbSpec> {
        (verb == "ext.legacy.add").then(|| ExtVerbSpec {
            namespace: "legacy".into(),
            is_mutation: true,
        })
    }
    fn apply(&mut self, _verb: &str, payload: &[u8]) -> Result<KanbanReply, KanbanReply> {
        let v: serde_json::Value = serde_json::from_slice(payload)
            .map_err(|e| KanbanReply::error(&format!("Invalid JSON: {e}"), "INVALID_PAYLOAD"))?;
        let row = v.get("row").and_then(|r| r.as_str()).unwrap_or("").to_string();
        self.rows.push(row.clone());
        Ok(KanbanReply::Value(
            serde_json::json!({ "row": row, "count": self.rows.len() }),
        ))
    }
    fn derive_event(&self, cmd: &ExtCommand, reply: &KanbanReply) -> Option<MutationEvent> {
        Some(MutationEvent::Extension {
            namespace: cmd.namespace.clone(),
            kind: "add".into(),
            payload: reply.to_value(),
        })
    }
    fn replay(
        &mut self,
        _n: &str,
        _k: &str,
        _p: &serde_json::Value,
    ) -> Result<(), String> {
        Ok(())
    }
}

#[test]
fn a_apply_only_extension_dispatches_unchanged_through_the_default() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut engine = KanbanEngine::with_extension(
        KanbanStore::new(),
        RelationsStore::new(),
        dir.path().to_path_buf(),
        HealthGate::new(),
        Box::new(LegacyExtension::default()),
    );

    let r = call(
        &mut engine,
        "ext.legacy.add",
        serde_json::json!({ "row": "first" }),
    );
    assert_eq!(r["row"], "first", "the plain apply ran via the default: {r}");
    assert_eq!(r["count"], 1);
    assert_eq!(
        engine.committed_seq(),
        1,
        "one mutation, one committed event — parity with the pre-widening seam"
    );
}

// ── the probe extension: overrides `apply_with_core`, reads core ─────────────

/// Overrides `apply_with_core` to answer probes from the core tables. Its plain
/// `apply` is a marked failure: reaching it means the engine stopped
/// dispatching through the `_with_core` variant.
#[derive(Default)]
struct ProbeExtension {
    notes: u64,
}

impl EngineExtension for ProbeExtension {
    fn classify(&self, verb: &str) -> Option<ExtVerbSpec> {
        match verb {
            "ext.probe.item" => Some(ExtVerbSpec {
                namespace: "probe".into(),
                is_mutation: false,
            }),
            "ext.probe.note" => Some(ExtVerbSpec {
                namespace: "probe".into(),
                is_mutation: true,
            }),
            _ => None,
        }
    }

    fn apply(&mut self, _verb: &str, _payload: &[u8]) -> Result<KanbanReply, KanbanReply> {
        Err(KanbanReply::error(
            "plain apply reached — the engine no longer dispatches apply_with_core",
            "APPLY_FALLBACK",
        ))
    }

    fn apply_with_core(
        &mut self,
        verb: &str,
        payload: &[u8],
        core: CoreRead<'_>,
    ) -> Result<KanbanReply, KanbanReply> {
        match verb {
            "ext.probe.item" => {
                let v: serde_json::Value = serde_json::from_slice(payload).map_err(|e| {
                    KanbanReply::error(&format!("Invalid JSON: {e}"), "INVALID_PAYLOAD")
                })?;
                let id = v.get("id").and_then(|i| i.as_str()).unwrap_or("");
                match core.store.get_item(id) {
                    Ok(batch) => {
                        let col = |i: usize| {
                            batch
                                .column(i)
                                .as_any()
                                .downcast_ref::<StringArray>()
                                .map(|a| a.value(0).to_string())
                                .unwrap_or_default()
                        };
                        Ok(KanbanReply::Value(serde_json::json!({
                            "found": true,
                            "title": col(items_col::TITLE),
                            "status": col(items_col::STATUS),
                            "relations": core.relations.query_relations(id).iter()
                                .map(|b| b.num_rows()).sum::<usize>(),
                        })))
                    }
                    Err(_) => Ok(KanbanReply::Value(serde_json::json!({ "found": false }))),
                }
            }
            "ext.probe.note" => {
                self.notes += 1;
                Ok(KanbanReply::Value(
                    serde_json::json!({ "notes": self.notes }),
                ))
            }
            other => Err(KanbanReply::error(
                &format!("Unknown command: {other}"),
                "UNKNOWN_COMMAND",
            )),
        }
    }

    fn derive_event(&self, cmd: &ExtCommand, reply: &KanbanReply) -> Option<MutationEvent> {
        Some(MutationEvent::Extension {
            namespace: cmd.namespace.clone(),
            kind: "note".into(),
            payload: reply.to_value(),
        })
    }

    fn replay(
        &mut self,
        _n: &str,
        _k: &str,
        _p: &serde_json::Value,
    ) -> Result<(), String> {
        self.notes += 1;
        Ok(())
    }
}

fn probe_engine(dir: &std::path::Path) -> KanbanEngine {
    KanbanEngine::with_extension(
        KanbanStore::new(),
        RelationsStore::new(),
        dir.to_path_buf(),
        HealthGate::new(),
        Box::new(ProbeExtension::default()),
    )
}

// ── (b) core state is visible: item + typed relations ────────────────────────

#[test]
fn b_extension_reads_core_item_and_relations_through_the_view() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut engine = probe_engine(dir.path());

    let a = call(
        &mut engine,
        "create",
        serde_json::json!({ "title": "root item", "item_type": "chore" }),
    );
    let a_id = a["id"].as_str().expect("created id").to_string();
    let b = call(
        &mut engine,
        "create",
        serde_json::json!({ "title": "leaf item", "item_type": "chore" }),
    );
    let b_id = b["id"].as_str().expect("created id").to_string();
    call(
        &mut engine,
        "relation.add",
        serde_json::json!({ "source_id": a_id, "target_id": b_id, "predicate": "related" }),
    );

    let probe = call(&mut engine, "ext.probe.item", serde_json::json!({ "id": a_id }));
    assert_eq!(
        probe["found"], true,
        "the extension sees the core item through CoreRead: {probe}"
    );
    assert_eq!(probe["title"], "root item");
    assert_eq!(probe["status"], "backlog");
    assert_eq!(
        probe["relations"], 1,
        "the typed relation is visible through the same view: {probe}"
    );

    let missing = call(
        &mut engine,
        "ext.probe.item",
        serde_json::json!({ "id": "CH-9999" }),
    );
    assert_eq!(missing["found"], false, "an absent id reads as absent: {missing}");
}

// ── (c) the view is live: a core move changes what the next probe sees ───────

#[test]
fn c_view_is_live_across_core_mutations() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut engine = probe_engine(dir.path());

    let c = call(
        &mut engine,
        "create",
        serde_json::json!({ "title": "mover", "item_type": "chore" }),
    );
    let id = c["id"].as_str().expect("created id").to_string();

    let before = call(&mut engine, "ext.probe.item", serde_json::json!({ "id": id }));
    assert_eq!(before["status"], "backlog");

    call(
        &mut engine,
        "move",
        serde_json::json!({ "id": id, "status": "in_progress" }),
    );

    let after = call(&mut engine, "ext.probe.item", serde_json::json!({ "id": id }));
    assert_eq!(
        after["status"], "in_progress",
        "the probe reads the state of THIS dispatch, not a stale bind: {after}"
    );
}

// ── (d) an overriding mutation keeps commit-boundary parity ──────────────────

#[test]
fn d_overriding_mutation_commits_exactly_one_event() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut engine = probe_engine(dir.path());
    assert_eq!(engine.committed_seq(), 0);

    let r = call(&mut engine, "ext.probe.note", serde_json::json!({}));
    assert_eq!(r["notes"], 1, "the overriding apply ran: {r}");
    assert_eq!(
        engine.committed_seq(),
        1,
        "one mutation through apply_with_core, one committed event"
    );

    let read = call(
        &mut engine,
        "ext.probe.item",
        serde_json::json!({ "id": "CH-1" }),
    );
    assert_eq!(
        engine.committed_seq(),
        1,
        "a read through apply_with_core commits nothing: {read}"
    );
}
