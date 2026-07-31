// SPDX-License-Identifier: MIT
//! A read-only, embeddable replica that tails the committed log and fails closed.
//!
//! [`Replica`] is the read-side mirror of the write-side actor in
//! [`crate::actor`]: where the actor installs a fresh
//! [`AggregateSnapshot`](crate::snapshot::AggregateSnapshot) after each durable
//! commit and answers reads as a lock-free `Arc` clone, a replica installs a fresh
//! snapshot after each durable ADVANCE it observes on the source's committed log,
//! and answers reads the same way. It issues no commands, produces no reply bytes,
//! changes no on-disk format — it is a pure additive read side.
//!
//! # What it guarantees
//!
//! A replica exists to be *safe to read from* without silently serving stale data:
//!
//! - **It tails the DURABLE committed log** — the append-only change log the writer
//!   fsyncs, read through the same pure path-reads the outbox uses
//!   ([`ParquetBackend::read_events_since`](crate::storage::ParquetBackend::read_events_since)
//!   / [`read_committed_seq`](crate::storage::ParquetBackend::read_committed_seq)) —
//!   NOT a lossy notification stream. Applying an event is
//!   [`MutationEvent::replay_into`](crate::events::MutationEvent::replay_into), the
//!   same idempotent replay the backend's own crash recovery uses.
//! - **It fails closed on a gap.** The tail must be contiguous from the last applied
//!   position. A hole (a dropped event), a tail that starts past the next expected
//!   position (the log tail was reclaimed), or an empty tail while the source is
//!   ahead all trigger a **re-bootstrap** from a fresh complete snapshot at the
//!   source high-water — which leapfrogs the missing span rather than applying an
//!   event onto a state that never saw its predecessor.
//! - **It refuses reads once it cannot confirm currency within a declared bound.**
//!   A [`StalenessBound`] declares how long the replica may go without confirming it
//!   is current (and, optionally, how many positions behind it may fall). During a
//!   partition — the currency probe returns `Err`, so the lag is *unmeasurable* — a
//!   replica that has passed its bound flips to [`ReplicaState::StaleRefused`] and
//!   [`read`](Replica::read) returns [`ReplicaError::Stale`] instead of handing back
//!   a coherent-but-old snapshot labelled current. This is the structural
//!   enforcement of "never silently serves stale-as-current".
//! - **It detects a split source.** Every committed event carries the writer-lease
//!   epoch that produced it; the epoch is monotonic across increasing positions, so
//!   an epoch that REGRESSES across the tail is two writer generations interleaved —
//!   a split brain — and the replica fails closed rather than applying it.
//!
//! # The source seam
//!
//! [`ReplicaSource`] is the interface a deployment reads its canonical log through —
//! exactly as [`StorageBackend`](crate::storage::StorageBackend) is the write-side
//! durability seam. The shipped default, [`LocalReplicaSource`], is a same-host,
//! pure path-read over the writer's data directory: it never contends with the
//! writer's owning task, because it reads the durable files directly. A different
//! deployment (a log shipped over a network) supplies a different `ReplicaSource`
//! WITHOUT touching the replica — that is a documented implementation slot, not a
//! forced dependency.
//!
//! # No new dependency, deterministic time
//!
//! The staleness bound is a wall-clock comparison, so the clock is INJECTABLE (a
//! plain closure, the same seam idiom the writer lease uses for its liveness
//! predicate). Tests drive it deterministically; production defaults it to the wall
//! clock. There are no timers, no sleeps, and no new crate.

use crate::events::MutationEvent;
use crate::extension::EngineExtension;
use crate::lease::Epoch;
use crate::snapshot::AggregateSnapshot;
use crate::storage::{self, CommittedEvent, ParquetBackend, Seq, StorageBackend};
use arrow_kanban::crud::KanbanStore;
use arrow_kanban::relations::RelationsStore;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A monotonic wall clock in milliseconds since the unix epoch, boxed so it can be
/// injected. Production defaults to [`wall_clock_ms`]; a test supplies a closure
/// over a counter it controls, so the staleness logic is exercised without sleeps.
type ClockFn = Box<dyn Fn() -> u64 + Send>;

