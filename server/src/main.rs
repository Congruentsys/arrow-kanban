// SPDX-License-Identifier: MIT
//! arrow-kanban-server — NATS server for the Arrow-native kanban engine.
//!
//! The request-reply + JetStream loop is wired directly on `async_nats`, so the
//! kanban distribution ships a standalone server with zero closed crates:
//! subscribe `kanban.cmd.>`, dispatch via [`handlers::dispatch`], reply on
//! `msg.reply`, and publish each mutation as a v1.0 event envelope to the
//! durable `KANBAN_EVENTS` JetStream — so every existing consumer of that
//! stream parses events unchanged. Ctrl-C triggers a graceful shutdown that
//! persists state.

use std::path::PathBuf;
use std::time::Duration;

use arrow_kanban::backup::{self, BackupConfig};
use arrow_kanban::persist;
use arrow_kanban_server::actor;
use arrow_kanban_server::state::ServerState;
use arrow_kanban_server::storage::{ParquetBackend, Seq, StorageBackend};
use clap::Parser;
use futures::StreamExt;

/// NATS command subject prefix — the server subscribes to `{PREFIX}.>`.
const CMD_SUBJECT_PREFIX: &str = "kanban.cmd";
/// Mutation events are published to `{EVENT_SUBJECT_PREFIX}.{event_type}`.
const EVENT_SUBJECT_PREFIX: &str = "kanban.event";
/// Extension events (E3 3c-i) are published to a SEPARATE subject tree,
/// `{EXT_EVENT_SUBJECT_PREFIX}.{namespace}.{kind}` — deliberately NOT under
/// `kanban.event.>`, so the frozen `KANBAN_EVENTS` stream and its v1.0 consumers
/// never receive an extension event.
const EXT_EVENT_SUBJECT_PREFIX: &str = "kanban.ext.event";
/// Durable JetStream stream that captures mutation events.
const EVENT_STREAM_NAME: &str = "KANBAN_EVENTS";
/// Subject filter the stream captures (matches every `kanban.event.*`).
const EVENT_SUBJECT_FILTER: &str = "kanban.event.>";
/// `source` field stamped on every emitted event envelope.
const EVENT_SOURCE: &str = "kanban-server";
/// Stream retention: max age (was `StreamConfig::new` default, 24h).
const STREAM_MAX_AGE_SECS: u64 = 86_400;
/// Stream retention: max messages (was `StreamConfig::new` default).
const STREAM_MAX_MSGS: i64 = 100_000;

/// Standard CLI arguments — stable flags and defaults across launch commands.
#[derive(Parser, Debug, Clone)]
struct ServiceArgs {
    /// Working directory for persistent data.
    #[arg(long, default_value = ".")]
    data_dir: PathBuf,

    /// NATS server URL.
    #[arg(long, default_value = "nats://localhost:4222")]
    nats_url: String,
}

fn main() {
    let args = ServiceArgs::parse();
    let state = load_state(&args.data_dir);

    // Run startup backup check in a background thread so it doesn't delay server start.
    let backup_root = args.data_dir.clone();
    std::thread::spawn(move || {
        let config = BackupConfig::default();
        match backup::is_backup_due(&config) {
            Ok(true) => {
                eprintln!(
                    "[backup] Snapshot due, creating backup to {:?} ...",
                    config.destination
                );
                match backup::create_snapshot(&config, &backup_root) {
                    Ok(path) => {
                        eprintln!(
                            "[backup] Snapshot created: {}",
                            path.file_name().unwrap_or_default().to_string_lossy()
                        );
                    }
                    Err(e) => {
                        eprintln!("[backup] Warning: failed to create snapshot: {e}");
                    }
                }
            }
            Ok(false) => {
                eprintln!("[backup] No backup due.");
            }
            Err(e) => {
                eprintln!("[backup] Warning: could not determine backup status: {e}");
            }
        }
    });

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    if let Err(e) = rt.block_on(run(args, state)) {
        eprintln!("Server error: {e}");
        std::process::exit(1);
    }
}

