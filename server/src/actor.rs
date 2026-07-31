// SPDX-License-Identifier: MIT
//! The engine actor — one owning task serializes every mutation; reads never queue.
//!
//! [`KanbanHandle`] wraps a [`KanbanEngine`] behind a single owning `tokio` task.
//! That task is the ONLY caller of [`KanbanEngine::dispatch`], so all mutations
//! are serialized onto it with no lock around the engine and no torn concurrent
//! write. It is a *serialization wrapper*: the owning task calls the existing,
//! synchronous `dispatch` UNCHANGED (same health gate, same commit boundary, same
//! reply bytes), then returns those exact bytes over a oneshot — the wire
//! contract is byte-identical to calling the engine directly.
//!
//! # Reads never touch the channel
//!
//! The handle holds a `std::sync::RwLock<Arc<AggregateSnapshot>>` (no new
//! dependency). [`KanbanHandle::snapshot`] just clones the `Arc`. After each
//! SUCCESSFUL mutating dispatch — i.e. once `committed_seq` advances, meaning the
//! write is durably committed — the owning task rebuilds and installs a fresh
//! snapshot BEFORE sending the reply, so:
//!
//! - a reader can never observe a `seq` that is not durably committed
//!   (an in-memory write whose commit FAILED does not advance the seq, so it is
//!   not installed — the snapshot stays at the last durable state);
//! - "reply received" happens-after "snapshot installed", so a caller that just
//!   got an ack immediately sees the snapshot reflecting it.
//!
//! # No deadlock
//!
//! The owning task takes the snapshot write-lock only for the duration of one
//! `Arc` swap and never calls back into the handle; readers take the read-lock
//! only to clone an `Arc`. No task holds the lock across an `.await`, and the
//! owning task never awaits the channel it drains, so the read lock and the
//! owning task cannot deadlock.

use crate::engine::{KanbanEngine, KanbanReply};
use crate::lease::WriterLease;
use crate::snapshot::AggregateSnapshot;
use crate::storage::{Seq, StorageBackend};
use std::sync::{Arc, RwLock};
use tokio::sync::{mpsc, oneshot};

/// One serialized dispatch request handed to the owning task.
struct DispatchRequest {
    subject: String,
    payload: Vec<u8>,
    /// Where the owning task returns the exact wire reply bytes.
    reply: oneshot::Sender<Vec<u8>>,
}

/// The shared snapshot cell: the latest durably-committed [`AggregateSnapshot`].
type SnapshotCell = Arc<RwLock<Arc<AggregateSnapshot>>>;

/// A cheap, `Clone + Send + Sync` handle to the engine-owning task.
///
/// Cloning shares the same command channel and snapshot cell (an
/// `mpsc::Sender` + an `Arc`), so any number of callers can dispatch and read
/// concurrently while the single owning task keeps writes serialized.
#[derive(Clone)]
pub struct KanbanHandle {
    tx: mpsc::Sender<DispatchRequest>,
    snapshot: SnapshotCell,
}

/// The channel depth — mutations queue here when the owning task is busy. Ample
/// for the server's single request loop; `dispatch` awaits backpressure if full.
const COMMAND_CHANNEL_DEPTH: usize = 1024;

/// Spawn the engine-owning task and return a handle to it.
///
/// The returned [`KanbanHandle`] is the only way to reach the moved `engine`.
/// When every handle clone is dropped (the command channel closes), the task
/// runs `on_shutdown(engine)` — the server passes a final persist there — and the
/// returned [`tokio::task::JoinHandle`] completes.
///
/// Generic over the backend so tests can drive the actor over a fault-injecting
/// [`StorageBackend`]; the server spawns it over the default `ParquetBackend`.
pub fn spawn<B, L, F>(
    mut engine: KanbanEngine<B, L>,
    on_shutdown: F,
) -> (KanbanHandle, tokio::task::JoinHandle<()>)
where
    B: StorageBackend + Send + 'static,
    L: WriterLease + Send + 'static,
    F: FnOnce(KanbanEngine<B, L>) + Send + 'static,
{
    // Seed the snapshot from the loaded state at its committed high-water,
    // including the installed extension's table bind (None for the open engine).
    let initial = Arc::new(AggregateSnapshot::new(
        &engine.store,
        &engine.relations,
        engine.committed_seq(),
        engine.extension.as_ref().and_then(|x| x.snapshot()),
    ));
    let cell: SnapshotCell = Arc::new(RwLock::new(initial));
    let task_cell = cell.clone();

    let (tx, mut rx) = mpsc::channel::<DispatchRequest>(COMMAND_CHANNEL_DEPTH);

    let join = tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            // The ONLY caller of the synchronous engine dispatch — same health
            // gate, same commit boundary, same reply bytes.
            let seq_before = engine.committed_seq();
            let bytes = engine.dispatch(&req.subject, &req.payload);
            let seq_after = engine.committed_seq();

            // Install a fresh snapshot ONLY when a durable commit advanced the seq
            // (a read, a refused mutation, or a failed commit leaves it unchanged),
            // and do it BEFORE replying so "ack received" implies "snapshot ready".
            if seq_after != seq_before {
                let snap = Arc::new(AggregateSnapshot::new(
                    &engine.store,
                    &engine.relations,
                    seq_after,
                    engine.extension.as_ref().and_then(|x| x.snapshot()),
                ));
                *task_cell
                    .write()
                    .expect("snapshot cell poisoned (a snapshot install never panics)") = snap;
            }

            // The requester may have gone away (dropped rx); that is not our error.
            let _ = req.reply.send(bytes);
        }
        // Channel closed — every handle dropped. Hand the engine to the shutdown
        // hook (the server persists; the commit boundary already made every
        // mutation durable, so this is belt-and-suspenders).
        on_shutdown(engine);
    });

    (KanbanHandle { tx, snapshot: cell }, join)
}

impl KanbanHandle {
    /// Send `subject` + `payload` to the owning task and await its exact wire
    /// reply bytes — byte-identical to `KanbanEngine::dispatch` on the same input.
    pub async fn dispatch(&self, subject: &str, payload: &[u8]) -> Vec<u8> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let req = DispatchRequest {
            subject: subject.to_string(),
            payload: payload.to_vec(),
            reply: reply_tx,
        };
        if self.tx.send(req).await.is_err() {
            // The owning task is gone — surface a structured error rather than
            // panicking a request loop (does not happen in normal operation).
            return KanbanReply::error("engine task unavailable", "INTERNAL").into_bytes();
        }
        reply_rx.await.unwrap_or_else(|_| {
            KanbanReply::error("engine task dropped the reply", "INTERNAL").into_bytes()
        })
    }

    /// The latest durably-committed snapshot — an `Arc` clone, no channel round
    /// trip, no lock held past the clone.
    pub fn snapshot(&self) -> Arc<AggregateSnapshot> {
        self.snapshot
            .read()
            .expect("snapshot cell poisoned (a snapshot read never panics)")
            .clone()
    }

    /// The durably-committed high-water reflected by the latest snapshot.
    pub fn committed_seq(&self) -> Seq {
        self.snapshot().seq()
    }
}