/// The default clock: real milliseconds since the unix epoch.
fn wall_clock_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// How current a replica must stay to keep answering reads.
///
/// The TIME bound is load-bearing: it is what flips a replica to
/// [`ReplicaState::StaleRefused`] during a partition, when the lag cannot be
/// measured because the currency probe itself is failing. The optional position
/// bound is a second, measurable ceiling: even while the source is reachable, a
/// replica that has fallen more than `max_lag_seqs` behind and cannot catch up
/// refuses rather than serving a snapshot known to be far behind.
#[derive(Debug, Clone)]
pub struct StalenessBound {
    /// The longest a replica may go without CONFIRMING it is current before it must
    /// refuse reads. Confirmation is a currency probe that either shows the replica
    /// level with the source or lets it catch up.
    pub max_staleness: Duration,
    /// An optional ceiling on how many committed positions behind the source a
    /// replica may fall while still answering reads. `None` = no position ceiling
    /// (the time bound alone governs).
    pub max_lag_seqs: Option<Seq>,
}

impl StalenessBound {
    /// A time-only bound (no position ceiling) — the common case.
    pub fn of(max_staleness: Duration) -> Self {
        StalenessBound {
            max_staleness,
            max_lag_seqs: None,
        }
    }
}

/// Why a replica entered [`ReplicaState::Resyncing`] — recorded for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GapReason {
    /// A hole in the tail: `found` arrived where `expected` was due.
    DroppedEvent { expected: Seq, found: Seq },
    /// The tail no longer reaches the replica's position — the earliest available
    /// event is `first_available`, past `have + 1` (the log tail was reclaimed).
    RetentionExpired { have: Seq, first_available: Seq },
}

/// The observable state of a replica.
#[derive(Debug, Clone)]
pub enum ReplicaState {
    /// Current: the last confirmation succeeded and the published snapshot reflects
    /// the source high-water the replica has applied. Reads are served.
    Live,
    /// A gap was detected at `from_seq`; the replica is re-bootstrapping to leapfrog
    /// it. Reads are served ONLY while still provably within the staleness bound.
    Resyncing { from_seq: Seq, reason: GapReason },
    /// Currency could not be confirmed within the declared bound (a partition, an
    /// unhealable gap, or a split source). Reads are REFUSED — the fail-closed state.
    StaleRefused {
        /// Human-readable cause.
        reason: String,
        /// When currency was last confirmed (millis since the unix epoch).
        last_confirmed: u64,
    },
}

/// A typed replica error.
#[derive(Debug)]
pub enum ReplicaError {
    /// A read was refused because the replica cannot confirm it is current within
    /// its staleness bound.
    Stale {
        reason: String,
        last_confirmed_ms: u64,
        now_ms: u64,
    },
    /// The source could not be reached (the model of a partition).
    SourceUnreachable(String),
    /// The source's bootstrap snapshot carries a table-contract version this replica
    /// was not built for — fail closed rather than misread the tables.
    SchemaMismatch { expected: u32, found: u32 },
    /// A gap in the committed log could not be healed (a re-bootstrap failed while
    /// the replica was already beyond its bound).
    Gap { expected: Seq, found: Seq },
    /// The committed epoch regressed across increasing positions — two writer
    /// generations interleaved (a split brain).
    SplitBrain {
        at_seq: Seq,
        epoch_from: Epoch,
        epoch_to: Epoch,
    },
    /// A committed event could not be replayed onto the replica's tables.
    Replay(String),
}

impl std::fmt::Display for ReplicaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplicaError::Stale {
                reason,
                last_confirmed_ms,
                now_ms,
            } => write!(
                f,
                "read REFUSED — replica is stale ({reason}); last confirmed at {last_confirmed_ms} ms, \
                 now {now_ms} ms"
            ),
            ReplicaError::SourceUnreachable(m) => write!(f, "replica source unreachable: {m}"),
            ReplicaError::SchemaMismatch { expected, found } => write!(
                f,
                "replica schema mismatch: built for table-contract version {expected}, source is {found}"
            ),
            ReplicaError::Gap { expected, found } => {
                write!(
                    f,
                    "unhealable gap in the committed log: expected {expected}, found {found}"
                )
            }
            ReplicaError::SplitBrain {
                at_seq,
                epoch_from,
                epoch_to,
            } => write!(
                f,
                "split source at position {at_seq}: epoch regressed {epoch_from} -> {epoch_to} \
                 (two writer generations interleaved)"
            ),
            ReplicaError::Replay(m) => write!(f, "replica replay failed: {m}"),
        }
    }
}

