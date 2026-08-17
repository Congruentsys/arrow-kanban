// SPDX-License-Identifier: MIT
//! The command-extension seam — a namespaced verb family bolted onto the engine.
//!
//! The core [`KanbanEngine`](crate::engine::KanbanEngine) offers a *closed* verb
//! set (`create`, `move`, …). An [`EngineExtension`] adds a SECOND, namespaced
//! verb family (`ext.<namespace>.<verb>`) with its own tables, WITHOUT widening
//! the core: the open engine (no extension) offers zero extension verbs and emits
//! zero extension events.
//!
//! # The write path (this module, 3c-i)
//!
//! An extension declares, per verb:
//! - [`classify`](EngineExtension::classify) — does this extension own the verb,
//!   and is it a mutation? A `None` means "not mine"; the engine then reports the
//!   usual `UNKNOWN_COMMAND`. This is consulted ONLY for a verb the core `parse`
//!   did not recognise, so an extension can never shadow a core verb.
//! - [`apply`](EngineExtension::apply) — run the verb against the extension's own
//!   (in-memory, for 3c-i) tables and return a [`KanbanReply`]. The engine
//!   dispatches through [`apply_with_core`](EngineExtension::apply_with_core),
//!   whose default delegates here — override it to also read the core tables
//!   ([`CoreRead`]) while deciding the verb.
//! - [`derive_event`](EngineExtension::derive_event) — the committed
//!   [`MutationEvent::Extension`] a successful mutation logs.
//! - [`replay`](EngineExtension::replay) — rebuild the extension's tables from a
//!   logged event (exercised directly here; the engine's load-time replay
//!   orchestration lands with the persistence half, 3c-ii).
//!
//! A classified mutation flows through the EXACT SAME commit boundary as a core
//! one — health-gate admission, writer-lease fence, `backend.commit`, seq, outbox
//! — so a fenced or degraded extension mutation leaves no table change, no event,
//! and no seq advance, identically to core.
//!
//! # The persistence path (3c-ii)
//!
//! 3c-i left an extension's events durable in the commit log but not restored on
//! load. 3c-ii completes the seam with three ADDITIVE, defaulted methods, so no
//! existing `impl EngineExtension` has to change:
//! - [`checkpoint`](EngineExtension::checkpoint) — a derived, engine-orchestrated
//!   POST-commit step (like the core `_checkpoint.manifest`): write the extension's
//!   own table parquet(s) via tmp+rename and return the file names covered. The
//!   [`StorageBackend`](crate::storage::StorageBackend) is UNCHANGED — the ext
//!   checkpoint rides alongside the core manifest write, never through the backend.
//! - [`restore`](EngineExtension::restore) — load the ext checkpoint on startup and
//!   return the seq it reflects (`0` if none). The engine then replays the ext
//!   event tail with `seq > ext_seq` through [`replay`](EngineExtension::replay), so
//!   a torn/absent ext checkpoint is recovered from the log — not lost, not doubled.
//! - [`snapshot`](EngineExtension::snapshot) — an O(1) `Arc` bind of the current ext
//!   tables into the [`AggregateSnapshot`](crate::snapshot::AggregateSnapshot), read
//!   back by a consumer through a `dyn Any` downcast to its OWN snapshot type.
//!
//! The three methods deal ONLY in `dir`/`seq`/file-name strings + `dyn Any`; the
//! public crate never names a consumer table. Each has a safe default (no
//! checkpoint, restore-from-zero, no snapshot), so the seam widens without
//! disturbing the write-path impls.

use crate::engine::KanbanReply;
use crate::events::MutationEvent;
use crate::storage::Seq;
use arrow_kanban::crud::KanbanStore;
use arrow_kanban::relations::RelationsStore;
use std::path::Path;
use std::sync::Arc;

