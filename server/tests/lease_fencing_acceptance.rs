// SPDX-License-Identifier: MIT
//! Writer-lease fencing acceptance — a stale writer is fenced even holding a live handle.
//!
//! # What this proves (E3 PR-3b)
//!
//! The engine holds a [`WriterLease`] and, at the commit boundary, stamps the
//! epoch it holds on every commit and refuses to commit if a newer writer has
//! taken the lease. Three properties:
//!
//! 1. **Stale-writer-fenced-at-commit** (the named one) — two engines over the
//!    SAME store dir; the second takes the lease (bumps the epoch); the first,
//!    still holding a live handle, has its next mutating commit FENCED. The
//!    fenced write is absent from the store AND from `events_since` — no event,
//!    no seq advance, nothing durable. RED-before-green: the *identical* stale
//!    write LANDS through an engine without the epoch check (a no-fence lease),
//!    so it is the fence — not the command — that refuses it.
//! 2. **Crash-recovery reclaim** — a DEAD owner's lease is reclaimed by a fresh
//!    writer with a higher epoch, which then commits normally; a LIVE owner is a
//!    contested takeover, never a clean reclaim (never silently stolen).
//! 3. **Backward-compat** — a `_events.log` line written before the epoch field
//!    existed (a PR-2/3a `{seq, event}` line) still loads and replays, its epoch
//!    reading back as `0`.
//!
//! Fault injection is deterministic and in-process, matching the house style: a
//! second real lease acquisition for the takeover, a hand-planted owner record
//! (with a genuinely dead child pid) for the reclaim, and a hand-written legacy
//! log line for backward-compat.

use arrow_kanban::crud::KanbanStore;
use arrow_kanban::relations::RelationsStore;
use arrow_kanban_server::engine::KanbanEngine;
use arrow_kanban_server::events::{CreatedEvent, MutationEvent};
use arrow_kanban_server::health::HealthGate;
use arrow_kanban_server::lease::{
    Acquisition, Epoch, LeaseError, LocalWriterLease, OWNER_FILE, WriterLease, WriterOwner,
};
use arrow_kanban_server::storage::{ParquetBackend, StorageBackend};

fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).unwrap_or(serde_json::Value::Null)
}

fn create_payload(title: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "title": title, "item_type": "chore" }))
        .expect("encode")
}

fn healthy_engine(root: &std::path::Path) -> KanbanEngine {
    KanbanEngine::new(
        KanbanStore::new(),
        RelationsStore::new(),
        root.to_path_buf(),
        HealthGate::new(),
    )
}

/// A lease with NO epoch fence — `is_current` is always true. Injecting it into
/// the engine is exactly "the engine WITHOUT the epoch check", the RED control.
struct NoFenceLease;
impl WriterLease for NoFenceLease {
    fn epoch(&self) -> Epoch {
        0
    }
    fn current_epoch(&self) -> Result<Epoch, LeaseError> {
        Ok(0)
    }
    fn renew(&mut self) -> Result<Epoch, LeaseError> {
        Ok(0)
    }
    // is_current uses the default: 0 == 0 → always current → never fences.
}

// ── 1. THE named battery: a stale writer holding a live handle is fenced ─────

