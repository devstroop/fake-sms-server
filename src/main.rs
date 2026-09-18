//! Fake SMS gateway for 1KM dev: Twilio-compatible receive endpoint +
//! custom-bridge endpoint, with a web inbox. Ephemeral by design —
//! bounded in-memory store, nothing persists restarts.
//!
//! Endpoints:
//! ```text
//! POST /2010-04-01/Accounts/:sid/Messages.json  Twilio shape (form To/From/Body)
//! POST /send                                     custom bridge shape (JSON to/body/sender?)
//! GET  /api/messages                            inbox, newest first
//! GET  /healthz                                 liveness
//! GET  /                                        web inbox UI
//! ```
//!
//! Auth is accepted but never verified (any Basic credentials pass) —
//! this is a dev fake, never exposed publicly.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use axum::{
    Json, Router,
    extract::{Form, Path, State},
    http::StatusCode,
    response::Html,
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Inbox cap: newest wins, oldest evicted (ephemeral dev store).
const MAX_KEPT: usize = 1000;

#[derive(Debug, Clone, Serialize)]
struct Message {
    id: Uuid,
    to: String,
    from: String,
    body: String,
    provider: &'static str,
    sid: String,
    received_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct TwilioForm {
    #[serde(rename = "To")]
    to: String,
    #[serde(rename = "From")]
    from: String,
    #[serde(rename = "Body")]
    body: String,
}

#[derive(Debug, Deserialize)]
struct CustomBody {
    to: String,
    body: String,
    #[serde(default)]
    sender: Option<String>,
}

#[derive(Clone)]
struct AppState {
    inbox: Arc<RwLock<Vec<Message>>>,
}

/// Twilio-style SID: `SM` + 32 lowercase hex.
fn new_sid() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    let hex: String = (0..32)
        .map(|_| format!("{:x}", rng.random_range(0..16)))
        .collect();
    format!("SM{hex}")
}

fn store(state: &AppState, to: String, from: String, body: String, provider: &'static str) -> Message {
    let msg = Message {
        id: Uuid::new_v4(),
        to,
        from,
        body,
        provider,
        sid: new_sid(),
        received_at: Utc::now(),
    };
    state
        .inbox
        .write()
        .map(|mut g| {
            g.push(msg.clone());
            let excess = g.len().saturating_sub(MAX_KEPT);
            if excess > 0 {
                g.drain(..excess);
            }
        })
        .unwrap_or_else(|_| tracing::error!("inbox poisoned"));
    tracing::info!(to = %msg.to, sid = %msg.sid, provider, "fake sms received");
    msg
}

async fn twilio_receive(
    State(state): State<AppState>,
    Path(_account_sid): Path<String>,
    Form(form): Form<TwilioForm>,
) -> (StatusCode, Json<serde_json::Value>) {
    let msg = store(&state, form.to, form.from, form.body, "twilio");
    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "sid": msg.sid,
            "status": "queued",
            "to": msg.to,
            "from": msg.from,
        })),
    )
}

async fn custom_receive(
    State(state): State<AppState>,
    Json(input): Json<CustomBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if input.to.trim().is_empty() || input.body.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "to and body are required" })),
        );
    }
    let from = input.sender.unwrap_or_else(|| "1KM".to_string());
    let msg = store(&state, input.to, from, input.body, "custom");
    (
        StatusCode::CREATED,
        Json(serde_json::json!({ "sid": msg.sid, "status": "queued" })),
    )
}

async fn messages(State(state): State<AppState>) -> Json<serde_json::Value> {
    let mut items = state
        .inbox
        .read()
        .map(|g| g.clone())
        .unwrap_or_default();
    items.sort_by_key(|m| std::cmp::Reverse(m.received_at));
    Json(serde_json::json!({ "data": items, "total": items.len() }))
}

