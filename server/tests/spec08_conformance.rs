// SPDX-License-Identifier: MIT
//! Spec-08 (`kanban.cmd.*` wire protocol) conformance battery.
//!
//! Executable conformance for the published wire protocol — falsifies each §7
//! criterion against the REAL dispatch path (not prose). Ref:
//! `08-kanban-cmd-wire-protocol.md` §3 (verb set) + §7 (conformance).

use arrow_kanban_server::handlers::dispatch;
use arrow_kanban_server::state::ServerState;
use serde_json::{Value, json};

fn state(dir: &std::path::Path) -> ServerState {
    ServerState::new(
        arrow_kanban::crud::KanbanStore::new(),
        arrow_kanban::relations::RelationsStore::new(),
        dir.to_path_buf(),
        arrow_kanban_server::health::HealthGate::new(),
    )
}

fn call(st: &mut ServerState, verb: &str, payload: Value) -> Value {
    let bytes = dispatch(
        &format!("kanban.cmd.{verb}"),
        payload.to_string().as_bytes(),
        st,
    );
    serde_json::from_slice(&bytes).expect("reply is JSON")
}

/// §7.1 subject scheme + §7.2 typed errors: an unknown verb yields a typed error
/// object with a machine-readable code — distinguishable from success by SHAPE.
#[test]
fn unknown_verb_is_a_typed_error_not_a_success() {
    let dir = tempfile::tempdir().unwrap();
    let mut st = state(dir.path());
    let r = call(&mut st, "definitely-not-a-verb", json!({}));
    assert_eq!(r["code"], "UNKNOWN_COMMAND");
    assert!(
        r.get("error").is_some(),
        "an error MUST be distinguishable by shape (§7.2): {r}"
    );
}

/// §3 verb set: every documented CORE verb is offered — dispatched to a handler,
/// never `UNKNOWN_COMMAND`, even when a minimal payload makes the handler error.
/// (`query` is intentionally offered-but-UNSUPPORTED in the open engine — still a
/// recognised verb, still a typed error, never UNKNOWN_COMMAND.)
#[test]
fn core_verb_set_is_offered() {
    let dir = tempfile::tempdir().unwrap();
    let mut st = state(dir.path());
    // Exactly the published spec-08 §3 verb table (reads + writes). `query` is offered-
    // but-UNSUPPORTED in the open engine — still a recognised verb, never UNKNOWN_COMMAND.
    let spec08_core = [
        // reads (§3)
        "list",
        "show",
        "board",
        "query",
        "roadmap",
        "worklist",
        "blocked",
        "history",
        "stats",
        "export",
        "validate",
        "templates", // writes (§3)
        "create",
        "move",
        "update",
        "comment",
        "rank",
        "delete",
        "ratify",
    ];
    // arrow-kanban offers a SUPERSET beyond §3 (a conformant server MAY, §3). Exercised too
    // so a regression dropping them is caught, but kept SEPARATE so `spec08_core` never
    // drifts from the published verb table (E2 review, note 1).
    let superset = ["critical-path", "next-id"];
    for verb in spec08_core.iter().chain(superset.iter()) {
        let r = call(&mut st, verb, json!({}));
        assert_ne!(
            r["code"], "UNKNOWN_COMMAND",
            "verb `{verb}` MUST be offered: {r}"
        );
    }
}

/// §7.3 reads don't mutate: after a seed write, running every read verb leaves the
/// board byte-identical.
#[test]
fn reads_do_not_mutate() {
    let dir = tempfile::tempdir().unwrap();
    let mut st = state(dir.path());
    let seed = call(
        &mut st,
        "create",
        json!({"title": "seed", "item_type": "chore"}),
    );
    assert!(seed.get("id").is_some(), "seed create failed: {seed}");
    let before = call(&mut st, "board", json!({}));
    for read in [
        "list", "show", "board", "stats", "validate", "blocked", "roadmap",
    ] {
        call(&mut st, read, json!({}));
    }
    let after = call(&mut st, "board", json!({}));
    assert_eq!(before, after, "read verbs MUST NOT mutate state (§7.3)");
}

/// §7.5 events derive from committed writes: an accepted write commits exactly
/// one event (advancing the log by one and surfacing on `events_since`); a read
/// commits none. Exercised through the REAL engine commit + outbox source — the
/// typed derivation that replaced `detect_mutation` (E3 PR-2).
#[test]
fn events_derive_from_writes_not_reads() {
    let dir = tempfile::tempdir().unwrap();
    let mut st = state(dir.path());
    assert_eq!(st.committed_seq(), 0, "fresh engine has committed nothing");

    let created = dispatch(
        "kanban.cmd.create",
        json!({"title": "x", "item_type": "chore"})
            .to_string()
            .as_bytes(),
        &mut st,
    );
    let v: Value = serde_json::from_slice(&created).expect("reply is JSON");
    assert!(v.get("id").is_some(), "seed create failed: {v}");
    assert_eq!(
        st.committed_seq(),
        1,
        "an accepted write MUST commit exactly one event (§7.5)"
    );
    let events = st.events_since(0).expect("events_since");
    assert_eq!(events.len(), 1, "exactly one committed event");
    assert_eq!(events[0].event.subject_suffix(), "created");

    let before = st.committed_seq();
    let _ = dispatch("kanban.cmd.list", b"{}", &mut st);
    assert_eq!(
        st.committed_seq(),
        before,
        "a read MUST NOT commit an event (§7.5)"
    );
    assert!(
        st.events_since(before).expect("events_since").is_empty(),
        "a read commits no event"
    );
}