impl std::error::Error for ReplicaError {}

/// The canonical committed log a replica tails.
///
/// A `ReplicaSource` is READ-ONLY: it exposes the durable change log, a currency
/// probe, and a fresh complete snapshot for (re-)bootstrap. It never accepts a
/// write. `Err` from any method models the source being unreachable (a partition),
/// which the replica handles by falling back to its staleness bound.
pub trait ReplicaSource {
    /// The committed events with position strictly greater than `after`, in position
    /// order — the DURABLE change log, not a lossy notification stream.
    fn events_since(&self, after: Seq) -> Result<Vec<CommittedEvent>, ReplicaError>;

    /// The source's current committed high-water — the currency probe. `Err` models
    /// a partition (the lag is then unmeasurable, and the time bound governs).
    fn canonical_committed_seq(&self) -> Result<Seq, ReplicaError>;

    /// A fresh, complete snapshot at the source high-water — used for the initial
    /// start AND for re-bootstrap after a gap, where it LEAPFROGS a missing or
    /// reclaimed log tail rather than applying an event onto a state that never saw
    /// its predecessor.
    fn bootstrap(&self) -> Result<AggregateSnapshot, ReplicaError>;

    /// The local store directory an extension-bound replica restores its own
    /// checkpoint from, or `None` for a source with no local disk (the extension is
    /// then seeded by replaying the whole committed tail). The default is `None` —
    /// the open replica (no extension) never consults it.
    fn store_dir(&self) -> Option<PathBuf> {
        None
    }
}

/// A read-only replica of the committed board.
///
/// It holds the core stores it rebuilds by replay, an OPTIONAL extension bound the
/// same way, the position it has applied to, the published snapshot readers clone,
/// and the machinery for the staleness bound. `None` extension is the default (an
/// open replica — the primary configuration); an extension-bound replica seeds and
/// tails its extension's tables exactly as the write-side engine does on load.
pub struct Replica<S: ReplicaSource> {
    /// items + runs + item-comments, rebuilt by replay.
    store: KanbanStore,
    /// typed relations, rebuilt by replay.
    relations: RelationsStore,
    /// An optional extension whose tables are seeded and tailed alongside the core.
    /// `None` = the open replica (the default): zero extension events applied.
    extension: Option<Box<dyn EngineExtension + Send>>,
    /// The committed high-water this replica has APPLIED.
    seq: Seq,
    /// The table-contract version, gated against this build's at bootstrap.
    schema_version: u32,
    /// The latest published snapshot — a reader clones this `Arc`.
    snapshot: Arc<AggregateSnapshot>,
    /// The observable state (drives whether `read` serves or refuses).
    state: ReplicaState,
    /// The read-only source of the canonical committed log.
    source: S,
    /// The declared staleness bound.
    bound: StalenessBound,
    /// Injectable wall clock (millis since the unix epoch).
    clock: ClockFn,
    /// When currency was last confirmed (millis since the unix epoch).
    last_confirmed_at: u64,
    /// The epoch of the last applied event — the split-brain baseline. Monotonic, so
    /// a lower epoch on a later position is a regression.
    last_epoch: Epoch,
    /// Whether the contiguity/gap guard is armed. `true` is the only correct
    /// production value; a `false` build is the battery's RED control — it would
    /// silently apply an event past a gap, which is exactly the hazard the guard
    /// exists to prevent.
    detect_gaps: bool,
}

impl<S: ReplicaSource> Replica<S> {
    /// Open a read-only replica (no extension) over `source`, bootstrapping it to
    /// the source high-water and arming the gap guard. Uses the wall clock.
    pub fn open(source: S, bound: StalenessBound) -> Result<Self, ReplicaError> {
        Self::with_clock(source, bound, None, true, wall_clock_ms)
    }

