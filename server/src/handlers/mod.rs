// SPDX-License-Identifier: MIT
//! Request-reply handlers for all kanban commands.
//!
//! Each `kanban.cmd.{command}` subject maps to a handler function that
//! deserializes the JSON payload, calls the corresponding `arrow-kanban`
//! library function, and returns a JSON response.
//!
//! Modules:
//! - [`core`] — CRUD, analytics, HDD creation
//! - [`relations`] — Dependency graph
//! - [`pr`] — PR / Proposal lifecycle (feature: `pr`)
//! - [`source`] — Git bundle transport (feature: `git`)
//! - [`research`] — HDD experiment run tracking (feature: `research`)

pub mod core;
pub mod relations;
#[cfg(feature = "research")]
pub mod research;
#[cfg(feature = "git")]
pub mod source;

use crate::engine::KanbanEngine;
use serde::{Deserialize, Serialize};

/// Unified error response sent back to NATS clients.
///
/// `Deserialize` (with an owned `code`) lets the engine round-trip an
/// already-serialized error — a handler's `error_response(...)` bytes — back
/// into a typed [`crate::engine::KanbanReply::Error`] without changing a single
/// byte on the wire (the round-trip is exact for any value `error_response`
/// produced).
#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
    pub code: String,
}

/// Default board states.
pub(crate) const DEFAULT_STATES: &[&str] = &["backlog", "in_progress", "review", "done"];

/// Dispatch a NATS message through the typed engine.
///
/// A thin wire-entry shim over [`KanbanEngine::dispatch`] — the one typed
/// semantics. The former string-dispatch (command match + admission gate +
/// persist-or-degrade) now lives in [`crate::engine`]; this only forwards.
/// Kept as a free function so the existing integration-test harness (which
/// calls `dispatch(subject, payload, &mut engine)`) is unchanged.
pub fn dispatch(subject: &str, payload: &[u8], engine: &mut KanbanEngine) -> Vec<u8> {
    engine.dispatch(subject, payload)
}

pub(crate) fn error_response(msg: &str, code: &'static str) -> Vec<u8> {
    serde_json::to_vec(&ErrorResponse {
        error: msg.to_string(),
        code: code.to_string(),
    })
    .unwrap_or_else(|_| br#"{"error":"serialization failed","code":"INTERNAL"}"#.to_vec())
}

pub(crate) fn parse_payload<T: for<'de> serde::Deserialize<'de>>(
    payload: &[u8],
) -> Result<T, Vec<u8>> {
    serde_json::from_slice(payload)
        .map_err(|e| error_response(&format!("Invalid JSON: {e}"), "INVALID_PAYLOAD"))
}

pub(crate) fn states_as_strings() -> Vec<String> {
    DEFAULT_STATES.iter().map(|s| s.to_string()).collect()
}
