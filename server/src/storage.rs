// SPDX-License-Identifier: MIT
//! The durable storage boundary — the commit point + transactional outbox source.
//!
//! # The commit boundary (Option A)
//!
//! Plain Parquet has no cross-file transaction: the four snapshot files
//! (`items`/`runs`/`item_comments`/`relations`) are written by four independent
//! atomic renames, so "persist the whole board" is really four commits, and a
//! crash between them tears the set. Rather than invent a multi-file transaction,
//! this backend makes an **append-only event log the single fsync commit point**
//! and treats the state Parquet as a *derived checkpoint*:
//!
//! 1. [`StorageBackend::commit`] appends the sequenced event to the log and
//!    fsyncs the log file **and its containing directory** — that fsync is the
//!    commit barrier. The write is committed iff it returns. The monotonic
//!    [`Seq`] is the log position.
//! 2. Only then is the state Parquet re-checkpointed via the existing atomic
//!    tmp+rename path (byte-identical on-disk layout, so stores written before
//!    this backend load unchanged). A torn checkpoint is **not** a lost write:
//!    the atomic rename means the live Parquet is always a *complete* earlier
//!    checkpoint, and the committed tail past it is replayed from the log on
//!    load.
//!
//! Both outbox invariants then hold by construction:
//! - **no durable write without its event** — the event is the barrier, written
//!   before the checkpoint;
//! - **no event for a write that did not land** — the seq advances only after
//!   the append fsync returns, so a failed commit logs nothing.
//!
//! This module is deliberately generic (no reference to any specific write-ahead
//! scheme): [`StorageBackend`] is the seam a consumer swaps to change durability,
//! and [`ParquetBackend`] is the default. PR-2 ships only these two.

use crate::events::MutationEvent;
use crate::lease::Epoch;
use arrow_kanban::crud::KanbanStore;
use arrow_kanban::relations::RelationsStore;
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

/// A monotonic commit-log position. Advances by exactly one per committed event;
/// `0` means "nothing committed yet".
pub type Seq = u64;

/// The on-disk table-contract version stamped into the checkpoint manifest (and
/// bound into every [`AggregateSnapshot`](crate::snapshot::AggregateSnapshot)).
/// Bump only when the persisted table schemas change incompatibly.
pub const SCHEMA_VERSION: u32 = 1;

/// The append-only event log — the single fsync commit point.
const EVENT_LOG_FILE: &str = "_events.log";
/// The high-water seq the state checkpoint reflects (leading `_` keeps it out of
/// the data-table namespace, like `_commits.json`).
///
/// LEGACY (PR-2): a bare seq marker written AFTER the parquet. It could trail the
/// parquet across a crash, replaying an already-checkpointed event → a
/// double-apply of the non-idempotent kinds (comment / relation.add). Superseded
/// by [`CHECKPOINT_MANIFEST_FILE`]; still READ as a fallback so a store written by
/// the old backend upgrades cleanly (marker seq is trusted when no manifest exists).
const CHECKPOINT_SEQ_FILE: &str = "_checkpoint.seq";
/// The checkpoint manifest — the single authoritative record of what the derived
/// parquet checkpoint covers: `{seq, schema_version, files}`. Written via
/// tmp+rename as the FINAL step of a commit, so a crash before the rename leaves
/// the PREVIOUS complete manifest in place and the whole uncovered tail is
/// replayed from there. On load, the manifest's seq is the checkpoint high-water:
/// replay is strictly AFTER it, so a checkpointed event is never replayed.
const CHECKPOINT_MANIFEST_FILE: &str = "_checkpoint.manifest";