    /// Open a replica whose `extension`'s tables are seeded and tailed alongside the
    /// core, exactly as the write-side engine restores an extension on load.
    pub fn with_extension(
        source: S,
        bound: StalenessBound,
        extension: Box<dyn EngineExtension + Send>,
    ) -> Result<Self, ReplicaError> {
        Self::with_clock(source, bound, Some(extension), true, wall_clock_ms)
    }

    /// The fully-explicit constructor — the test seam (mirrors the writer lease's
    /// injectable-predicate constructor). `clock` drives the staleness comparison
    /// deterministically; `detect_gaps=false` is the battery's RED control, never a
    /// production value.
    pub fn with_clock<F>(
        source: S,
        bound: StalenessBound,
        extension: Option<Box<dyn EngineExtension + Send>>,
        detect_gaps: bool,
        clock: F,
    ) -> Result<Self, ReplicaError>
    where
        F: Fn() -> u64 + Send + 'static,
    {
        let mut replica = Replica {
            store: KanbanStore::new(),
            relations: RelationsStore::new(),
            extension,
            seq: 0,
            schema_version: storage::SCHEMA_VERSION,
            snapshot: Arc::new(AggregateSnapshot::empty()),
            state: ReplicaState::Live,
            source,
            bound,
            clock: Box::new(clock),
            last_confirmed_at: 0,
            last_epoch: 0,
            detect_gaps,
        };
        replica.bootstrap_state()?;
        Ok(replica)
    }

    // ── read side ────────────────────────────────────────────────────────────

    /// Read the current snapshot — an `Arc` clone, no source round trip.
    ///
    /// Served in [`Live`](ReplicaState::Live), and in
    /// [`Resyncing`](ReplicaState::Resyncing) ONLY while still provably within the
    /// staleness bound. In [`StaleRefused`](ReplicaState::StaleRefused) it returns
    /// [`ReplicaError::Stale`] — the structural refusal that keeps a replica from
    /// ever silently serving stale-as-current.
    pub fn read(&self) -> Result<Arc<AggregateSnapshot>, ReplicaError> {
        match &self.state {
            ReplicaState::Live => Ok(self.snapshot.clone()),
            ReplicaState::Resyncing { .. } => {
                if self.within_staleness_bound() {
                    Ok(self.snapshot.clone())
                } else {
                    Err(self.stale_err("resyncing past the staleness bound"))
                }
            }
            ReplicaState::StaleRefused {
                reason,
                last_confirmed,
            } => Err(ReplicaError::Stale {
                reason: reason.clone(),
                last_confirmed_ms: *last_confirmed,
                now_ms: (self.clock)(),
            }),
        }
    }

    /// The current observable state.
    pub fn state(&self) -> &ReplicaState {
        &self.state
    }

    /// The committed high-water this replica has applied.
    pub fn seq(&self) -> Seq {
        self.seq
    }

    /// The table-contract version this replica bootstrapped at.
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Whether reads are currently served (a convenience over [`state`](Self::state)).
    pub fn is_live(&self) -> bool {
        matches!(self.state, ReplicaState::Live)
    }

    // ── catch-up ─────────────────────────────────────────────────────────────

    /// Probe the source and apply any committed tail — the single catch-up step.
    ///
    /// Returns `Ok(())` for the normal outcomes (level, caught up, re-bootstrapped,
    /// or a soft transition into [`Resyncing`](ReplicaState::Resyncing) /
    /// [`StaleRefused`](ReplicaState::StaleRefused) whose effect `read` enforces),
    /// and `Err` for a hard fail-closed condition (a split source, a schema
    /// mismatch, an unhealable gap past bound, a replay failure). Call it on a timer
    /// or before a read; it holds no lock and issues no write.
    pub fn poll(&mut self) -> Result<(), ReplicaError> {
        // (1) The currency probe. `Err` = a partition: the lag is unmeasurable, so
        // only the time bound can govern.
        let canonical = match self.source.canonical_committed_seq() {
            Ok(c) => c,
            Err(_) => return self.on_unreachable(),
        };

        // (2) A canonical high-water BELOW ours is a source rewind / split — the log
        // it claims cannot be an extension of the one we applied. Fail closed.
        if canonical < self.seq {
            self.enter_stale(format!(
                "source high-water {canonical} is behind the applied position {} — a source rewind",
                self.seq
            ));
            return Err(ReplicaError::SplitBrain {
                at_seq: canonical,
                epoch_from: self.last_epoch,
                epoch_to: self.last_epoch,
            });
        }

        // (3) Level with the source — confirm and return.
        if canonical == self.seq {
            self.mark_confirmed();
            return Ok(());
        }

        // (4) Behind: fetch the tail we still need.
        let tail = match self.source.events_since(self.seq) {
            Ok(t) => t,
            Err(_) => return self.on_unreachable(),
        };

        // (5) Gap guard: the tail must be contiguous from `seq + 1`. A hole, a tail
        // that starts late (reclaimed), or an empty tail while the source is ahead
        // is a gap — re-bootstrap to leapfrog it rather than apply past it. Disarming
        // this guard (the RED control) would silently apply the tail over the hole.
        if self.detect_gaps
            && let Some(reason) = detect_gap(&tail, self.seq)
        {
            return self.resync(reason, canonical);
        }

        // (6) Apply the contiguous tail (which also enforces epoch monotonicity).
        self.apply_tail(tail)?;
        self.mark_confirmed();
        Ok(())
    }