async fn healthz() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>fake-sms-server inbox</title>
<style>
body { font-family: system-ui, sans-serif; max-width: 720px; margin: 2rem auto; padding: 0 1rem; }
table { border-collapse: collapse; width: 100%; }
th, td { border: 1px solid #ccc; padding: .4rem .6rem; text-align: left; font-size: .9rem; }
tr.detail td { background: #f6f6f6; white-space: pre-wrap; }
.badge { font-size: .75rem; padding: .1rem .4rem; border-radius: 4px; background: #eee; }
</style>
</head>
<body>
<h1>fake-sms-server inbox</h1>
<p><span id="count">0</span> messages · auto-refreshes every 3s · ephemeral (restart wipes)</p>
<table>
<thead><tr><th>To</th><th>From</th><th>Body</th><th>Via</th><th>At</th></tr></thead>
<tbody id="rows"></tbody>
</table>
<script>
async function load() {
  const res = await fetch('/api/messages');
  const j = await res.json();
  document.getElementById('count').textContent = j.total;
  const tb = document.getElementById('rows');
  tb.innerHTML = '';
  for (const m of j.data) {
    const tr = document.createElement('tr');
    tr.innerHTML = `<td>${m.to}</td><td>${m.from}</td><td></td>` +
      `<td><span class="badge">${m.provider}</span></td><td>${m.received_at}</td>`;
    tr.children[2].textContent = m.body;
    tr.style.cursor = 'pointer';
    tr.onclick = () => {
      const d = document.createElement('tr');
      d.className = 'detail';
      d.innerHTML = `<td colspan="5">sid: ${m.sid}<br>id: ${m.id}</td>`;
      tr.after(d);
    };
    tb.appendChild(tr);
  }
}
load();
setInterval(load, 3000);
</script>
</body>
</html>"#;

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

fn router(state: AppState) -> Router {
    Router::new()
        .route(
            "/2010-04-01/Accounts/{sid}/Messages.json",
            post(twilio_receive),
        )
        .route("/send", post(custom_receive))
        .route("/api/messages", get(messages))
        .route("/healthz", get(healthz))
        .route("/", get(index))
        .with_state(state)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();
    let host = std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8080);
    let state = AppState {
        inbox: Arc::new(RwLock::new(Vec::new())),
    };
    let listener = tokio::net::TcpListener::bind(format!("{host}:{port}"))
        .await
        .expect("bind fake-sms-server");
    tracing::info!("fake-sms-server on {host}:{port}");
    axum::serve(listener, router(state)).await.expect("serve");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum_test::TestServer;

    fn server() -> TestServer {
        TestServer::new(router(AppState {
            inbox: Arc::new(RwLock::new(Vec::new())),
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn twilio_shape_returns_sid_and_lands_in_inbox() {
        let s = server();
        let res = s
            .post("/2010-04-01/Accounts/AC123/Messages.json")
            .add_header(
                axum::http::header::AUTHORIZATION,
                axum::http::HeaderValue::from_static("Basic QUJDMTIzOnRva2Vu"),
            )
            .form(&HashMap::from([
                ("To", "9876543210"),
                ("From", "+15551234567"),
                ("Body", "hello"),
            ]))
            .await;
        res.assert_status(StatusCode::CREATED);
        let sid = res.json::<serde_json::Value>()["sid"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(sid.starts_with("SM"), "twilio-style sid, got {sid}");

        let inbox = s.get("/api/messages").await;
        inbox.assert_status_ok();
        let body = inbox.json::<serde_json::Value>();
        assert_eq!(body["total"], 1);
        assert_eq!(body["data"][0]["to"], "9876543210");
        assert_eq!(body["data"][0]["provider"], "twilio");
    }

    #[tokio::test]
    async fn custom_shape_validates_and_stores() {
        let s = server();
        let bad = s
            .post("/send")
            .json(&serde_json::json!({"to": "", "body": "x"}))
            .await;
        bad.assert_status(StatusCode::BAD_REQUEST);

        let ok = s
            .post("/send")
            .json(&serde_json::json!({"to": "9000000001", "body": "hi", "sender": "1KM"}))
            .await;
        ok.assert_status(StatusCode::CREATED);
        let inbox = s.get("/api/messages").await;
        let body = inbox.json::<serde_json::Value>();
        assert_eq!(body["data"][0]["provider"], "custom");
        assert_eq!(body["data"][0]["from"], "1KM");
    }

    #[tokio::test]
    async fn inbox_caps_at_limit_newest_first() {
        let state = AppState {
            inbox: Arc::new(RwLock::new(Vec::new())),
        };
        for i in 0..(MAX_KEPT + 5) {
            store(
                &state,
                format!("{i}"),
                "s".to_string(),
                "b".to_string(),
                "twilio",
            );
        }
        let s = TestServer::new(router(state)).unwrap();
        let body = s.get("/api/messages").await.json::<serde_json::Value>();
        assert_eq!(body["total"], MAX_KEPT);
        assert_eq!(body["data"][0]["to"], format!("{}", MAX_KEPT + 4));
    }

    #[tokio::test]
    async fn healthz_and_index_serve() {
        let s = server();
        s.get("/healthz").await.assert_status_ok();
        let idx = s.get("/").await;
        idx.assert_status_ok();
        idx.assert_text_contains("fake-sms-server inbox");
    }
}
