//! Fake SMS gateway for 1KM dev: Twilio-compatible receive endpoint +
//! custom-bridge endpoint, with a web inbox. Ephemeral by design —
//! bounded in-memory store, nothing persists restarts.
//!
//! Endpoints:
//! ```text
//! POST /2010-04-01/Accounts/:sid/Messages.json  Twilio shape (form To/From/Body)
//! POST /send                                     custom bridge shape (JSON to/body/sender?)
//! GET  /api/messages                            inbox, newest first (JSON)
//! GET  /rows?q=&provider=                       inbox rows (htmx HTML partial)
//! GET  /stats                                   counts (htmx HTML partial)
//! POST /clear                                   empty the inbox (returns rows partial)
//! GET  /healthz                                 liveness
//! GET  /                                        htmx web inbox UI
//! ```
//!
//! Auth is accepted but never verified (any Basic credentials pass) —
//! this is a dev fake, never exposed publicly.

use std::sync::{Arc, RwLock};

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

fn store(
    state: &AppState,
    to: String,
    from: String,
    body: String,
    provider: &'static str,
) -> Message {
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
    let mut items = state.inbox.read().map(|g| g.clone()).unwrap_or_default();
    items.sort_by_key(|m| std::cmp::Reverse(m.received_at));
    Json(serde_json::json!({ "data": items, "total": items.len() }))
}

async fn healthz() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

/// HTML-escape user-controlled text (message bodies land in the UI).
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[derive(Debug, serde::Deserialize)]
struct RowsQuery {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    provider: Option<String>,
}

/// Newest-first inbox with optional substring (`to`/`from`/`body`,
/// case-insensitive) and provider filters.
fn filtered(state: &AppState, q: &RowsQuery) -> Vec<Message> {
    let mut items = state.inbox.read().map(|g| g.clone()).unwrap_or_default();
    items.sort_by_key(|m| std::cmp::Reverse(m.received_at));
    let needle = q.q.as_deref().map(str::to_lowercase).unwrap_or_default();
    // Empty query params (the filter form always submits both fields)
    // mean "no filter", not "match empty".
    let provider = q.provider.as_deref().filter(|p| !p.is_empty());
    items
        .into_iter()
        .filter(|m| {
            provider.is_none_or(|p| m.provider == p)
                && (needle.is_empty()
                    || m.to.to_lowercase().contains(&needle)
                    || m.from.to_lowercase().contains(&needle)
                    || m.body.to_lowercase().contains(&needle))
        })
        .collect()
}

fn short_body(body: &str) -> String {
    const MAX: usize = 90;
    if body.chars().count() <= MAX {
        body.to_string()
    } else {
        format!("{}…", body.chars().take(MAX).collect::<String>())
    }
}

fn rows_html(items: &[Message]) -> String {
    if items.is_empty() {
        return "<tr><td colspan=\"5\" class=\"empty\">No messages yet — request an OTP from the 1KM server and it lands here.</td></tr>"
            .to_string();
    }
    let mut out = String::new();
    for m in items {
        out.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}<details><summary>full</summary><div class=\"full\">{}</div><div class=\"meta\">sid: {}<br>id: {}</div></details></td><td><span class=\"badge {}\">{}</span></td><td class=\"at\">{}</td></tr>",
            esc(&m.to),
            esc(&m.from),
            esc(&short_body(&m.body)),
            esc(&m.body),
            esc(&m.sid),
            m.id,
            m.provider,
            esc(m.provider),
            m.received_at.format("%d %b %H:%M:%S"),
        ));
    }
    out
}

fn stats_html(state: &AppState) -> String {
    let inbox = state.inbox.read().map(|g| g.clone()).unwrap_or_default();
    let twilio = inbox.iter().filter(|m| m.provider == "twilio").count();
    let custom = inbox.len() - twilio;
    format!(
        "<strong>{}</strong> messages · <span class=\"badge twilio\">twilio {}</span> <span class=\"badge custom\">custom {}</span>",
        inbox.len(),
        twilio,
        custom
    )
}

async fn rows(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<RowsQuery>,
) -> Html<String> {
    Html(rows_html(&filtered(&state, &q)))
}

async fn stats(State(state): State<AppState>) -> Html<String> {
    Html(stats_html(&state))
}

async fn clear(State(state): State<AppState>) -> Html<String> {
    state
        .inbox
        .write()
        .map(|mut g| g.clear())
        .unwrap_or_else(|_| tracing::error!("inbox poisoned"));
    tracing::info!("inbox cleared from web UI");
    Html(rows_html(&[]))
}

