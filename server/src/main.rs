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
use arrow_kanban_server::events::detect_mutation;
use arrow_kanban_server::state::ServerState;
use clap::Parser;
use futures::StreamExt;

/// NATS command subject prefix — the server subscribes to `{PREFIX}.>`.
const CMD_SUBJECT_PREFIX: &str = "kanban.cmd";
/// Mutation events are published to `{EVENT_SUBJECT_PREFIX}.{event_type}`.
const EVENT_SUBJECT_PREFIX: &str = "kanban.event";
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

async fn run(args: ServiceArgs, mut state: ServerState) -> Result<(), Box<dyn std::error::Error>> {
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

    tracing::info!(
        prefix = CMD_SUBJECT_PREFIX,
        url = %args.nats_url,
        stream = EVENT_STREAM_NAME,
        "kanban service ready"
    );

    loop {
        tokio::select! {
            msg = subscriber.next() => {
                let Some(msg) = msg else {
                    tracing::warn!("NATS subscription closed");
                    break;
                };
                let subject = msg.subject.to_string();
                let command = strip_prefix(&subject);

                // Dispatch: the kanban server routes every command through the
                // typed engine (decode → apply → encode).
                let response = state.dispatch(&subject, &msg.payload);

                // Reply to the requester.
                if let Some(reply_to) = msg.reply
                    && let Err(e) = client.publish(reply_to, response.clone().into()).await
                {
                    tracing::error!(error = %e, subject = %subject, "reply failed");
                }

                // Mutation → durable JetStream event (ShipEvent v1.0 envelope).
                if let Some((event_type, event_bytes)) = detect_mutation(command, &response) {
                    let full_subject = format!("{EVENT_SUBJECT_PREFIX}.{event_type}");
                    let data = serde_json::to_vec(&event_envelope(&event_type, &event_bytes))
                        .unwrap_or_else(|_| event_bytes.clone());
                    // Await the publish call (send to the connection); the server
                    // ack future is dropped — fire-and-forget, matching the former
                    // builder's `Ok(ack) => drop(ack)`.
                    if let Err(e) = js.publish(full_subject, data.into()).await {
                        tracing::error!(error = %e, "jetstream event publish failed");
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down");
                break;
            }
        }
    }

    persist_state(&state);
    Ok(())
}

/// Build the durable event envelope — the v1.0 event shape (fixed field names,
/// `version` "1.0") so existing `KANBAN_EVENTS` consumers parse it unchanged.
fn event_envelope(event_type: &str, event_bytes: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "event_type": event_type,
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "source": EVENT_SOURCE,
        "payload": serde_json::from_slice::<serde_json::Value>(event_bytes)
            .unwrap_or(serde_json::Value::Null),
        "correlation_id": uuid::Uuid::new_v4().to_string()[..8].to_string(),
        "version": "1.0",
    })
}

/// Strip the `kanban.cmd.` prefix off a subject → the command suffix
/// (was `NatsServiceBuilder::strip_prefix`). Unmatched subjects pass through.
fn strip_prefix(subject: &str) -> &str {
    subject
        .strip_prefix(CMD_SUBJECT_PREFIX)
        .and_then(|s| s.strip_prefix('.'))
        .unwrap_or(subject)
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

    let store = persist::load_store(data_dir).unwrap_or_else(|e| {
        eprintln!("Failed to load kanban state from {data_dir:?}: {e}");
        std::process::exit(1);
    });
    let relations = persist::load_relations(data_dir).unwrap_or_else(|e| {
        eprintln!("Failed to load relations from {data_dir:?}: {e}");
        std::process::exit(1);
    });
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

    ServerState {
        store,
        relations,
        data_dir: data_dir.to_path_buf(),
        health,
    }
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
    fn strip_prefix_extracts_the_command_suffix() {
        assert_eq!(strip_prefix("kanban.cmd.create"), "create");
        // Multi-segment commands (e.g. pr subcommands) keep their full suffix.
        assert_eq!(strip_prefix("kanban.cmd.pr.merge"), "pr.merge");
        // A subject that is exactly the prefix (no suffix) passes through.
        assert_eq!(strip_prefix("kanban.cmd"), "kanban.cmd");
        // An unrelated subject passes through unchanged (never mis-stripped).
        assert_eq!(strip_prefix("other.subject"), "other.subject");
    }

    #[test]
    fn event_envelope_matches_shipevent_v1_shape() {
        // The envelope MUST stay JSON-compatible with the v1.0 event shape
        // or every KANBAN_EVENTS consumer breaks. Assert every field positively.
        let payload = serde_json::to_vec(&serde_json::json!({"id": "EX-1"})).unwrap();
        let env = event_envelope("item.moved", &payload);

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
    }

    #[test]
    fn event_envelope_null_payload_when_response_is_not_json() {
        // A non-JSON handler response must not panic — payload falls to null.
        let env = event_envelope("item.moved", b"not json");
        assert!(env["payload"].is_null());
        assert_eq!(env["event_type"], "item.moved");
    }
}