/// A read-only, borrowed view of the core tables at the point of dispatch.
///
/// Handed to [`EngineExtension::apply_with_core`] so an extension can consult
/// core state (does this item exist? what status is it in? what relations does
/// it carry?) while deciding its own verb — the capability whose absence forced
/// downstream deployments to re-implement read paths around the engine instead
/// of through this seam.
///
/// Three properties, all structural rather than conventional:
/// - **Read-only:** shared references with no interior mutability — there is no
///   way to reach a mutation through this view.
/// - **Coherent:** the view is the live pre-commit state of the very dispatch in
///   flight — exactly what a core handler in the same dispatch would see, one
///   state, never torn.
/// - **Un-storable:** the borrow ends with the `apply_with_core` call, so an
///   extension cannot retain the view past the dispatch (retained reads belong
///   to [`AggregateSnapshot`](crate::snapshot::AggregateSnapshot), which is the
///   post-commit, `Arc`-shared read surface).
pub struct CoreRead<'a> {
    /// items + runs + item-comments, live at this dispatch.
    pub store: &'a KanbanStore,
    /// typed relations, live at this dispatch.
    pub relations: &'a RelationsStore,
}

/// A namespaced command extension: one verb family with its own tables, routed
/// through the engine's commit boundary.
///
/// Methods take `&self`/`&mut self` on the extension's OWN state — the engine
/// holds the extension and never reaches inside it. The trait is the whole seam;
/// it carries no MANDATORY core vocabulary, so an extension is free to be
/// anything from a review workflow to a scratch demo (see the write-path
/// acceptance battery): core state is OFFERED read-only through
/// [`apply_with_core`](Self::apply_with_core)'s [`CoreRead`] view, and an
/// extension that implements only [`apply`](Self::apply) never names a core
/// type — the defaulted method drops the view on its way through.
pub trait EngineExtension {
    /// Does this extension own `verb`, and if so is it a mutation? `None` means
    /// "not this extension's verb" — the engine falls back to `UNKNOWN_COMMAND`.
    ///
    /// The engine consults this ONLY for a verb its own `parse` did not recognise,
    /// so a `Some(..)` for a core verb can never intercept it — the core wins by
    /// construction.
    fn classify(&self, verb: &str) -> Option<ExtVerbSpec>;

    /// Run `verb` (with its raw `payload`) against the extension's own tables.
    /// `Ok` is the success reply; `Err` is a typed error reply (an
    /// [`INVALID_PAYLOAD`](KanbanReply::error) / domain error the extension
    /// produced). Called only after [`classify`](Self::classify) claimed the verb.
    fn apply(&mut self, verb: &str, payload: &[u8]) -> Result<KanbanReply, KanbanReply>;

    /// [`apply`](Self::apply) with a read-only view of the core tables — the
    /// variant the engine actually dispatches through.
    ///
    /// The default delegates to `apply`, dropping the view, so every existing
    /// impl (and any extension that wants nothing from core) keeps its exact
    /// behaviour without naming a core type — the same additive-defaulted
    /// widening the persistence trio (`checkpoint`/`restore`/`snapshot`) used.
    /// Override it to consult core state while deciding an extension verb:
    /// validate that a referenced item exists, read its status before accepting
    /// a proposal against it, answer an extension read verb from core tables.
    ///
    /// Deliberately NOT extended to [`derive_event`](Self::derive_event) or
    /// [`replay`](Self::replay): `replay` must stay a pure function of the
    /// logged event — a replay that consulted live core state would rebuild
    /// different tables depending on WHEN the rebuild ran, breaking the
    /// torn-checkpoint recovery contract — and `derive_event` runs immediately
    /// after a successful `apply_with_core` in the same dispatch, so anything
    /// it needs the extension can capture there.
    fn apply_with_core(
        &mut self,
        verb: &str,
        payload: &[u8],
        _core: CoreRead<'_>,
    ) -> Result<KanbanReply, KanbanReply> {
        self.apply(verb, payload)
    }

    /// The committed event a successful mutation logs, as a
    /// [`MutationEvent::Extension`]. `None` for a read (a non-mutating verb) —
    /// the engine only asks for this after a mutating `apply` succeeded.
    fn derive_event(&self, cmd: &ExtCommand, reply: &KanbanReply) -> Option<MutationEvent>;

