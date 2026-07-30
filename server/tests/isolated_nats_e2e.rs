// SPDX-License-Identifier: MIT
//! CH-6758 (VY-6695 E2 follow-up) — end-to-end against an **isolated** nats-server.
//!
//! Proves the direct-`async_nats` rewrite of the server bootstrap still serves
//! board CRUD over `kanban.cmd.>` **and** publishes each mutation as a v1.0
//! event envelope to the durable `KANBAN_EVENTS` JetStream — the exact contract
//! the prior server loop provided.
//!
//! **Never touches the live fleet bus (4222):** it spawns its own `nats-server
//! -js` on an OS-assigned free port and its own server binary on a temp store.
//! If `nats-server` is not installed, the test SKIPS with a message (the rig is
//! present on M5 + the CI image that provisions it; absent elsewhere it is a
//! no-op rather than a false red).

use std::process::{Child, Command};
use std::time::Duration;

use futures::StreamExt;
use serde_json::{Value, json};

/// Locate the `nats-server` binary (PATH or the Homebrew default). `None` ⇒ skip.
fn nats_server_bin() -> Option<String> {
    for cand in ["nats-server", "/opt/homebrew/bin/nats-server"] {
        let available = if cand.contains('/') {
            std::path::Path::new(cand).exists()
        } else {
            Command::new(cand).arg("--version").output().is_ok()
        };
        if available {
            return Some(cand.to_string());
        }
    }
    None
}

/// An OS-assigned free TCP port (bind :0, read the port, drop the listener).
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local_addr")
        .port()
}

/// Kill-on-drop guard so a panicking assertion never leaves an orphaned process.
struct Killable(Child);
impl Drop for Killable {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Poll `kanban.cmd.stats` until the server answers (it needs a moment to boot,
/// connect, and subscribe). Fails the test if it never comes up.
async fn wait_for_server(client: &async_nats::Client) {
    let body = serde_json::to_vec(&json!({})).unwrap();
    for _ in 0..100 {
        let req = client.request("kanban.cmd.stats", body.clone().into());
        if let Ok(Ok(msg)) = tokio::time::timeout(Duration::from_millis(300), req).await
            && serde_json::from_slice::<Value>(&msg.payload).is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("kanban server did not become ready within ~20s");
}

#[tokio::test]
async fn board_crud_and_push_events_over_isolated_nats() {
    let Some(nats_bin) = nats_server_bin() else {
        eprintln!("[skip] nats-server not installed — isolated NATS rig unavailable");
        return;
    };

    let port = free_port();
    let url = format!("nats://127.0.0.1:{port}");
    let nats_sd = tempfile::tempdir().expect("nats store dir");

    // ── isolated nats-server with JetStream (never the live 4222) ────────────
    let _nats = Killable(
        Command::new(&nats_bin)
            .args([
                "-js",
                "-a",
                "127.0.0.1",
                "-p",
                &port.to_string(),
                "-sd",
                nats_sd.path().to_str().unwrap(),
            ])
            .spawn()
            .expect("spawn nats-server"),
    );

    // wait for nats to accept a client connection
    let client = {
        let mut c = None;
        for _ in 0..50 {
            if let Ok(client) = async_nats::connect(&url).await {
                c = Some(client);
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        c.expect("connect to isolated nats-server")
    };

    // ── the kanban server binary on a temp store, pointed at the isolated bus ─
    let data_dir = tempfile::tempdir().expect("kanban data dir");
    let _server = Killable(
        Command::new(env!("CARGO_BIN_EXE_arrow-kanban-server"))
            .args([
                "--nats-url",
                &url,
                "--data-dir",
                data_dir.path().to_str().unwrap(),
            ])
            .spawn()
            .expect("spawn kanban server"),
    );

    wait_for_server(&client).await;

    // Subscribe to mutation events BEFORE the mutation so none is missed.
    let mut events = client
        .subscribe("kanban.event.>")
        .await
        .expect("subscribe kanban.event.>");

    // ── CREATE (a mutation → reply + durable event) ──────────────────────────
    let create_body =
        serde_json::to_vec(&json!({"title": "ch6758 e2e item", "item_type": "chore"})).unwrap();
    let create = client
        .request("kanban.cmd.create", create_body.into())
        .await
        .expect("create request-reply");
    let created: Value = serde_json::from_slice(&create.payload).expect("create response json");
    assert!(
        created.get("error").is_none(),
        "create must succeed, got: {created}"
    );
    let id = created["id"]
        .as_str()
        .expect("create returns an id")
        .to_string();
    assert!(id.starts_with("CH-"), "a chore gets a CH- id, got {id}");

    // ── PUSH EVENT: the mutation lands on KANBAN_EVENTS as a v1.0 envelope ────
    let event = tokio::time::timeout(Duration::from_secs(5), events.next())
        .await
        .expect("a mutation event within 5s")
        .expect("event stream open");
    let envelope: Value = serde_json::from_slice(&event.payload).expect("event envelope json");
    assert_eq!(
        envelope["version"], "1.0",
        "ShipEvent v1.0 envelope preserved"
    );
    assert_eq!(envelope["source"], "kanban-server", "source stamped");
    assert!(
        envelope["event_type"].as_str().is_some(),
        "event_type present: {envelope}"
    );
    assert_eq!(
        envelope["correlation_id"].as_str().map(str::len),
        Some(8),
        "8-char correlation id"
    );

    // ── CRUD read-back: the created item is queryable through the server ──────
    let show_body = serde_json::to_vec(&json!({"id": id, "format": "json"})).unwrap();
    let show = client
        .request("kanban.cmd.show", show_body.into())
        .await
        .expect("show request-reply");
    let shown: Value = serde_json::from_slice(&show.payload).expect("show response json");
    assert!(
        shown.get("error").is_none(),
        "show must find the created item, got: {shown}"
    );
}
