use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::ws::Message;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use pg_rest_server_common::error::ApiError;
use pg_rest_server_common::state::AppState;
use resolute::Client;

use crate::backend::ResoluteBackend;

// ---------------------------------------------------------------------------
// Schema reload
// ---------------------------------------------------------------------------

/// POST /reload — rebuild the schema cache from the database.
pub async fn handle_reload(
    State(state): State<Arc<AppState<ResoluteBackend>>>,
) -> Result<Response, ApiError> {
    let client = Client::connect_from_str(&state.config.database.uri).await?;
    let cache =
        pg_schema_cache::resolute::build_schema_cache(&client, &state.config.database.schemas)
            .await?;

    let tables = cache.tables.len();
    let functions = cache.functions.len();
    state.schema_cache_tx.send(Arc::new(cache)).ok();

    let specs = state.rebuild_openapi_cache();
    *state.openapi_cache.write().await = specs;

    tracing::info!("Schema cache reloaded: {tables} tables, {functions} functions");

    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::json!({
            "message": "schema cache reloaded",
            "tables": tables,
            "functions": functions,
        })
        .to_string(),
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// WebSocket NOTIFY forwarding
// ---------------------------------------------------------------------------

/// GET /ws?channel=my_channel — WebSocket endpoint that forwards PostgreSQL
/// NOTIFY messages to connected clients as JSON frames.
pub async fn handle_ws(
    State(state): State<Arc<AppState<ResoluteBackend>>>,
    Query(params): Query<HashMap<String, String>>,
    ws: axum::extract::WebSocketUpgrade,
) -> Response {
    let channel = params
        .get("channel")
        .cloned()
        .unwrap_or_else(|| "pgrst".to_string());
    let uri = state.config.database.uri.clone();

    ws.on_upgrade(move |socket| ws_handler(socket, uri, channel))
}

async fn ws_handler(mut socket: axum::extract::ws::WebSocket, uri: String, channel: String) {
    use resolute::PgListener;

    let (user, password, host, port, database) = match parse_pg_uri_for_pool(&uri) {
        Some(t) => t,
        None => {
            let _ = socket
                .send(Message::Text(
                    serde_json::json!({"error": "invalid database URI"})
                        .to_string()
                        .into(),
                ))
                .await;
            return;
        }
    };
    let addr = format!("{host}:{port}");

    let mut listener = match PgListener::connect(&addr, &user, &password, &database).await {
        Ok(l) => l,
        Err(e) => {
            let _ = socket
                .send(Message::Text(
                    serde_json::json!({"error": e.to_string()})
                        .to_string()
                        .into(),
                ))
                .await;
            return;
        }
    };
    if let Err(e) = listener.listen(&channel).await {
        let _ = socket
            .send(Message::Text(
                serde_json::json!({"error": e.to_string()})
                    .to_string()
                    .into(),
            ))
            .await;
        return;
    }

    let (notify_tx, mut notify_rx) = tokio::sync::mpsc::unbounded_channel::<(String, String)>();
    tokio::spawn(async move {
        while let Ok(n) = listener.recv().await {
            if notify_tx.send((n.channel, n.payload)).is_err() {
                break;
            }
        }
    });

    loop {
        tokio::select! {
            Some((ch, payload)) = notify_rx.recv() => {
                let msg = serde_json::json!({
                    "channel": ch,
                    "payload": payload,
                });
                if socket.send(Message::Text(msg.to_string().into())).await.is_err() {
                    break;
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a postgres:// URI into (user, password, host, port, database).
/// Used by the WS handler to bootstrap a fresh PgListener on each upgrade.
fn parse_pg_uri_for_pool(uri: &str) -> Option<(String, String, String, u16, String)> {
    let rest = uri
        .strip_prefix("postgres://")
        .or_else(|| uri.strip_prefix("postgresql://"))?;
    let rest = rest.split('?').next().unwrap_or(rest);
    let (auth, hostdb) = rest.split_once('@').unwrap_or(("postgres:postgres", rest));
    let (user, password) = auth.split_once(':').unwrap_or((auth, ""));
    let (hostport, database) = hostdb.split_once('/').unwrap_or((hostdb, "postgres"));
    let (host, port_str) = hostport.split_once(':').unwrap_or((hostport, "5432"));
    let port: u16 = port_str.parse().unwrap_or(5432);
    Some((
        user.to_string(),
        password.to_string(),
        host.to_string(),
        port,
        database.to_string(),
    ))
}