    // ── internals ──────────────────────────────────────────────────────────────

    /// (Re-)bootstrap: seed the core from a fresh complete source snapshot, gate the
    /// schema, seed the extension, and publish. Used for the initial open and to
    /// leapfrog a gap. Sets [`Live`](ReplicaState::Live) and confirms currency.
    fn bootstrap_state(&mut self) -> Result<(), ReplicaError> {
        let snap = self.source.bootstrap()?;
        // Schema gate: fail closed rather than read tables under a contract this
        // build was not compiled for.
        if snap.schema_version != storage::SCHEMA_VERSION {
            let err = ReplicaError::SchemaMismatch {
                expected: storage::SCHEMA_VERSION,
                found: snap.schema_version,
            };
            self.enter_stale(err.to_string());
            return Err(err);
        }
        self.seq = snap.seq;
        self.schema_version = snap.schema_version;
        self.store = snap.store;
        self.relations = snap.relations;
        // The bootstrap snapshot fixes the applied position; the last-applied epoch
        // baseline resets to 0 (the snapshot carries no epoch), so the split-brain
        // check keys off tail monotonicity from here.
        self.last_epoch = 0;
        self.seed_extension()?;
        self.mark_confirmed();
        self.publish();
        Ok(())
    }

    /// Seed the extension's tables — the read-side mirror of the engine's load-time
    /// restore: load its own checkpoint (if a local dir is available), then replay
    /// every committed extension event past the checkpoint position. A no-op for the
    /// open replica.
    fn seed_extension(&mut self) -> Result<(), ReplicaError> {
        if self.extension.is_none() {
            return Ok(());
        }
        // (1) Restore the extension's checkpoint from a local dir if the source has
        // one; otherwise start from 0 and replay the whole tail.
        let ext_seq = match self.source.store_dir() {
            Some(dir) => self
                .extension
                .as_mut()
                .expect("extension present")
                .restore(&dir)
                .map_err(ReplicaError::Replay)?,
            None => 0,
        };
        // (2) The committed tail the checkpoint does not yet reflect.
        let tail = self.source.events_since(ext_seq)?;
        // (3) Replay each EXTENSION event onto the extension's tables (core events
        // belong to the core store and are skipped here), idempotently.
        let ext = self.extension.as_mut().expect("extension present");
        for committed in tail {
            if let MutationEvent::Extension {
                namespace,
                kind,
                payload,
            } = committed.event
            {
                ext.replay(&namespace, &kind, &payload)
                    .map_err(ReplicaError::Replay)?;
            }
        }
        Ok(())
    }

