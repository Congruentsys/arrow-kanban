// SPDX-License-Identifier: MIT
//! Server state — all stores bundled into one struct.

use crate::health::HealthGate;
use arrow_kanban::crud::KanbanStore;
use arrow_kanban::relations::RelationsStore;
use std::path::PathBuf;

/// All server state in one place.
pub struct ServerState {
    pub store: KanbanStore,
    pub relations: RelationsStore,
    pub data_dir: PathBuf,
    /// Write-durability gate. Refuses mutations when the store is not
    /// accepting durable writes, instead of acking writes a restart would lose.
    pub health: HealthGate,
}
