//! Loopback HTTP: the app page and a small token-guarded API. The server binds to 127.0.0.1 only.
//! Any web page could still try to reach loopback, so every API call needs the launch token.

use crate::api;
use crate::app::App;
use crate::chat;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Html;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

const PAGE: &str = include_str!("page.html");
const TOKENS_CSS: &str = include_str!("ui/tokens.css");
const COMPONENTS_CSS: &str = include_str!("ui/components.css");
const TOKEN_HEADER: &str = "x-fx-token";
const MAX_MESSAGE_CHARS: usize = 2_000;
const MIN_TOKEN_LEN: usize = 32;
const MAX_NONCE_LEN: usize = 64;
const NOT_AUTHORISED: &str = "Not authorised. Open the app from its launcher.";

#[derive(Deserialize)]
struct ChatIn {
    message: String,
    #[serde(default)]
    history: Vec<chat::Turn>,
}

/// The page with the design tokens and component styles inlined, built once.
fn page() -> &'static str {
    static BUILT: OnceLock<String> = OnceLock::new();
    BUILT.get_or_init(|| {
        PAGE.replace("/*TOKENS_CSS*/", TOKENS_CSS)
            .replace("/*COMPONENTS_CSS*/", COMPONENTS_CSS)
    })
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(|| async { Html(page()) }))
        .route("/api/chat", post(chat_route))
        .route("/api/overview", get(overview_route))
        .route("/api/control", get(control_get).post(control_post))
        .route("/api/events", get(events_route))
        .route("/api/export", get(export_route))
        .route("/api/ping", get(ping_route))
        .route("/api/update", get(update_get).post(update_post))
        .with_state(app)
}

/// Compares without stopping at the first different byte.
fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

fn authorised(app: &App, headers: &HeaderMap) -> bool {
    headers
        .get(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t.len() >= MIN_TOKEN_LEN && same_secret(t, &app.cfg.token))
}

/// Proves to the Android app that this is the daemon holding the token, without sending it:
/// the answer is a hash of the token and the caller's nonce.
async fn ping_route(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    let nonce = q.get("nonce").map_or("", String::as_str);
    if nonce.is_empty() || nonce.len() > MAX_NONCE_LEN {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "nonce required"})),
        );
    }
    let digest = Sha256::digest(format!("{}{nonce}", app.cfg.token).as_bytes());
    let proof: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    (StatusCode::OK, Json(json!({"proof": proof})))
}

async fn update_get(State(app): State<Arc<App>>, headers: HeaderMap) -> (StatusCode, Json<Value>) {
    if !authorised(&app, &headers) {
        return denied();
    }
    (StatusCode::OK, Json(app.update_json()))
}

async fn update_post(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if !authorised(&app, &headers) {
        return denied();
    }
    match app.update_action(&body).await {
        Ok(v) => (StatusCode::OK, Json(v)),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))),
    }
}

async fn events_route(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    if !authorised(&app, &headers) {
        return denied();
    }
    let since = q.get("since").and_then(|s| s.parse().ok()).unwrap_or(-1);
    (StatusCode::OK, Json(api::events(&app, since)))
}

async fn export_route(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> axum::response::Response {
    if !authorised(&app, &headers) {
        return denied().into_response();
    }
    match api::export(&app, q.get("kind").map_or("", String::as_str)) {
        Ok((body, mime)) => ([(header::CONTENT_TYPE, mime)], body).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

fn denied() -> (StatusCode, Json<Value>) {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": NOT_AUTHORISED, "reply": NOT_AUTHORISED})),
    )
}

async fn chat_route(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<ChatIn>,
) -> (StatusCode, Json<Value>) {
    if !authorised(&app, &headers) {
        return denied();
    }
    let message: String = body.message.chars().take(MAX_MESSAGE_CHARS).collect();
    let r = chat::handle(&app, &message, &body.history).await;
    (
        StatusCode::OK,
        Json(json!({"reply": r.text, "card": r.card})),
    )
}

async fn overview_route(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    if !authorised(&app, &headers) {
        return denied();
    }
    (StatusCode::OK, Json(api::overview(&app)))
}

async fn control_get(State(app): State<Arc<App>>, headers: HeaderMap) -> (StatusCode, Json<Value>) {
    if !authorised(&app, &headers) {
        return denied();
    }
    (StatusCode::OK, Json(api::control(&app)))
}

async fn control_post(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if !authorised(&app, &headers) {
        return denied();
    }
    match api::set_control(&app, &body) {
        Ok(v) => (StatusCode::OK, Json(v)),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))),
    }
}
