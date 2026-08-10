// SPDX-License-Identifier: MIT
//! Command-extension WRITE-PATH acceptance battery (E3 3c-i).
//!
//! Proves the extension seam: a namespaced verb family (`ext.<ns>.<verb>`) is
//! routed through the EXACT SAME commit boundary as a core command — one event
//! per accepted mutation, none for a read, fence/gate parity, a SEPARATE event
//! subject tree, and no shadowing of core verbs. The open engine (no extension)
//! offers zero of it.
//!
//! The demo extension below is DELIBERATELY generic — a `demo` namespace over an
//! in-memory table, with no workflow vocabulary of any kind. It lives in the
//! public test tree the export gate scans, so its presence is itself evidence
//! the seam carries no domain content.

use arrow_kanban::crud::KanbanStore;
use arrow_kanban::relations::RelationsStore;
use arrow_kanban_server::engine::{self, KanbanEngine, KanbanReply};
use arrow_kanban_server::events::MutationEvent;
use arrow_kanban_server::extension::{EngineExtension, ExtCommand, ExtVerbSpec};
use arrow_kanban_server::health::HealthGate;
use arrow_kanban_server::lease::{Epoch, LocalWriterLease, WriterLease};
use arrow_kanban_server::storage::{CommittedEvent, LoadedState, Seq, StorageBackend, StoreError};

// ── The toy demo extension: `demo` namespace over an in-memory table ─────────

/// A minimal extension: `ext.demo.propose` (mutation) records a row; `ext.demo.list`
/// (read) returns the rows. No `pr`/`proposal`/`review`/`ci` vocabulary — a
/// generic table, so the seam is proven vocabulary-neutral by construction.
#[derive(Default)]
struct DemoExtension {
    rows: std::collections::BTreeMap<String, String>,
    next_id: u64,
}

impl DemoExtension {
    fn new() -> Self {
        DemoExtension {
            rows: std::collections::BTreeMap::new(),
            next_id: 1,
        }
    }
}

impl EngineExtension for DemoExtension {
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
                self.rows.insert(id.clone(), title.clone());
                Ok(KanbanReply::Value(
                    serde_json::json!({ "id": id, "title": title }),
                ))
            }
            "ext.demo.list" => {
                let items: Vec<_> = self
                    .rows
                    .iter()
                    .map(|(id, title)| serde_json::json!({ "id": id, "title": title }))
                    .collect();
                Ok(KanbanReply::Value(serde_json::json!({ "items": items })))
            }
            other => Err(KanbanReply::error(
                &format!("Unknown command: {other}"),
                "UNKNOWN_COMMAND",
            )),
        }
    }

    fn derive_event(&self, cmd: &ExtCommand, reply: &KanbanReply) -> Option<MutationEvent> {
        // Only the mutating verb produces an event.
        if !cmd.is_mutation {
            return None;
        }
        // The event kind is the verb's last dotted segment (ext.demo.propose -> propose).
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
            self.rows.insert(id, title);
        }
        Ok(())
    }
}

/// An adversarial extension that GREEDILY claims every verb — including core ones.
/// The engine must still never route a core verb here (core `parse` recognises it
/// first), so this is the shadowing guard's positive control.
struct ShadowExtension;
impl EngineExtension for ShadowExtension {
    fn classify(&self, _verb: &str) -> Option<ExtVerbSpec> {
        Some(ExtVerbSpec {
            namespace: "shadow".into(),
            is_mutation: true,
        })
    }
    fn apply(&mut self, _verb: &str, _payload: &[u8]) -> Result<KanbanReply, KanbanReply> {
        // A distinctive marker: if a core verb were ever routed here, the reply
        // would carry `shadowed` instead of the core `id`.
        Ok(KanbanReply::Value(serde_json::json!({ "shadowed": true })))
    }
    fn derive_event(&self, cmd: &ExtCommand, reply: &KanbanReply) -> Option<MutationEvent> {
        Some(MutationEvent::Extension {
            namespace: cmd.namespace.clone(),
            kind: "x".into(),
            payload: reply.to_value(),
        })
    }
    fn replay(&mut self, _n: &str, _k: &str, _p: &serde_json::Value) -> Result<(), String> {
        Ok(())
    }
}