async fn run(args: ServiceArgs, state: ServerState) -> Result<(), Box<dyn std::error::Error>> {
    // Plaintext fleet bus (byte-for-byte the former non-TLS connect path).
    let client = async_nats::connect(&args.nats_url).await?;
    let js = async_nats::jetstream::new(client.clone());

    // Ensure the durable event stream exists — idempotent (a no-op if the live
    // stream is already there, exactly like the former `ensure_stream`).
    js.get_or_create_stream(async_nats::jetstream::stream::Config {
        name: EVENT_STREAM_NAME.to_string(),
        subjects: vec![EVENT_SUBJECT_FILTER.to_string()],
        max_age: Duration::from_secs(STREAM_MAX_AGE_SECS),
        max_messages: STREAM_MAX_MSGS,
        storage: async_nats::jetstream::stream::StorageType::File,
        ..Default::default()
    })
    .await?;

    let subscribe_subject = format!("{CMD_SUBJECT_PREFIX}.>");
    let mut subscriber = client.subscribe(subscribe_subject).await?;

    let data_dir = args.data_dir.clone();

    // The transactional-outbox watermark: the last seq durably PUBLISHED to
    // JetStream. Committed events are published OUTSIDE the mutation critical
    // section, and the watermark advances only after each publish ACKs. A crash
    // between commit and publish leaves `committed_seq` ahead of this watermark,
    // so the tail is re-derived (`events_since`) and re-published on restart;
    // JetStream dedups the re-publish by the stable `Nats-Msg-Id`.
    let mut published: Seq = read_published_watermark(&data_dir);

    // Spawn the ONE engine-owning task. The request loop reaches the engine only
    // through the handle (which serializes mutations); reads and the outbox drain
    // stay off the write path. On shutdown the task persists — belt-and-suspenders
    // over the commit boundary, which already made every mutation durable.
    let (handle, engine_task) = actor::spawn(state, |engine| persist_state(&engine));

    tracing::info!(
        prefix = CMD_SUBJECT_PREFIX,
        url = %args.nats_url,
        stream = EVENT_STREAM_NAME,
        committed = handle.committed_seq(),
        published,
        "kanban service ready"
    );

    // Startup drain: re-publish anything committed-but-unpublished from a prior
    // crash before serving new traffic (the crash-after-commit-before-publish case).
    drain_outbox(&js, &data_dir, &mut published).await;

    loop {
        tokio::select! {
            msg = subscriber.next() => {
                let Some(msg) = msg else {
                    tracing::warn!("NATS subscription closed");
                    break;
                };
                let subject = msg.subject.to_string();

                // Dispatch through the engine-owning task: same decode → apply →
                // commit → encode, serialized, byte-identical reply.
                let response = handle.dispatch(&subject, &msg.payload).await;

                // Reply to the requester.
                if let Some(reply_to) = msg.reply
                    && let Err(e) = client.publish(reply_to, response.into()).await
                {
                    tracing::error!(error = %e, subject = %subject, "reply failed");
                }

                // Outbox relay: publish committed-but-unpublished events by
                // path-reading the durable log directly (never through the owning
                // task), advancing the watermark per ACK.
                drain_outbox(&js, &data_dir, &mut published).await;
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down");
                break;
            }
        }
    }

    // Close the command channel (drop the sole handle) so the owning task runs its
    // shutdown persist, then wait for it to finish before exiting.
    drop(handle);
    let _ = engine_task.await;
    Ok(())
}

/// The file recording the outbox watermark (last seq durably published).
const PUBLISHED_WATERMARK_FILE: &str = "_published.seq";

/// The stable, seq-derived event id used as the JetStream `Nats-Msg-Id` — the
/// dedup key that makes a crash-recovery re-publish idempotent.
fn stable_event_id(seq: Seq) -> String {
    format!("kanban-event-{seq}")
}

/// Read the persisted outbox watermark (0 if absent/unreadable).
fn read_published_watermark(data_dir: &std::path::Path) -> Seq {
    let Ok(dir) = persist::data_dir(data_dir) else {
        return 0;
    };
    std::fs::read_to_string(dir.join(PUBLISHED_WATERMARK_FILE))
        .ok()
        .and_then(|s| s.trim().parse::<Seq>().ok())
        .unwrap_or(0)
}

