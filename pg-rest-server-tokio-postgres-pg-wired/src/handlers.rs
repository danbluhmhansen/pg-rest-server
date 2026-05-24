use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::ws::Message;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use pg_rest_server_common::error::ApiError;
use pg_rest_server_common::state::AppState;

use crate::backend::PgWiredBackend;

// ---------------------------------------------------------------------------
// Schema reload
// ---------------------------------------------------------------------------

/// POST /reload — rebuild the schema cache from the database.
pub async fn handle_reload(
    State(state): State<Arc<AppState<PgWiredBackend>>>,
) -> Result<Response, ApiError> {
    let (client, conn) = tokio_postgres::connect(&state.config.database.uri, tokio_postgres::NoTls)
        .await
        .map_err(|e| ApiError::Database(Box::new(e)))?;
    tokio::spawn(async move {
        conn.await.ok();
    });
    let cache = pg_schema_cache::tokio_postgres::build_schema_cache(
        &client,
        &state.config.database.schemas,
    )
    .await?;
    drop(client);

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
    State(state): State<Arc<AppState<PgWiredBackend>>>,
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
    let conn = tokio_postgres::connect(&uri, tokio_postgres::NoTls).await;
    let (client, mut connection) = match conn {
        Ok(c) => c,
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

    let (notify_tx, mut notify_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            match std::future::poll_fn(|cx| connection.poll_message(cx)).await {
                Some(Ok(tokio_postgres::AsyncMessage::Notification(n))) => {
                    if notify_tx.send(n).is_err() {
                        break;
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            }
        }
    });

    let quoted = format!("\"{}\"", channel.replace('"', "\"\""));
    if client
        .execute(&format!("LISTEN {quoted}"), &[])
        .await
        .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            Some(notification) = notify_rx.recv() => {
                let msg = serde_json::json!({
                    "channel": notification.channel(),
                    "payload": notification.payload(),
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