// ── A fault-injecting StorageBackend double (commit fails on demand) ─────────

struct FaultBackend {
    committed: Vec<CommittedEvent>,
    committed_seq: Seq,
    fail_commit: bool,
}
impl FaultBackend {
    fn new(fail_commit: bool) -> Self {
        FaultBackend {
            committed: Vec::new(),
            committed_seq: 0,
            fail_commit,
        }
    }
}
impl StorageBackend for FaultBackend {
    fn commit(
        &mut self,
        _store: &KanbanStore,
        _relations: &RelationsStore,
        epoch: Epoch,
        event: &MutationEvent,
    ) -> Result<Seq, StoreError> {
        if self.fail_commit {
            return Err(StoreError::Log(std::io::Error::other(
                "injected commit-log append failure",
            )));
        }
        let seq = self.committed_seq + 1;
        self.committed.push(CommittedEvent {
            seq,
            epoch,
            event: event.clone(),
        });
        self.committed_seq = seq;
        Ok(seq)
    }
    fn load(&mut self) -> Result<(LoadedState, Seq), StoreError> {
        Ok((
            LoadedState {
                store: KanbanStore::new(),
                relations: RelationsStore::new(),
            },
            self.committed_seq,
        ))
    }
    fn events_since(&self, after: Seq) -> Result<Vec<CommittedEvent>, StoreError> {
        Ok(self
            .committed
            .iter()
            .filter(|c| c.seq > after)
            .cloned()
            .collect())
    }
    fn committed_seq(&self) -> Seq {
        self.committed_seq
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).unwrap_or(serde_json::Value::Null)
}

/// A default-backend engine with the demo extension installed.
fn demo_engine(root: &std::path::Path) -> KanbanEngine {
    KanbanEngine::with_extension(
        KanbanStore::new(),
        RelationsStore::new(),
        root.to_path_buf(),
        HealthGate::new(),
        Box::new(DemoExtension::new()),
    )
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

// ── (a) one event per mutation, none for a read ──────────────────────────────

#[test]
fn a_propose_commits_exactly_one_event_and_a_list_commits_none() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut engine = demo_engine(dir.path());
    assert_eq!(engine.committed_seq(), 0, "fresh engine committed nothing");

    // The mutation routes through the SAME apply and commits exactly one event.
    let r = call(
        &mut engine,
        "ext.demo.propose",
        serde_json::json!({ "title": "first" }),
    );
    assert_eq!(r["id"], "DEMO-1", "the extension apply ran: {r}");
    assert_eq!(
        engine.committed_seq(),
        1,
        "an accepted extension mutation commits exactly one event"
    );
    let events = engine.events_since(0).expect("events_since");
    assert_eq!(events.len(), 1);
    assert!(
        events[0].event.is_extension(),
        "the committed event is an extension event"
    );
    assert_eq!(events[0].event.subject_suffix(), "ext");

    // The read commits nothing.
    let before = engine.committed_seq();
    let l = call(&mut engine, "ext.demo.list", serde_json::json!({}));
    assert!(l.get("items").is_some(), "list returns rows: {l}");
    assert_eq!(
        engine.committed_seq(),
        before,
        "an extension read MUST NOT commit an event"
    );
    assert!(
        engine
            .events_since(before)
            .expect("events_since")
            .is_empty(),
        "a read commits no event"
    );
}

// ── (b) sequence integrity interleaved with core commands ────────────────────