    /// Apply a contiguous committed tail in position order, after proving the epoch
    /// does not regress across it (a split-brain check). The epoch scan runs FIRST,
    /// so a split source leaves NO partial application.
    fn apply_tail(&mut self, tail: Vec<CommittedEvent>) -> Result<(), ReplicaError> {
        // Epoch monotonicity: the committed epoch never decreases as the position
        // increases; a decrease is two writer generations interleaved.
        let mut running = self.last_epoch;
        for committed in &tail {
            if committed.epoch < running {
                let err = ReplicaError::SplitBrain {
                    at_seq: committed.seq,
                    epoch_from: running,
                    epoch_to: committed.epoch,
                };
                self.enter_stale(err.to_string());
                return Err(err);
            }
            running = committed.epoch;
        }
        // Apply. A core event replays onto the core stores; an extension event onto
        // the held extension (skipped for an open replica, exactly as core replay does).
        for committed in tail {
            match &committed.event {
                MutationEvent::Extension {
                    namespace,
                    kind,
                    payload,
                } => {
                    if let Some(ext) = self.extension.as_mut() {
                        ext.replay(namespace, kind, payload)
                            .map_err(ReplicaError::Replay)?;
                    }
                }
                core => {
                    core.replay_into(&mut self.store, &mut self.relations)
                        .map_err(ReplicaError::Replay)?;
                }
            }
            self.seq = committed.seq;
            self.last_epoch = committed.epoch;
        }
        self.publish();
        Ok(())
    }

    /// A gap was detected: re-bootstrap to leapfrog it. On success the replica is
    /// Live at the source high-water; if the source cannot supply a fresh snapshot
    /// right now, stay [`Resyncing`](ReplicaState::Resyncing) while within bounds, or
    /// fail closed once beyond them.
    fn resync(&mut self, reason: GapReason, canonical: Seq) -> Result<(), ReplicaError> {
        self.state = ReplicaState::Resyncing {
            from_seq: self.seq,
            reason: reason.clone(),
        };
        match self.bootstrap_state() {
            Ok(()) => Ok(()),
            Err(ReplicaError::SchemaMismatch { .. }) | Err(ReplicaError::Replay(_)) => {
                // A fatal re-bootstrap failure (already recorded as StaleRefused by
                // bootstrap_state) propagates.
                Err(self.last_stale_as_error())
            }
            Err(_unreachable) => {
                // The source is momentarily unreachable for a fresh snapshot. Tolerate
                // it only while within BOTH bounds; otherwise the gap is unhealable.
                if self.beyond_bounds(canonical) {
                    let (expected, found) = match reason {
                        GapReason::DroppedEvent { expected, found } => (expected, found),
                        GapReason::RetentionExpired {
                            have,
                            first_available,
                        } => (have + 1, first_available),
                    };
                    self.enter_stale(format!(
                        "gap at position {expected} could not be healed (source unreachable) and the \
                         replica is beyond its staleness/lag bound"
                    ));
                    Err(ReplicaError::Gap { expected, found })
                } else {
                    // Within bounds: remain Resyncing on the last known-good snapshot.
                    Ok(())
                }
            }
        }
    }

    /// Handle an unreachable source (a partition). Stay on the last known-good
    /// snapshot while within the time bound; flip to
    /// [`StaleRefused`](ReplicaState::StaleRefused) once past it.
    fn on_unreachable(&mut self) -> Result<(), ReplicaError> {
        if !self.within_staleness_bound() {
            self.enter_stale(
                "source unreachable for longer than the declared staleness bound".to_string(),
            );
        }
        Ok(())
    }

    /// Publish a fresh snapshot bound to the current applied position (and the
    /// extension's table bind, if any) — the Arc-swap readers observe.
    fn publish(&mut self) {
        let ext_bind = self.extension.as_ref().and_then(|x| x.snapshot());
        self.snapshot = Arc::new(AggregateSnapshot::new(
            &self.store,
            &self.relations,
            self.seq,
            ext_bind,
        ));
    }

    /// Mark currency confirmed at the current clock and become Live.
    fn mark_confirmed(&mut self) {
        self.last_confirmed_at = (self.clock)();
        self.state = ReplicaState::Live;
    }

    /// Enter the fail-closed state with a cause, preserving the last-confirmed time.
    fn enter_stale(&mut self, reason: String) {
        self.state = ReplicaState::StaleRefused {
            reason,
            last_confirmed: self.last_confirmed_at,
        };
    }

    /// Whether the replica is still within its time bound (has confirmed currency
    /// recently enough).
    fn within_staleness_bound(&self) -> bool {
        let now = (self.clock)();
        let elapsed = now.saturating_sub(self.last_confirmed_at) as u128;
        elapsed <= self.bound.max_staleness.as_millis()
    }