/// Errors from the storage backend. (Hand-rolled `Display`/`Error` — the server
/// crate deliberately carries no `thiserror` dependency.)
#[derive(Debug)]
pub enum StoreError {
    /// The commit-log append/fsync failed — the write is NOT committed.
    Log(std::io::Error),
    /// The derived state checkpoint failed to load or a lower-level store error.
    Checkpoint(String),
    /// A log line could not be decoded on load / scan.
    Decode(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Log(e) => write!(f, "commit-log append failed: {e}"),
            StoreError::Checkpoint(e) => write!(f, "checkpoint error: {e}"),
            StoreError::Decode(e) => write!(f, "corrupt commit-log entry: {e}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Log(e) => Some(e),
            _ => None,
        }
    }
}

/// One committed event tagged with its durable log position, in seq order.
#[derive(Debug, Clone, PartialEq)]
pub struct CommittedEvent {
    /// The commit-log position assigned at commit time.
    pub seq: Seq,
    /// The writer-lease epoch (fencing token) the committing writer held. Records
    /// which writer generation produced this event — a replica reads it to detect
    /// a split brain (two generations interleaved). `0` for a pre-epoch log line.
    pub epoch: Epoch,
    /// The typed mutation event.
    pub event: MutationEvent,
}

/// State reconstructed by a load — the checkpoint brought current with the log tail.
pub struct LoadedState {
    /// The item/runs/comments store.
    pub store: KanbanStore,
    /// The typed-relations store.
    pub relations: RelationsStore,
}

/// The durable substrate behind the engine's write path.
///
/// A consumer swaps durability by constructing the engine over a different
/// backend; PR-2 ships only [`ParquetBackend`]. The commit is a synchronous
/// fsync barrier — no queue, no actor (that is later PR work).
pub trait StorageBackend {
    /// THE fsync commit barrier. Append+fsync the sequenced event (the commit
    /// point) STAMPED with the committing writer's lease `epoch`, then checkpoint
    /// `store`/`relations` as a derived snapshot, and return the assigned monotonic
    /// seq. On failure **nothing is committed**, the seq is unchanged, and no event
    /// becomes visible to [`events_since`]. The caller (the engine) fences a stale
    /// writer BEFORE calling this, so a fenced mutation never reaches the barrier.
    fn commit(
        &mut self,
        store: &KanbanStore,
        relations: &RelationsStore,
        epoch: Epoch,
        event: &MutationEvent,
    ) -> Result<Seq, StoreError>;

    /// Load the checkpoint and replay the committed log tail past it, returning
    /// the reconstructed state and the high-water committed seq. A torn
    /// checkpoint is recovered here rather than lost.
    fn load(&mut self) -> Result<(LoadedState, Seq), StoreError>;

    /// Committed events with `seq` strictly greater than `after`, in seq order —
    /// the outbox relay's source (re-derives the unpublished tail after a crash),
    /// and the replay/replica feed a future consumer will read.
    fn events_since(&self, after: Seq) -> Result<Vec<CommittedEvent>, StoreError>;

    /// The current committed high-water seq.
    fn committed_seq(&self) -> Seq;
}

/// One serialized commit-log line: the seq, the committing writer's lease epoch,
/// and its event.
///
/// `epoch` is `#[serde(default)]` so a log written before this field existed (a
/// PR-2/3a `{seq, event}` line) still deserializes — the epoch reads back as `0`.
/// It is the LAST-added serialized field placed mid-struct deliberately: the
/// default makes its absence backward-compatible, and no code keys off byte order
/// of the log (it is line-delimited JSON, not a fixed layout).
#[derive(serde::Serialize, serde::Deserialize)]
struct LogLine {
    seq: Seq,
    #[serde(default)]
    epoch: Epoch,
    event: MutationEvent,
}

/// The checkpoint manifest — one atomic record binding the derived parquet
/// checkpoint to the seq it reflects, the table-contract version, and the file
/// set it covers. Written tmp+rename as the final commit step; its `seq` is the
/// authoritative checkpoint high-water on load.
#[derive(serde::Serialize, serde::Deserialize)]
struct CheckpointManifest {
    seq: Seq,
    schema_version: u32,
    /// The data tables this checkpoint covers (the `save_all` file set). Recorded
    /// for auditability / future validation; the load path keys off `seq`.
    files: Vec<String>,
}