#[test]
fn stale_writer_holding_a_live_handle_is_fenced_at_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    // The incumbent writer takes the lease (epoch 1) and commits one item.
    let mut incumbent = healthy_engine(root);
    let a = json(&incumbent.dispatch("kanban.cmd.create", &create_payload("a")));
    let a_id = a["id"].as_str().expect("a id").to_string();
    assert_eq!(incumbent.committed_seq(), 1, "the incumbent's write landed");
    assert_eq!(incumbent.lease.epoch(), 1, "the incumbent holds epoch 1");

    // A SECOND writer takes the lease over the same directory — bumping the epoch.
    // (Acquiring the lease is the whole takeover; it need not be a full engine.)
    let second = LocalWriterLease::acquire(root);
    assert_eq!(second.epoch(), 2, "the second writer holds a higher epoch");
    assert_eq!(
        second.acquisition(),
        &Acquisition::Contested { from_epoch: 1 },
        "taking over a live incumbent is a contested takeover"
    );

    // The incumbent — STILL holding its live handle — attempts a mutating commit.
    // It is FENCED even though its in-process engine is perfectly alive.
    let b = json(&incumbent.dispatch("kanban.cmd.create", &create_payload("b")));
    assert_eq!(
        b["code"], "WRITER_FENCED",
        "the stale incumbent's commit must be fenced, got: {b}"
    );

    // The fenced mutation left NOTHING: no seq advance, no event, no in-memory
    // state, nothing durable.
    assert_eq!(
        incumbent.committed_seq(),
        1,
        "a fenced mutation does NOT advance the committed seq"
    );
    let events = incumbent.events_since(0).expect("events_since");
    assert_eq!(
        events.len(),
        1,
        "a fenced mutation emits NO event — only the incumbent's earlier commit remains"
    );
    assert_eq!(
        events[0].epoch, 1,
        "the surviving event carries its stamped epoch"
    );
    assert_eq!(
        incumbent.store.active_item_count(),
        1,
        "the fenced write never touched memory — the store still holds only 'a'"
    );
    assert!(
        incumbent.store.get_item(&a_id).is_ok(),
        "the incumbent's earlier item is intact"
    );

    // ...and nothing durable: a fresh load from disk sees exactly one item.
    let mut reopened = ParquetBackend::open(root).expect("reopen");
    let (loaded, committed_seq) = reopened.load().expect("load");
    assert_eq!(committed_seq, 1, "only the pre-fence commit is durable");
    assert_eq!(
        loaded.store.active_item_count(),
        1,
        "the fenced write is absent from the durable store"
    );
}

/// RED-before-green partner: the SAME stale on-disk state, but an engine WITHOUT
/// the epoch check (a no-fence lease) LANDS the write the fence refused above.
/// Isolates the fence as the cause — same store, same command, only the lease
/// differs.
#[test]
fn without_the_epoch_fence_the_same_stale_write_lands() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    // Identical setup: incumbent commits "a" (seq 1); a second writer bumps the
    // epoch to 2, so the authority now says the incumbent is stale.
    let mut incumbent = healthy_engine(root);
    incumbent.dispatch("kanban.cmd.create", &create_payload("a"));
    assert_eq!(incumbent.committed_seq(), 1);
    let second = LocalWriterLease::acquire(root);
    assert_eq!(
        second.epoch(),
        2,
        "the epoch was bumped — the store IS stale"
    );
    drop(incumbent);

    // An engine over the SAME (stale) directory but with NO epoch fence.
    let backend = ParquetBackend::open(root).expect("open");
    let mut no_fence = KanbanEngine::with_backend_and_lease(
        KanbanStore::new(),
        RelationsStore::new(),
        root.to_path_buf(),
        HealthGate::new(),
        backend,
        NoFenceLease,
    );

    // Without the epoch check the stale write LANDS (this is the RED the fence turns green).
    let b = json(&no_fence.dispatch("kanban.cmd.create", &create_payload("b")));
    assert!(
        b.get("error").is_none() && b.get("id").is_some(),
        "without the epoch check, the stale write is accepted: {b}"
    );
    assert_eq!(
        no_fence.committed_seq(),
        2,
        "the un-fenced write committed at the next seq — precisely what the fence prevents"
    );
}

// ── 2. Crash-recovery reclaim (dead owner) vs contested takeover (live owner) ─

/// A genuinely dead pid: spawn a child, reap it, and use its (now-freed) id.
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("spawn child");
    let pid = child.id();
    child.wait().expect("reap child");
    pid
}

fn plant_owner(root: &std::path::Path, owner: &WriterOwner) {
    std::fs::create_dir_all(root).expect("mkdir");
    std::fs::write(
        root.join(OWNER_FILE),
        serde_json::to_vec(owner).expect("encode owner"),
    )
    .expect("plant owner");
}

#[test]
fn a_fresh_writer_reclaims_from_a_dead_owner_and_commits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    // A prior owner at epoch 5 whose process is dead (a crash) still holds the
    // lease record on disk.
    plant_owner(
        root,
        &WriterOwner {
            epoch: 5,
            pid: dead_pid(),
            hostname: "crashed-host".into(),
            started_at_ms: 1,
            nonce: 1,
        },
    );

    // A fresh writer reclaims it — cleanly, with a strictly higher epoch.
    let lease = LocalWriterLease::acquire(root);
    assert_eq!(
        lease.acquisition(),
        &Acquisition::Reclaimed { from_epoch: 5 },
        "a dead owner is reclaimed (crash recovery)"
    );
    assert_eq!(lease.epoch(), 6, "reclaim mints a strictly higher epoch");

    // ...and the reclaimed lease commits normally.
    let backend = ParquetBackend::open(root).expect("open");
    let mut engine = KanbanEngine::with_backend_and_lease(
        KanbanStore::new(),
        RelationsStore::new(),
        root.to_path_buf(),
        HealthGate::new(),
        backend,
        lease,
    );
    let r = json(&engine.dispatch("kanban.cmd.create", &create_payload("post-recovery")));
    assert!(r.get("id").is_some(), "the reclaimed writer commits: {r}");
    assert_eq!(
        engine.committed_seq(),
        1,
        "the reclaimed writer's write landed"
    );
    let events = engine.events_since(0).expect("events_since");
    assert_eq!(
        events[0].epoch, 6,
        "the commit is stamped with the reclaimed epoch"
    );
}