/// Persist the outbox watermark (best-effort atomic write; a lost update only
/// causes a bounded, dedup-safe re-publish on restart).
fn write_published_watermark(data_dir: &std::path::Path, seq: Seq) {
    let Ok(dir) = persist::data_dir(data_dir) else {
        return;
    };
    let path = dir.join(PUBLISHED_WATERMARK_FILE);
    let tmp = dir.join(format!("{PUBLISHED_WATERMARK_FILE}.tmp"));
    if std::fs::write(&tmp, seq.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Publish every committed-but-unpublished event to JetStream, advancing the
/// watermark only after each server ACK. Stops on the first publish/ack error so
/// the unacked tail is retried on the next drain (never dropped).
///
/// The committed tail is a PURE PATH-READ of the durable append-only log
/// ([`ParquetBackend::read_events_since`]), so the drain never contends with the
/// engine-owning task — it reads the same durable bytes the commit fsynced,
/// without a channel round trip or a lock on the engine.
async fn drain_outbox(
    js: &async_nats::jetstream::Context,
    data_dir: &std::path::Path,
    published: &mut Seq,
) {
    let events = match ParquetBackend::read_events_since(data_dir, *published) {
        Ok(events) => events,
        Err(e) => {
            tracing::error!(error = %e, "outbox: reading committed events failed");
            return;
        }
    };
    for committed in events {
        // Core events stay on `kanban.event.<suffix>` (byte-identical to before);
        // extension events go on the SEPARATE `kanban.ext.event.<ns>.<kind>` tree,
        // so the frozen `kanban.event.>` stream never captures them.
        let full_subject = committed
            .event
            .published_subject(EVENT_SUBJECT_PREFIX, EXT_EVENT_SUBJECT_PREFIX);
        let payload = committed.event.outbox_payload();
        // The envelope's `event_type` is the event's logical subject — the frozen
        // suffix for a core event (byte-identical to the former `suffix`), and
        // `ext.<ns>.<kind>` for an extension event.
        let event_type = committed.event.event_subject();
        let envelope = event_envelope_versioned(
            &event_type,
            &payload,
            committed.event.contract_version(),
            committed.seq,
        );
        let Ok(data) = serde_json::to_vec(&envelope) else {
            tracing::error!(seq = committed.seq, "outbox: envelope encode failed");
            break;
        };

        // A STABLE, seq-derived Nats-Msg-Id: JetStream deduplicates a re-publish
        // of the same committed event (idempotent crash recovery).
        let mut headers = async_nats::HeaderMap::new();
        headers.insert("Nats-Msg-Id", stable_event_id(committed.seq).as_str());

        match js
            .publish_with_headers(full_subject, headers, data.into())
            .await
        {
            // Await the SERVER ack (do not drop it) — the watermark advances only
            // once the event is durably in the stream.
            Ok(ack_fut) => match ack_fut.await {
                Ok(_ack) => {
                    *published = committed.seq;
                    write_published_watermark(data_dir, *published);
                }
                Err(e) => {
                    tracing::error!(error = %e, seq = committed.seq, "outbox: publish ack failed");
                    break;
                }
            },
            Err(e) => {
                tracing::error!(error = %e, seq = committed.seq, "outbox: publish failed");
                break;
            }
        }
    }
}

/// Build the durable event envelope. The three legacy events (`created`/`moved`/
/// `deleted`) ship the FROZEN v1.0 shape (fixed field names, `version` "1.0", a
/// random `correlation_id`) so existing `KANBAN_EVENTS` consumers parse them
/// byte-for-byte unchanged. The events the outbox added ship `version` "1.1"
/// with one ADDITIVE field — a stable, seq-derived `event_id` — so a v1.0
/// consumer still parses them unchanged. The `version` field IS the contract
/// marker; the dedup key travels in the `Nats-Msg-Id` transport header, never in
/// the v1.0 body (which must not change).
fn event_envelope_versioned(
    event_type: &str,
    payload: &serde_json::Value,
    version: &str,
    seq: Seq,
) -> serde_json::Value {
    let mut env = serde_json::json!({
        "event_type": event_type,
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "source": EVENT_SOURCE,
        "payload": payload,
        "correlation_id": uuid::Uuid::new_v4().to_string()[..8].to_string(),
        "version": version,
    });
    if version != "1.0"
        && let serde_json::Value::Object(map) = &mut env
    {
        map.insert(
            "event_id".to_string(),
            serde_json::Value::String(stable_event_id(seq)),
        );
    }
    env
}

fn load_state(data_dir: &std::path::Path) -> ServerState {
    // An interrupted save leaves a WAL and/or orphaned
    // `*.parquet.tmp`. Quarantine that evidence BEFORE loading — the next save
    // would otherwise overwrite it, and during the incident the interrupted write
    // was the only copy of two months of state. Never delete it.
    match arrow_kanban::persistence::PersistenceEngine::recover_wal(data_dir) {
        Ok(rec) if rec.recovered => {
            eprintln!(
                "kanban: recovered from an INTERRUPTED SAVE — {} file(s) quarantined to {}",
                rec.quarantined.len(),
                rec.quarantine_dir
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<none>".into())
            );
            if rec.is_suspect() {
                eprintln!(
                    "kanban: 🔴 SUSPECT LOAD — the interrupted save is NEWER than the state about \
                     to be loaded ({}). Serving this is probably a ROLLBACK. Inspect the \
                     quarantine before trusting the board.",
                    rec.suspect
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        Ok(_) => {}
        Err(e) => eprintln!("kanban: WAL recovery check failed: {e}"),
    }

    // Load through the storage backend: the state checkpoint (byte-identical to
    // the former `load_store`/`load_relations` for a pre-backend store — no event
    // log means no replay) brought current with any committed-but-uncheckpointed
    // log tail. A torn checkpoint is recovered here, not silently served stale.
    let mut backend = ParquetBackend::open(data_dir).unwrap_or_else(|e| {
        eprintln!("Failed to open storage backend at {data_dir:?}: {e}");
        std::process::exit(1);
    });
    let (loaded, committed_seq) = backend.load().unwrap_or_else(|e| {
        eprintln!("Failed to load kanban state from {data_dir:?}: {e}");
        std::process::exit(1);
    });
    if committed_seq > 0 {
        eprintln!("kanban: loaded through commit seq {committed_seq}.");
    }

    // probe the store before serving. Coming up "ready" on a store
    // that cannot take a write is how the incident stayed invisible for days.
    let mut health = arrow_kanban_server::health::HealthGate::new();
    health.probe_now(&arrow_kanban_server::health::store_dir(data_dir));
    if health.is_degraded() {
        eprintln!(
            "kanban: 🔴 STARTING DEGRADED — {}. Serving READS ONLY; mutations are refused until \
             the store accepts writes.",
            health.reason().unwrap_or("store is not writable")
        );
    }

    ServerState::with_backend(
        loaded.store,
        loaded.relations,
        data_dir.to_path_buf(),
        health,
        backend,
    )
}

fn persist_state(state: &ServerState) {
    if let Err(e) = persist::save_store(&state.data_dir, &state.store) {
        eprintln!("Warning: failed to save store: {e}");
    }
    if let Err(e) = persist::save_relations(&state.data_dir, &state.relations) {
        eprintln!("Warning: failed to save relations: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_envelope_matches_shipevent_v1_shape() {
        // The v1.0 envelope MUST stay byte-compatible or every KANBAN_EVENTS
        // consumer breaks. Assert every field positively — and that the additive
        // v1.1 `event_id` is ABSENT (the v1.0 body is frozen; the dedup key lives
        // only in the transport header for legacy events).
        let payload = serde_json::json!({"id": "EX-1"});
        let env = event_envelope_versioned("item.moved", &payload, "1.0", 5);

        assert_eq!(env["event_type"], "item.moved");
        assert_eq!(env["source"], EVENT_SOURCE);
        assert_eq!(env["version"], "1.0");
        assert_eq!(
            env["payload"]["id"], "EX-1",
            "the mutation payload is nested under `payload`"
        );
        assert_eq!(
            env["correlation_id"].as_str().map(|s| s.len()),
            Some(8),
            "correlation_id is an 8-char UUID prefix"
        );
        assert!(
            env["timestamp"].as_str().is_some_and(|t| t.contains('T')),
            "timestamp is RFC-3339"
        );
        assert!(
            env.get("event_id").is_none(),
            "the frozen v1.0 envelope must NOT carry the additive event_id field"
        );
    }

    #[test]
    fn event_envelope_v11_is_additive_over_v1_0() {
        // A v1.1 envelope adds `version:1.1` + a stable seq-derived `event_id`,
        // and nothing else — a v1.0 consumer parses it unchanged.
        let payload = serde_json::json!({"phase_tag": "p"});
        let env = event_envelope_versioned("ratified", &payload, "1.1", 7);

        assert_eq!(env["version"], "1.1");
        assert_eq!(env["event_id"], stable_event_id(7));
        assert_eq!(env["event_id"], "kanban-event-7");
        // Same v1.0 fields still present (additive, not replacing).
        assert_eq!(env["event_type"], "ratified");
        assert_eq!(env["source"], EVENT_SOURCE);
        assert_eq!(env["payload"]["phase_tag"], "p");
        assert_eq!(env["correlation_id"].as_str().map(|s| s.len()), Some(8));
    }

    #[test]
    fn event_envelope_tolerates_a_null_payload() {
        // A null payload must not panic — it is carried through verbatim.
        let env = event_envelope_versioned("item.moved", &serde_json::Value::Null, "1.0", 1);
        assert!(env["payload"].is_null());
        assert_eq!(env["event_type"], "item.moved");
    }
}