/// The parquet file set a checkpoint covers — the tables `save_all` persists.
const CHECKPOINT_FILES: &[&str] = &["items", "runs", "item_comments", "relations"];

/// The default backend: append-only event log + derived Parquet checkpoint.
pub struct ParquetBackend {
    /// The `--data-dir` root (the store lives at `<root>/.arrow-kanban`).
    root: PathBuf,
    /// High-water committed seq (max seq in the log).
    committed_seq: Seq,
}

impl ParquetBackend {
    /// Open the backend at `root`, discovering the committed high-water seq from
    /// the existing event log (0 for a fresh or pre-backend store).
    pub fn open(root: &Path) -> Result<Self, StoreError> {
        let committed_seq = Self::scan_high_water(root)?;
        Ok(ParquetBackend {
            root: root.to_path_buf(),
            committed_seq,
        })
    }

    /// The store directory path (`<root>/.arrow-kanban`) WITHOUT creating it.
    ///
    /// Unlike `persist::data_dir` (which creates the dir), resolution here is
    /// side-effect-free: creation is deferred to the first commit's write, so a
    /// read-only data ROOT still fails that commit honestly (the create fails)
    /// rather than being silently pre-created while probing the log.
    fn store_dir_path(root: &Path) -> PathBuf {
        // Mirrors `persist`'s private DATA_DIR const; the write path
        // (`append_and_fsync` / `save_all`) is what actually creates it.
        root.join(".arrow-kanban")
    }

    /// Scan the log for the maximum seq. Missing log → 0 (a pre-backend store).
    fn scan_high_water(root: &Path) -> Result<Seq, StoreError> {
        let dir = Self::store_dir_path(root);
        let path = dir.join(EVENT_LOG_FILE);
        if !path.exists() {
            return Ok(0);
        }
        let mut high = 0;
        for line in Self::read_lines(&path)? {
            let rec: LogLine =
                serde_json::from_str(&line).map_err(|e| StoreError::Decode(e.to_string()))?;
            high = high.max(rec.seq);
        }
        Ok(high)
    }

    /// Read the non-empty lines of the log file.
    fn read_lines(path: &Path) -> Result<Vec<String>, StoreError> {
        let file = fs::File::open(path).map_err(StoreError::Log)?;
        let reader = std::io::BufReader::new(file);
        let mut out = Vec::new();
        for line in reader.lines() {
            let line = line.map_err(StoreError::Log)?;
            if !line.trim().is_empty() {
                out.push(line);
            }
        }
        Ok(out)
    }

    /// The authoritative checkpoint high-water: the seq the derived parquet
    /// checkpoint reflects. Replay on load is strictly AFTER this.
    ///
    /// Resolution order, so both current and upgrading stores load correctly:
    /// 1. the [`CheckpointManifest`]'s `seq` (the current, authoritative record);
    /// 2. else the legacy [`CHECKPOINT_SEQ_FILE`] marker — a store written by the
    ///    PR-2 backend has a marker but no manifest, and trusting its recorded seq
    ///    is what stops the upgrade load from replaying the whole (already
    ///    checkpointed) tail;
    /// 3. else `0` — a pre-backend store (no manifest, no marker) has no log, so
    ///    "checkpoint reflects nothing past 0" plus "no committed tail" = no replay.
    fn read_checkpoint_high_water(dir: &Path) -> Seq {
        if let Ok(s) = fs::read_to_string(dir.join(CHECKPOINT_MANIFEST_FILE))
            && let Ok(m) = serde_json::from_str::<CheckpointManifest>(&s)
        {
            return m.seq;
        }
        fs::read_to_string(dir.join(CHECKPOINT_SEQ_FILE))
            .ok()
            .and_then(|s| s.trim().parse::<Seq>().ok())
            .unwrap_or(0)
    }