    /// Rebuild the extension's tables from one logged event `{namespace, kind,
    /// payload}`. Idempotent (a replay may re-see an already-applied event) — an
    /// already-present row MUST be skipped, exactly as the core store's
    /// `has_comment`/`has_relation` replays do, so a torn checkpoint that
    /// re-presents the log tail cannot double a row. Driven directly in 3c-i; the
    /// engine drives it on load in 3c-ii (see [`restore`](Self::restore)).
    fn replay(
        &mut self,
        namespace: &str,
        kind: &str,
        payload: &serde_json::Value,
    ) -> Result<(), String>;

    /// Write the extension's own table(s) to a derived checkpoint under `dir` (the
    /// store directory, alongside the core `_checkpoint.manifest`) and return the
    /// file names it covers. A POST-commit, best-effort step the engine calls after
    /// a successful `backend.commit`: the ext state is ALREADY durable in the event
    /// log, so a failed or torn checkpoint is replayed from the log tail on load —
    /// never a lost write. Write via tmp+rename so a crash leaves the previous
    /// complete checkpoint in place.
    ///
    /// The default writes nothing (an extension with no persisted table, or the
    /// 3c-i write-path-only impls): its events still live in the log and restore
    /// via full replay.
    fn checkpoint(&self, _dir: &Path, _seq: Seq) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }

    /// Load the extension's checkpoint from `dir` and return the committed `seq` it
    /// reflects (`0` if there is none). The engine replays every ext event with
    /// `seq` strictly greater than the returned value, so returning `0` requests a
    /// full-log replay (the correct, safe default for an extension with no
    /// checkpoint, or a pre-3cii store whose ext events predate the checkpoint).
    fn restore(&mut self, _dir: &Path) -> Result<Seq, String> {
        Ok(0)
    }

    /// An O(1) `Arc`-bind of the current ext tables for the aggregate snapshot, or
    /// `None` for an extension that exposes no snapshot. The engine calls this after
    /// each durable commit and stores the result in
    /// [`AggregateSnapshot::ext`](crate::snapshot::AggregateSnapshot); a consumer
    /// downcasts it to its own [`ExtensionSnapshot`] type to read the tables
    /// zero-copy. The default binds nothing.
    fn snapshot(&self) -> Option<Arc<dyn ExtensionSnapshot>> {
        None
    }
}

/// An immutable, point-in-time bind of an extension's tables, carried inside an
/// [`AggregateSnapshot`](crate::snapshot::AggregateSnapshot).
///
/// The engine holds it only as `dyn ExtensionSnapshot`; the extension's consumer
/// downcasts via [`as_any`](ExtensionSnapshot::as_any) to its OWN concrete
/// snapshot type to read the tables. `Send + Sync` (the aggregate snapshot is
/// shared across the actor's read/write threads) and `'static` via the `Any`
/// supertrait, so the public crate never names a consumer table.
pub trait ExtensionSnapshot: std::any::Any + Send + Sync {
    /// Upcast to `dyn Any` so a consumer can `downcast_ref` to its concrete
    /// snapshot type. Implementors return `self`.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// A decoded extension command carried inside
/// [`KanbanCommand::Extension`](crate::engine::KanbanCommand::Extension).
///
/// `is_mutation` is a property of the REGISTRATION (from
/// [`ExtVerbSpec`]), not of the opaque `verb_str` — which is why it travels on
/// the command itself rather than being re-derived.
#[derive(Debug, Clone)]
pub struct ExtCommand {
    /// The owning extension's namespace (e.g. `demo`).
    pub namespace: String,
    /// The full wire verb (e.g. `ext.demo.propose`).
    pub verb: String,
    /// The raw request payload, parsed by the extension's own `apply`.
    pub payload: Vec<u8>,
    /// Whether this verb mutates the extension's tables (drives the commit gate).
    pub is_mutation: bool,
}

/// The classification of a verb by an [`EngineExtension`]: which namespace owns
/// it and whether it mutates.
#[derive(Debug, Clone)]
pub struct ExtVerbSpec {
    /// The owning extension's namespace.
    pub namespace: String,
    /// Whether the verb mutates the extension's tables.
    pub is_mutation: bool,
}