    /// Whether the replica is beyond EITHER bound given a known source high-water.
    fn beyond_bounds(&self, canonical: Seq) -> bool {
        if !self.within_staleness_bound() {
            return true;
        }
        match self.bound.max_lag_seqs {
            Some(limit) => canonical.saturating_sub(self.seq) > limit,
            None => false,
        }
    }

    /// Build a [`ReplicaError::Stale`] for the current instant.
    fn stale_err(&self, reason: &str) -> ReplicaError {
        ReplicaError::Stale {
            reason: reason.to_string(),
            last_confirmed_ms: self.last_confirmed_at,
            now_ms: (self.clock)(),
        }
    }

    /// The current StaleRefused state, as an error (for propagating a fatal
    /// re-bootstrap failure that bootstrap_state already recorded).
    fn last_stale_as_error(&self) -> ReplicaError {
        match &self.state {
            ReplicaState::StaleRefused { reason, .. } => self.stale_err(reason),
            _ => self.stale_err("replica failed closed"),
        }
    }
}

/// Detect a gap in a committed tail relative to the last applied position `seq`.
///
/// The tail is a gap when it does not begin exactly at `seq + 1` and run
/// contiguous: an empty tail while the source is ahead means the tail we need is
/// gone; a first position past `seq + 1` means the log tail was reclaimed; a hole
/// mid-tail is a dropped event. `None` = a clean contiguous tail.
fn detect_gap(tail: &[CommittedEvent], seq: Seq) -> Option<GapReason> {
    let Some(first) = tail.first() else {
        // Caller only invokes this when the source is AHEAD of `seq`, so an empty
        // tail means the events we need are no longer available.
        return Some(GapReason::RetentionExpired {
            have: seq,
            first_available: 0,
        });
    };
    if first.seq != seq + 1 {
        return Some(GapReason::RetentionExpired {
            have: seq,
            first_available: first.seq,
        });
    }
    let mut prev = first.seq;
    for c in &tail[1..] {
        if c.seq != prev + 1 {
            return Some(GapReason::DroppedEvent {
                expected: prev + 1,
                found: c.seq,
            });
        }
        prev = c.seq;
    }
    None
}

/// The shipped default source: a same-host, pure path-read over the writer's data
/// directory.
///
/// Every method is a durable path-read that never contends with the writer's owning
/// task — the same reads the outbox drain uses. `bootstrap` loads the checkpoint
/// and replays its committed tail (the backend's own recovery), binding NO extension
/// (an extension-bound local replica seeds its tables via
/// [`store_dir`](ReplicaSource::store_dir) + replay). It changes no on-disk format
/// and adds no write path.
pub struct LocalReplicaSource {
    /// The data ROOT the writer commits under (the store lives at
    /// `<root>/.arrow-kanban`).
    root: PathBuf,
}

impl LocalReplicaSource {
    /// A source rooted at the writer's data ROOT (what the writer's `--data-dir`
    /// points at).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        LocalReplicaSource { root: root.into() }
    }
}

impl ReplicaSource for LocalReplicaSource {
    fn events_since(&self, after: Seq) -> Result<Vec<CommittedEvent>, ReplicaError> {
        ParquetBackend::read_events_since(&self.root, after)
            .map_err(|e| ReplicaError::SourceUnreachable(e.to_string()))
    }

    fn canonical_committed_seq(&self) -> Result<Seq, ReplicaError> {
        ParquetBackend::read_committed_seq(&self.root)
            .map_err(|e| ReplicaError::SourceUnreachable(e.to_string()))
    }

    fn bootstrap(&self) -> Result<AggregateSnapshot, ReplicaError> {
        let mut backend = ParquetBackend::open(&self.root)
            .map_err(|e| ReplicaError::SourceUnreachable(e.to_string()))?;
        let (loaded, seq) = backend
            .load()
            .map_err(|e| ReplicaError::SourceUnreachable(e.to_string()))?;
        Ok(AggregateSnapshot::new(
            &loaded.store,
            &loaded.relations,
            seq,
            None,
        ))
    }

    fn store_dir(&self) -> Option<PathBuf> {
        Some(crate::health::store_dir(&self.root))
    }
}