#[test]
fn extension_and_core_share_one_monotonic_seq_interleaved() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut engine = demo_engine(dir.path());

    // core create (1) -> ext propose (2) -> core move (3), one monotonic seq.
    let c = call(
        &mut engine,
        "create",
        serde_json::json!({ "title": "root", "item_type": "chore" }),
    );
    let id = c["id"].as_str().expect("created id").to_string();
    assert_eq!(engine.committed_seq(), 1);

    call(
        &mut engine,
        "ext.demo.propose",
        serde_json::json!({ "title": "middle" }),
    );
    assert_eq!(engine.committed_seq(), 2);

    call(
        &mut engine,
        "move",
        serde_json::json!({ "id": id, "status": "in_progress" }),
    );
    assert_eq!(engine.committed_seq(), 3);

    let log = engine.events_since(0).expect("events_since");
    let shape: Vec<(Seq, &str)> = log
        .iter()
        .map(|c| (c.seq, c.event.subject_suffix()))
        .collect();
    assert_eq!(
        shape,
        vec![(1, "created"), (2, "ext"), (3, "moved")],
        "the log is [core, ext, core] at [1,2,3] under one monotonic seq"
    );
}

// ── (c) fence / degrade parity — RED-before-green ────────────────────────────

/// A FENCED writer's extension mutation leaves no event and no seq advance —
/// identical to a fenced core mutation. RED-before-green: the partner test below
/// shows the SAME propose LANDS without the fence, so it is the fence — not the
/// command — that refuses it.
#[test]
fn a_fenced_extension_mutation_leaves_no_event_or_seq() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let mut engine = demo_engine(root);
    assert_eq!(engine.lease.epoch(), 1, "the engine holds epoch 1");

    // A second writer takes the lease over the same dir — bumping the epoch and
    // fencing the engine, which still holds its live handle.
    let second = LocalWriterLease::acquire(root);
    assert_eq!(second.epoch(), 2, "the second writer holds a higher epoch");

    let r = call(
        &mut engine,
        "ext.demo.propose",
        serde_json::json!({ "title": "fenced" }),
    );
    assert_eq!(
        r["code"], "WRITER_FENCED",
        "a fenced writer's extension mutation is refused, got: {r}"
    );
    assert_eq!(
        engine.committed_seq(),
        0,
        "a fenced extension mutation does NOT advance the seq"
    );
    assert!(
        engine.events_since(0).expect("events_since").is_empty(),
        "a fenced extension mutation emits NO event"
    );
}

/// RED-before-green partner: WITHOUT the fence, the SAME propose lands (seq
/// advances, one event). Isolates the fence as the cause of the refusal above.
#[test]
fn without_the_fence_the_same_extension_mutation_lands() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut engine = demo_engine(dir.path());

    let r = call(
        &mut engine,
        "ext.demo.propose",
        serde_json::json!({ "title": "unfenced" }),
    );
    assert!(
        r.get("id").is_some() && r.get("code").is_none(),
        "without the fence, the extension mutation is accepted: {r}"
    );
    assert_eq!(
        engine.committed_seq(),
        1,
        "the un-fenced extension mutation committed at the next seq"
    );
    assert_eq!(engine.events_since(0).expect("events_since").len(), 1);
}

/// A backend commit failure surfaces as STORE_NOT_DURABLE and commits no
/// extension event — the durability gate governs an extension mutation identically.
#[test]
fn an_extension_mutation_that_cannot_commit_is_store_not_durable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut engine = KanbanEngine::with_backend(
        KanbanStore::new(),
        RelationsStore::new(),
        dir.path().to_path_buf(),
        HealthGate::new(),
        FaultBackend::new(true),
    );
    engine.extension = Some(Box::new(DemoExtension::new()));

    let r = call(
        &mut engine,
        "ext.demo.propose",
        serde_json::json!({ "title": "doomed" }),
    );
    assert_eq!(
        r["code"], "STORE_NOT_DURABLE",
        "a commit failure on an extension mutation must be reported failed, got: {r}"
    );
    assert_eq!(
        engine.committed_seq(),
        0,
        "no extension event is committed for a failed commit"
    );
    assert!(
        engine.events_since(0).expect("events_since").is_empty(),
        "no event may be visible for a failed extension commit"
    );
    assert!(
        engine.health.is_degraded(),
        "a commit failure must degrade the gate"
    );
}