/// A LIVE owner is never *silently* stolen from: taking over one is classified as
/// a contested takeover, NOT a clean reclaim (the negative control to the reclaim
/// above — the two cases are genuinely distinguished by liveness).
#[test]
fn a_live_owner_is_a_contested_takeover_not_a_silent_reclaim() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    // A prior owner at epoch 5 whose process is ALIVE (our own pid).
    plant_owner(
        root,
        &WriterOwner {
            epoch: 5,
            pid: std::process::id(),
            hostname: "this-host".into(),
            started_at_ms: 1,
            nonce: 1,
        },
    );

    let lease = LocalWriterLease::acquire(root);
    assert_eq!(
        lease.acquisition(),
        &Acquisition::Contested { from_epoch: 5 },
        "a LIVE owner is a contested takeover, never a clean reclaim — not silently stolen"
    );
    assert_eq!(
        lease.epoch(),
        6,
        "the takeover still mints a higher epoch (loud, fences the incumbent)"
    );
}

// ── 3. Backward-compat: a pre-epoch log line still loads ─────────────────────

/// Append one `{seq, event}` line WITHOUT the epoch field — a log line written by
/// the PR-2/3a backend, before the epoch existed.
fn append_legacy_line(root: &std::path::Path, seq: u64, event: &MutationEvent) {
    use std::io::Write;
    let dir = arrow_kanban::persist::data_dir(root).expect("store dir");
    let line = serde_json::to_string(&serde_json::json!({ "seq": seq, "event": event }))
        .expect("encode legacy line");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("_events.log"))
        .expect("open log");
    writeln!(f, "{line}").expect("append legacy line");
}

fn created(id: &str) -> MutationEvent {
    MutationEvent::Created(CreatedEvent {
        id: id.to_string(),
        title: format!("title {id}"),
        item_type: "chore".to_string(),
        board: "development".to_string(),
        priority: None,
        assignee: None,
        tags: Vec::new(),
        related: Vec::new(),
        depends_on: Vec::new(),
        body: None,
        relationships: Vec::new(),
    })
}

#[test]
fn a_pre_epoch_log_line_still_loads_and_reads_back_epoch_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    // One epoch-stamped commit through the current backend (seq 1, epoch 1)...
    {
        let mut engine = healthy_engine(root);
        engine.dispatch("kanban.cmd.create", &create_payload("stamped"));
        assert_eq!(engine.committed_seq(), 1);
    }
    // ...then a LEGACY line for seq 2 with no epoch field (a pre-epoch writer).
    append_legacy_line(root, 2, &created("LEGACY-2"));

    // The load must accept the legacy line (serde default → epoch 0), recover the
    // high-water, and replay it onto the checkpoint.
    let mut reopened = ParquetBackend::open(root).expect("reopen");
    let (loaded, committed_seq) = reopened.load().expect("load must accept a pre-epoch line");
    assert_eq!(
        committed_seq, 2,
        "the legacy line advances the recovered high-water"
    );
    assert!(
        loaded.store.get_item("LEGACY-2").is_ok(),
        "the pre-epoch line replays into the store"
    );

    // The legacy line reads back with epoch 0; the stamped one keeps its epoch.
    let all = reopened.events_since(0).expect("events_since");
    let legacy = all.iter().find(|c| c.seq == 2).expect("legacy event");
    assert_eq!(
        legacy.epoch, 0,
        "a pre-epoch line reads back as epoch 0 (serde default)"
    );
    let stamped = all.iter().find(|c| c.seq == 1).expect("stamped event");
    assert_eq!(stamped.epoch, 1, "the epoch-stamped line keeps its epoch");
}