    /// Append one line to the event log and fsync BOTH the file and its
    /// containing directory — the commit barrier. Returns only after the write
    /// is durable.
    fn append_and_fsync(dir: &Path, line: &str) -> std::io::Result<()> {
        fs::create_dir_all(dir)?;
        let path = dir.join(EVENT_LOG_FILE);
        {
            let mut f = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            f.write_all(line.as_bytes())?;
            f.write_all(b"\n")?;
            f.flush()?;
            // fsync the file: without it the append can sit in the page cache and
            // an ENOSPC/crash surfaces only later — after we called the write committed.
            f.sync_all()?;
        }
        // fsync the DIRECTORY so the append's directory entry (and, for a
        // first-ever append, the file's existence) is itself durable. Skipping
        // this loses the whole log on a crash even though the file was synced.
        let dirf = fs::File::open(dir)?;
        dirf.sync_all()?;
        Ok(())
    }

    /// Atomically write the checkpoint manifest via tmp+rename — the FINAL step of
    /// a commit. The rename is what publishes "the parquet now reflects `seq`":
    /// a crash before it leaves the previous complete manifest in place, so the
    /// uncovered tail is replayed from there (a checkpointed event is never
    /// replayed). Best-effort at the call site (a failure only leaves the manifest
    /// trailing, which the idempotent replay tolerates).
    fn write_checkpoint_manifest(dir: &Path, seq: Seq) -> std::io::Result<()> {
        let manifest = CheckpointManifest {
            seq,
            schema_version: SCHEMA_VERSION,
            files: CHECKPOINT_FILES.iter().map(|s| s.to_string()).collect(),
        };
        let bytes = serde_json::to_vec(&manifest)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let path = dir.join(CHECKPOINT_MANIFEST_FILE);
        let tmp = dir.join(format!("{CHECKPOINT_MANIFEST_FILE}.tmp"));
        fs::write(&tmp, &bytes)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Current byte length of the log file (0 if absent) — the rollback point.
    fn log_len(dir: &Path) -> u64 {
        dir.join(EVENT_LOG_FILE)
            .metadata()
            .map(|m| m.len())
            .unwrap_or(0)
    }

    /// Roll the log back to `len`, dropping a just-appended (or partially
    /// appended) tail. Used when the checkpoint cannot be written, so a write the
    /// caller is told FAILED leaves no phantom committed event to resurrect on
    /// replay. Best-effort + fsync (a failed truncate at worst leaves a
    /// dedup-safe event that replay would re-apply idempotently).
    fn truncate_log(dir: &Path, len: u64) {
        if let Ok(f) = fs::OpenOptions::new()
            .write(true)
            .open(dir.join(EVENT_LOG_FILE))
        {
            let _ = f.set_len(len);
            let _ = f.sync_all();
        }
    }
}

impl StorageBackend for ParquetBackend {
    fn commit(
        &mut self,
        store: &KanbanStore,
        relations: &RelationsStore,
        epoch: Epoch,
        event: &MutationEvent,
    ) -> Result<Seq, StoreError> {
        let dir = Self::store_dir_path(&self.root);
        let seq = self.committed_seq + 1;
        let rollback_to = Self::log_len(&dir);

        // (1) THE BARRIER: append + fsync the sequenced event (and fsync the
        // directory), STAMPED with the committing writer's lease epoch. A crash
        // AFTER this returns but before the checkpoint lands leaves the event in
        // the log with no checkpoint — recovered by replay on load (a torn
        // checkpoint is not a lost write).
        let line = serde_json::to_string(&LogLine {
            seq,
            epoch,
            event: event.clone(),
        })
        .map_err(|e| StoreError::Decode(e.to_string()))?;
        if let Err(e) = Self::append_and_fsync(&dir, &line) {
            // A partial append must not leave a half-line the next open would
            // mis-decode — roll back to the pre-append length.
            Self::truncate_log(&dir, rollback_to);
            return Err(StoreError::Log(e));
        }

        // (2) Re-checkpoint the derived state via the existing atomic tmp+rename
        // path. If the checkpoint cannot be written IN-PROCESS (the disk that
        // could take a tiny append cannot take the multi-file snapshot, or the
        // store dir is unwritable), roll the event back off the log and refuse:
        // the caller is told the write did not land, so no phantom committed
        // event may resurrect it on a later replay. (A CRASH here is different —
        // the event stays and replay recovers it; the rollback is only for an
        // in-process failure we can report.)
        if let Err(e) = arrow_kanban::persist::save_all(&self.root, store, relations) {
            Self::truncate_log(&dir, rollback_to);
            return Err(StoreError::Checkpoint(e.to_string()));
        }

        // Both landed — advance the committed high-water. The manifest is the
        // FINAL step (written after the parquet, so it never over-claims): its
        // rename publishes "the parquet reflects `seq`". A crash before the rename
        // leaves the previous complete manifest, and the uncovered tail (including
        // this event) is replayed idempotently on load — no double-apply.
        self.committed_seq = seq;
        if let Err(e) = Self::write_checkpoint_manifest(&dir, seq) {
            eprintln!(
                "kanban: checkpoint-manifest update after commit seq={seq} failed ({e}); the \
                 uncovered tail will be replayed idempotently on load."
            );
        }

        Ok(seq)
    }