async fn htmx_js() -> (
    [(axum::http::header::HeaderName, &'static str); 1],
    &'static str,
) {
    (
        [(axum::http::header::CONTENT_TYPE, "text/javascript")],
        include_str!("../htmx.min.js"),
    )
}

const INDEX_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>fake-sms-server inbox</title>
<script src="/htmx.min.js"></script>
<style>
:root { color-scheme: light dark; }
body { font-family: system-ui, sans-serif; max-width: 860px; margin: 2rem auto; padding: 0 1rem; }
header { display: flex; align-items: baseline; gap: 1rem; flex-wrap: wrap; }
header h1 { margin: 0; font-size: 1.4rem; }
#stats { color: #666; font-size: .9rem; }
form#filters { display: flex; gap: .5rem; margin: 1rem 0; flex-wrap: wrap; }
input[type=search], select { font: inherit; padding: .4rem .6rem; border: 1px solid #ccc; border-radius: 6px; }
input[type=search] { flex: 1; min-width: 200px; }
table { border-collapse: collapse; width: 100%; }
th, td { border: 1px solid #ccc; padding: .4rem .6rem; text-align: left; font-size: .9rem; vertical-align: top; }
td.at { white-space: nowrap; }
td.empty { text-align: center; color: #666; padding: 2rem; }
details summary { cursor: pointer; color: #666; font-size: .8rem; }
.full { white-space: pre-wrap; margin-top: .4rem; }
.meta { color: #666; font-size: .75rem; margin-top: .4rem; }
.badge { font-size: .75rem; padding: .1rem .4rem; border-radius: 4px; background: #eee; white-space: nowrap; }
.badge.twilio { background: #dff0e0; }
.badge.custom { background: #dde8f5; }
.toolbar { display: flex; justify-content: space-between; align-items: center; margin-top: 1rem; color: #666; font-size: .8rem; }
button.danger { font: inherit; padding: .4rem .8rem; border-radius: 6px; border: 1px solid #c00; background: none; color: #c00; cursor: pointer; }
</style>
</head>
<body>
<header>
<h1>fake-sms-server inbox</h1>
<div id="stats" hx-get="/stats" hx-trigger="load, every 3s" hx-swap="innerHTML">…</div>
</header>
<form id="filters" hx-get="/rows" hx-target="#rows" hx-swap="innerHTML"
      hx-trigger="load, every 3s, keyup changed delay:300ms from:#q, change from:#provider">
<input id="q" name="q" type="search" placeholder="Search to / from / body…" autocomplete="off">
<select id="provider" name="provider">
<option value="">All providers</option>
<option value="twilio">twilio</option>
<option value="custom">custom</option>
</select>
</form>
<table>
<thead><tr><th>To</th><th>From</th><th>Body</th><th>Via</th><th>At</th></tr></thead>
<tbody id="rows"></tbody>
</table>
<div class="toolbar">
<span>Ephemeral — restarts wipe the inbox.</span>
<button class="danger" hx-post="/clear" hx-confirm="Delete all messages?" hx-target="#rows" hx-swap="innerHTML">Clear inbox</button>
</div>
</body>
</html>"##;

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
        .route("/rows", get(rows))
        .route("/stats", get(stats))
        .route("/clear", post(clear))
        .route("/htmx.min.js", get(htmx_js))
        .route("/healthz", get(healthz))
        .route("/", get(index))
        .with_state(state)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
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
    use std::collections::HashMap;

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
        idx.assert_text_contains("htmx.min.js");
        let js = s.get("/htmx.min.js").await;
        js.assert_status_ok();
    }

    #[tokio::test]
    async fn rows_filter_search_and_clear() {
        let s = server();
        // Two custom messages via /send…
        for (to, body) in [("111", "otp 123456"), ("222", "hello there")] {
            s.post("/send")
                .json(&serde_json::json!({"to": to, "body": body, "sender": "1KM"}))
                .await
                .assert_status(StatusCode::CREATED);
        }
        // Both stored as custom via /send; add a twilio one directly.
        let res = s
            .post("/2010-04-01/Accounts/AC1/Messages.json")
            .form(&HashMap::from([
                ("To", "333"),
                ("From", "+1555"),
                ("Body", "twilio hello"),
            ]))
            .await;
        res.assert_status(StatusCode::CREATED);

        let rows = s.get("/rows").await;
        rows.assert_status_ok();
        let html = rows.text();
        assert!(html.contains("111") && html.contains("twilio hello"));

        // The filter form always submits both fields; empty means all.
        let all = s.get("/rows?q=&provider=").await;
        all.assert_text_contains("twilio hello");

        // Provider filter isolates.
        let tw = s.get("/rows?provider=twilio").await;
        let html = tw.text();
        assert!(html.contains("twilio hello"));
        assert!(!html.contains(">111<"));

        // Substring search across to/from/body.
        let q = s.get("/rows?q=hello%20there").await;
        q.assert_text_contains("222");
        let none = s.get("/rows?q=zzz-no-match").await;
        none.assert_text_contains("No messages yet");

        // Stats count both providers.
        let stats = s.get("/stats").await;
        stats.assert_text_contains("twilio 1");

        // Bodies are HTML-escaped in the UI (XSS-safe).
        s.post("/send")
            .json(&serde_json::json!({"to": "444", "body": "<script>alert(1)</script>"}))
            .await;
        let esc = s.get("/rows?q=alert").await;
        let html = esc.text();
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("<script>alert"));

        // Clear wipes everything.
        let cleared = s.post("/clear").await;
        cleared.assert_status_ok();
        cleared.assert_text_contains("No messages yet");
        let inbox = s.get("/api/messages").await.json::<serde_json::Value>();
        assert_eq!(inbox["total"], 0);
    }
}