/// NEGATIVE CONTROL for the STORE_NOT_DURABLE test: with the injection OFF, the
/// identical extension mutation COMMITS one event (the fault genuinely bites).
#[test]
fn negative_control_without_the_fault_the_extension_commit_lands() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut engine = KanbanEngine::with_backend(
        KanbanStore::new(),
        RelationsStore::new(),
        dir.path().to_path_buf(),
        HealthGate::new(),
        FaultBackend::new(false),
    );
    engine.extension = Some(Box::new(DemoExtension::new()));

    let r = call(
        &mut engine,
        "ext.demo.propose",
        serde_json::json!({ "title": "kept" }),
    );
    assert!(r.get("id").is_some(), "healthy commit must ack: {r}");
    assert_eq!(engine.committed_seq(), 1);
    assert!(!engine.health.is_degraded());
}

// ── (d) separate event subject tree — frozen kanban.event.> unaffected ───────

#[test]
fn extension_events_project_to_a_separate_subject_tree() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut engine = demo_engine(dir.path());

    // A core create and an extension propose, so both event shapes are present.
    call(
        &mut engine,
        "create",
        serde_json::json!({ "title": "core", "item_type": "chore" }),
    );
    call(
        &mut engine,
        "ext.demo.propose",
        serde_json::json!({ "title": "ext" }),
    );

    let events = engine.events_since(0).expect("events_since");
    let core_ev = &events[0].event;
    let ext_ev = &events[1].event;
    assert!(!core_ev.is_extension() && ext_ev.is_extension());

    // The extension event projects to `ext.demo.propose` and publishes on the
    // SEPARATE `kanban.ext.event.demo.propose` tree.
    assert_eq!(ext_ev.event_subject(), "ext.demo.propose");
    assert_eq!(ext_ev.contract_version(), "ext.1");
    let ext_subject = ext_ev.published_subject("kanban.event", "kanban.ext.event");
    assert_eq!(ext_subject, "kanban.ext.event.demo.propose");

    // The frozen KANBAN_EVENTS stream filters `kanban.event.>`; the extension
    // event is NOT under it, so v1.0 consumers never receive it.
    assert!(
        !ext_subject.starts_with("kanban.event."),
        "an extension event must NOT sit under the frozen kanban.event.> tree: {ext_subject}"
    );
    // ...while the core event still DOES (byte-identical to before).
    let core_subject = core_ev.published_subject("kanban.event", "kanban.ext.event");
    assert_eq!(core_subject, "kanban.event.created");
    assert!(core_subject.starts_with("kanban.event."));
}

/// The extension event's log form round-trips, and the renamed `ext_kind` field
/// does NOT collide with the `#[serde(tag="kind")]` internal tag (which is
/// `Extension`). Adding the variant leaves the tag on existing variants unchanged.
#[test]
fn extension_event_round_trips_without_a_tag_collision() {
    let ev = MutationEvent::Extension {
        namespace: "demo".into(),
        kind: "propose".into(),
        payload: serde_json::json!({ "id": "DEMO-1", "title": "t" }),
    };
    let line = serde_json::to_string(&ev).expect("encode");
    let v: serde_json::Value = serde_json::from_str(&line).expect("as value");
    assert_eq!(v["kind"], "Extension", "the serde tag is the variant name");
    assert_eq!(
        v["ext_kind"], "propose",
        "the kind field is renamed off the tag"
    );
    assert_eq!(v["namespace"], "demo");
    let back: MutationEvent = serde_json::from_str(&line).expect("decode");
    assert_eq!(ev, back, "the extension event round-trips");

    // A legacy variant's tag is unaffected by the new arm.
    let created = serde_json::to_value(MutationEvent::Deleted(
        arrow_kanban_server::events::DeletedEvent {
            id: "CH-1".into(),
            agent: None,
        },
    ))
    .expect("encode deleted");
    assert_eq!(created["kind"], "Deleted");
}