    fn load(&mut self) -> Result<(LoadedState, Seq), StoreError> {
        // The checkpoint: byte-identical to the pre-backend load path, so a store
        // written before this backend loads unchanged (no event log → no replay).
        let (mut store, mut relations) = arrow_kanban::persist::load_all(&self.root)
            .map_err(|e| StoreError::Checkpoint(e.to_string()))?;

        let dir = Self::store_dir_path(&self.root);
        let checkpoint_seq = Self::read_checkpoint_high_water(&dir);
        let committed_seq = Self::scan_high_water(&self.root)?;
        self.committed_seq = committed_seq;

        // Replay the committed tail the checkpoint does not yet reflect. In steady
        // state the manifest is written every commit, so this is empty; it is
        // non-empty only after a torn/trailing checkpoint, and rebuilding it here
        // is what makes that a recovered write rather than a lost or doubled one
        // (the replay of each event kind is idempotent — see `replay_into`).
        if committed_seq > checkpoint_seq {
            for committed in self.events_since(checkpoint_seq)? {
                committed
                    .event
                    .replay_into(&mut store, &mut relations)
                    .map_err(StoreError::Checkpoint)?;
            }
        }

        Ok((LoadedState { store, relations }, committed_seq))
    }

    fn events_since(&self, after: Seq) -> Result<Vec<CommittedEvent>, StoreError> {
        Self::read_events_since(&self.root, after)
    }

    fn committed_seq(&self) -> Seq {
        self.committed_seq
    }
}

impl ParquetBackend {
    /// Path-read the committed events with `seq > after`, in seq order — WITHOUT a
    /// live backend instance. The append-only log is the sole input, so this is a
    /// pure read of durable state: the outbox drain calls it directly (never
    /// through the engine-owning actor task), so re-publishing the committed tail
    /// never contends with the write path.
    pub fn read_events_since(root: &Path, after: Seq) -> Result<Vec<CommittedEvent>, StoreError> {
        let dir = Self::store_dir_path(root);
        let path = dir.join(EVENT_LOG_FILE);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for line in Self::read_lines(&path)? {
            let rec: LogLine =
                serde_json::from_str(&line).map_err(|e| StoreError::Decode(e.to_string()))?;
            if rec.seq > after {
                out.push(CommittedEvent {
                    seq: rec.seq,
                    epoch: rec.epoch,
                    event: rec.event,
                });
            }
        }
        out.sort_by_key(|c| c.seq);
        Ok(out)
    }

    /// Path-read the committed high-water (the log's max seq) WITHOUT a live
    /// backend — the pure-read companion to [`read_events_since`](Self::read_events_since).
    pub fn read_committed_seq(root: &Path) -> Result<Seq, StoreError> {
        Self::scan_high_water(root)
    }
}
