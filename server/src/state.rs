// SPDX-License-Identifier: MIT
//! Server state.
//!
//! The stores and the write-durability gate now live in
//! [`crate::engine::KanbanEngine`] — the one typed semantics. `ServerState` is
//! retained as an alias for it so the server's own binary and the integration
//! test harness (which construct `ServerState { store, relations, data_dir,
//! health }` and hold a `&mut ServerState`) keep working unchanged.

pub use crate::engine::KanbanEngine as ServerState;