// ── (e) unregistered ext verb / bare verb → UNKNOWN_COMMAND ──────────────────

#[test]
fn an_unregistered_extension_verb_is_unknown_command() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut engine = demo_engine(dir.path());

    // The extension does not classify `ext.demo.bogus` -> UNKNOWN_COMMAND.
    let a = call(&mut engine, "ext.demo.bogus", serde_json::json!({}));
    assert_eq!(a["code"], "UNKNOWN_COMMAND", "unregistered ext verb: {a}");

    // A bare `demo.x` (no ext prefix) is likewise unclassified.
    let b = call(&mut engine, "demo.x", serde_json::json!({}));
    assert_eq!(b["code"], "UNKNOWN_COMMAND", "unclassified bare verb: {b}");

    // Neither committed anything.
    assert_eq!(engine.committed_seq(), 0);
}

// ── (f) the open engine (no extension) offers zero ext verbs ─────────────────

#[test]
fn the_open_engine_offers_zero_extension_verbs() {
    let dir = tempfile::tempdir().expect("tempdir");
    // No extension installed — the open engine.
    let mut engine = KanbanEngine::new(
        KanbanStore::new(),
        RelationsStore::new(),
        dir.path().to_path_buf(),
        HealthGate::new(),
    );
    assert!(
        engine.extension.is_none(),
        "the open engine has no extension"
    );

    let r = call(
        &mut engine,
        "ext.demo.propose",
        serde_json::json!({ "title": "x" }),
    );
    assert_eq!(
        r["code"], "UNKNOWN_COMMAND",
        "the open engine offers no extension verbs: {r}"
    );
    assert_eq!(engine.committed_seq(), 0, "nothing committed");
}

// ── guards: no core verb starts with `ext.`; classify cannot shadow core ─────

#[test]
fn no_core_verb_starts_with_the_ext_prefix() {
    for verb in engine::ALL_VERBS {
        assert!(
            !verb.starts_with("ext."),
            "core verb `{verb}` collides with the extension namespace prefix"
        );
    }
}

#[test]
fn an_extension_cannot_shadow_a_core_verb() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Install the greedy extension that claims EVERY verb.
    let mut engine = KanbanEngine::with_extension(
        KanbanStore::new(),
        RelationsStore::new(),
        dir.path().to_path_buf(),
        HealthGate::new(),
        Box::new(ShadowExtension),
    );

    // `create` is a CORE verb — parse recognises it before the extension is ever
    // consulted, so it routes to core (returns an id), never to the shadow.
    let r = call(
        &mut engine,
        "create",
        serde_json::json!({ "title": "real", "item_type": "chore" }),
    );
    assert!(
        r.get("id").is_some() && r.get("shadowed").is_none(),
        "a core verb must route to core, never to a greedy extension: {r}"
    );
    assert_eq!(engine.committed_seq(), 1, "the core create committed");

    // But a genuinely non-core verb IS claimed by the extension (proving classify
    // works — it just can never intercept a core verb).
    let e = call(&mut engine, "totally.bogus", serde_json::json!({}));
    assert_eq!(
        e["shadowed"], true,
        "a non-core verb is routed to the extension: {e}"
    );
    assert_eq!(
        engine.committed_seq(),
        2,
        "the extension mutation committed"
    );
}

// ── replay: the extension rebuilds its table from a logged event (3c-i) ──────

/// `EngineExtension::replay` reconstructs the extension's table from a logged
/// `{namespace, kind, payload}`. (The engine's load-time replay orchestration is
/// 3c-ii; here the method is exercised directly.)
#[test]
fn the_extension_replays_its_table_from_a_logged_event() {
    let mut ext = DemoExtension::new();
    ext.replay(
        "demo",
        "propose",
        &serde_json::json!({ "id": "DEMO-7", "title": "restored" }),
    )
    .expect("replay");
    assert_eq!(
        ext.rows.get("DEMO-7").map(String::as_str),
        Some("restored"),
        "the extension table is rebuilt from the logged event"
    );
}
