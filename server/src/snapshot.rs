// SPDX-License-Identifier: MIT
//! A point-in-time, immutable, O(1)-cloneable view of every engine-owned table.
//!
//! [`AggregateSnapshot`] binds ALL the engine's persisted tables — items, runs,
//! item-comments (the [`KanbanStore`]) and the typed relations
//! ([`RelationsStore`]) — to ONE durably-committed [`Seq`] plus the on-disk
//! [`schema_version`](crate::storage::SCHEMA_VERSION). It is the read side of the
//! actor model in [`crate::actor`]: the engine-owning task installs a fresh
//! snapshot after each durable commit, and readers clone the `Arc` instead of
//! queueing on the write channel.
//!
//! # Why cloning is cheap and coherent
//!
//! Each held table is a set of Arrow `RecordBatch`es whose columns are `Arc`'d,
//! so [`AggregateSnapshot::new`] and `#[derive(Clone)]` are O(#batches)
//! Arc-pointer copies — no column data is copied ("zero-copy clone"). Because a
//! snapshot is an immutable value taken between commits, it can never observe a
//! torn write: the tables it holds are exactly those a single commit made
//! durable, all at the same `seq`.

use crate::storage::{SCHEMA_VERSION, Seq};
use arrow_kanban::crud::KanbanStore;
use arrow_kanban::relations::RelationsStore;

/// An immutable, coherent, O(1)-cloneable view of the whole board at one seq.
///
/// `seq` is the durably-committed high-water the snapshot reflects: it holds
/// exactly the state produced by the commits with `seq' <= seq`, and nothing
/// past it (the actor installs a new snapshot only after a commit makes the next
/// seq durable — never before).
#[derive(Clone)]
pub struct AggregateSnapshot {
    /// items + runs + item-comments, as of `seq`.
    pub store: KanbanStore,
    /// typed relations, as of `seq`.
    pub relations: RelationsStore,
    /// The durably-committed high-water this snapshot reflects (commits <= seq).
    pub seq: Seq,
    /// The on-disk table-contract version bound with the tables.
    pub schema_version: u32,
}

impl AggregateSnapshot {
    /// Bind the current `store` + `relations` to `seq` — an O(#batches)
    /// Arc-pointer copy of every table, no column data copied.
    pub fn new(store: &KanbanStore, relations: &RelationsStore, seq: Seq) -> Self {
        AggregateSnapshot {
            store: store.clone(),
            relations: relations.clone(),
            seq,
            schema_version: SCHEMA_VERSION,
        }
    }

    /// An empty snapshot at seq 0 (a fresh, pre-commit board).
    pub fn empty() -> Self {
        AggregateSnapshot {
            store: KanbanStore::new(),
            relations: RelationsStore::new(),
            seq: 0,
            schema_version: SCHEMA_VERSION,
        }
    }

    /// The durably-committed high-water this snapshot reflects.
    pub fn seq(&self) -> Seq {
        self.seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_kanban::crud::CreateItemInput;
    use arrow_kanban::item_type::ItemType;

    fn seed(store: &mut KanbanStore, title: &str) -> String {
        store
            .create_item(&CreateItemInput {
                title: title.to_string(),
                item_type: ItemType::Chore,
                priority: None,
                assignee: None,
                tags: Vec::new(),
                related: Vec::new(),
                depends_on: Vec::new(),
                body: None,
            })
            .expect("create")
    }

    /// A snapshot binds the tables to its seq and carries the schema version.
    #[test]
    fn new_binds_tables_and_seq() {
        let mut store = KanbanStore::new();
        let id = seed(&mut store, "one");
        let rels = RelationsStore::new();

        let snap = AggregateSnapshot::new(&store, &rels, 7);
        assert_eq!(snap.seq(), 7);
        assert_eq!(snap.schema_version, SCHEMA_VERSION);
        assert!(snap.store.get_item(&id).is_ok(), "snapshot holds the item");
    }

    /// A snapshot is an IMMUTABLE value: mutating the source store after the
    /// snapshot is taken does not change what the snapshot holds (the clone is a
    /// point-in-time copy, not a live view).
    #[test]
    fn snapshot_is_an_immutable_point_in_time_value() {
        let mut store = KanbanStore::new();
        let first = seed(&mut store, "first");
        let rels = RelationsStore::new();

        let snap = AggregateSnapshot::new(&store, &rels, 1);

        // Mutate the source AFTER taking the snapshot.
        let second = seed(&mut store, "second");

        assert!(
            snap.store.get_item(&first).is_ok(),
            "held item still present"
        );
        assert!(
            snap.store.get_item(&second).is_err(),
            "a later insert does not appear in a held snapshot"
        );
        // The live store DID see the mutation — proving the snapshot diverged.
        assert!(store.get_item(&second).is_ok());
    }

    /// `empty` is a fresh board at seq 0.
    #[test]
    fn empty_is_seq_zero_and_blank() {
        let snap = AggregateSnapshot::empty();
        assert_eq!(snap.seq(), 0);
        assert_eq!(snap.store.active_item_count(), 0);
        assert_eq!(snap.relations.active_count(), 0);
    }
}
