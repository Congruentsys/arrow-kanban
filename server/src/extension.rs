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
//!   (in-memory, for 3c-i) tables and return a [`KanbanReply`].
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
//! # Not here (3c-ii)
//!
//! The persistence half — checkpoint / restore of extension tables and their
//! binding into the aggregate snapshot, plus load-time event replay — is a
//! SEPARATE, additive change. In 3c-i an extension's events are durable in the
//! commit log but are not yet restored on load; 3c-ii completes that.

use crate::engine::KanbanReply;
use crate::events::MutationEvent;

/// A namespaced command extension: one verb family with its own tables, routed
/// through the engine's commit boundary.
///
/// Methods take `&self`/`&mut self` on the extension's OWN state — the engine
/// holds the extension and never reaches inside it. The trait is the whole seam;
/// it carries no core vocabulary, so an extension is free to be anything from a
/// review workflow to a scratch demo (see the write-path acceptance battery).
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

    /// The committed event a successful mutation logs, as a
    /// [`MutationEvent::Extension`]. `None` for a read (a non-mutating verb) —
    /// the engine only asks for this after a mutating `apply` succeeded.
    fn derive_event(&self, cmd: &ExtCommand, reply: &KanbanReply) -> Option<MutationEvent>;

    /// Rebuild the extension's tables from one logged event `{namespace, kind,
    /// payload}`. Idempotent (a replay may re-see an already-applied event). In
    /// 3c-i this is exercised directly; the engine's load-time replay
    /// orchestration is 3c-ii.
    fn replay(
        &mut self,
        namespace: &str,
        kind: &str,
        payload: &serde_json::Value,
    ) -> Result<(), String>;
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
