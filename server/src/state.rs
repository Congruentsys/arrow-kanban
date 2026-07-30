// SPDX-License-Identifier: MIT
//! Server state.
//!
//! The stores, the write-durability gate, and the durable [`StorageBackend`] now
//! live in [`crate::engine::KanbanEngine`] — the one typed semantics.
//! `ServerState` is retained as the default-backend alias for it so the server's
//! own binary and the integration test harness (which hold a `&mut ServerState`)
//! keep working; they now also carry a `backend` field (the commit boundary).

use crate::engine::KanbanEngine;
use crate::storage::ParquetBackend;

/// The server state over the default Parquet backend.
pub type ServerState = KanbanEngine<ParquetBackend>;
