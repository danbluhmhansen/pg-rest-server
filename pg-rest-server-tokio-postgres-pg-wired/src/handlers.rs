use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Json, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use pg_query_engine::{
    build_count_sql, build_sql, parse_order, parse_select, ApiRequest, CountOption, DeleteRequest,
    FilterNode, FunctionCall, InsertRequest, ReadRequest, SelectItem, SqlOutput, UpdateRequest,
};
use pg_rest_server_common::auth::JwtClaims;
use pg_rest_server_common::handlers::*;
use pg_schema_cache::{ReturnType, SchemaCache};

use crate::auth::extract_jwt_claims;
use crate::error::ApiError;
use crate::state::AppState;

/// Execute via pg-wired: binary protocol pipelining.
/// Auth path: BEGIN + SET LOCAL ROLE + set_config + parameterized query + COMMIT
/// in ONE TCP write with binary-safe parameters.
/// Anon path: just the parameterized query (1 round trip).
async fn execute_wire(
    async_pool: &std::sync::Arc<pg_wired::AsyncPool>,
    claims: &Option<JwtClaims>,
    anon_setup_sql: &str,
    sql: &SqlOutput,
) -> Result<Option<String>, ApiError> {
    // Borrow params directly — no copy needed since sql.params are Strings.
    let param_refs: Vec<Option<&[u8]>> = sql.params.iter().map(|s| Some(s.as_bytes())).collect();
    let param_oids: Vec<u32> = vec![0; sql.params.len()];

    if claims.is_none() {
        // Anon: use pre-computed setup SQL (no allocation).
        let rows = async_pool
            .exec_transaction(anon_setup_sql, &sql.sql, &param_refs, &param_oids)
            .await
            .map_err(crate::error::map_wire_error)?;
        let json = rows
            .first()
            .and_then(|r| r.cell(0))
            // SAFETY: PostgreSQL text output is always valid UTF-8.
            .map(|b| unsafe { String::from_utf8_unchecked(b.to_vec()) });
        return Ok(json);
    }

    let setup_sql = build_setup_sql(claims, anon_setup_sql);

    let rows = async_pool
        .exec_transaction(&setup_sql, &sql.sql, &param_refs, &param_oids)
        .await
        .map_err(crate::error::map_wire_error)?;

    let json = rows
        .first()
        .and_then(|r| r.cell(0))
        .map(|b| String::from_utf8_lossy(b).into_owned());
    Ok(json)
}

/// Execute via pg-wired with optional count query.
///
/// The count query runs inside the same role / JWT setup as the data query so
/// RLS policies and role-scoped permissions apply to both. Skipping the role
/// setup made the count run as `authenticator`, which bypassed RLS and
/// produced totals larger than what the data query could see, surfacing as
/// spurious 206 Partial Content responses on full result sets.
async fn execute_wire_with_count(
    async_pool: &std::sync::Arc<pg_wired::AsyncPool>,
    claims: &Option<JwtClaims>,
    anon_setup_sql: &str,
    sql: &SqlOutput,
    count_sql: Option<&SqlOutput>,
) -> Result<(Option<String>, Option<i64>), ApiError> {
    let json = execute_wire(async_pool, claims, anon_setup_sql, sql).await?;

    let total = if let Some(csql) = count_sql {
        let cpr: Vec<Option<&[u8]>> = csql.params.iter().map(|s| Some(s.as_bytes())).collect();
        let co: Vec<u32> = vec![0; csql.params.len()];
        let setup_sql = build_setup_sql(claims, anon_setup_sql);
        let rows = async_pool
            .exec_transaction(&setup_sql, &csql.sql, &cpr, &co)
            .await
            .map_err(crate::error::map_wire_error)?;
        rows.first()
            .and_then(|r| r.cell(0))
            .and_then(|b| String::from_utf8_lossy(b).parse::<i64>().ok())
    } else {
        None
    };

    Ok((json, total))
}

// ---------------------------------------------------------------------------
// Route handlers
// ---------------------------------------------------------------------------

pub async fn handle_read(
    State(state): State<Arc<AppState>>,
    Path(table): Path<String>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let params = parse_query_pairs(raw_query.as_deref().unwrap_or(""));
    let claims = extract_jwt_claims(&headers, &state)?;
    let cache: Arc<SchemaCache> = state.schema_cache.borrow().clone();
    let schemas = resolve_schemas(&headers, &state.config.database.schemas)?;

    let table_meta = cache
        .find_table(&table, schemas)
        .ok_or_else(|| ApiError::TableNotFound(table.clone()))?;
    let table_qn = table_meta.name.clone();

    let select = parse_select(
        params
            .iter()
            .find(|(k, _)| k == "select")
            .map(|(_, v)| v.as_str())
            .unwrap_or("*"),
    )?;
    // PostgREST uses the last order param (not combined).
    let order_str = params
        .iter()
        .rfind(|(k, _)| k == "order")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    let order = parse_order(order_str)?;
    let filters = extract_filters_multi(&params)?;

    let (range_limit, range_offset) = parse_range(&headers);
    let limit = params
        .iter()
        .find(|(k, _)| k == "limit")
        .and_then(|(_, v)| v.parse().ok())
        .or(range_limit);
    let offset = params
        .iter()
        .find(|(k, _)| k == "offset")
        .and_then(|(_, v)| v.parse().ok())
        .or(range_offset);

    let prefs = parse_prefer(&headers);
    let singular = wants_singular(&headers);

    let read_req = ReadRequest {
        table: table_qn,
        select,
        filters,
        order,
        limit,
        offset,
        count: prefs.count,
    };

    let mut sql = build_sql(&cache, &ApiRequest::Read(read_req.clone()), schemas)?;

    // EXPLAIN support: prepend EXPLAIN to return the query plan.
    if wants_explain(&headers) {
        sql.sql = format!("EXPLAIN (FORMAT JSON) {}", sql.sql);
        let role = claims
            .as_ref()
            .map(|c| c.role.as_str())
            .unwrap_or(&state.config.database.anon_role);
        let quoted_role = format!("\"{}\"", role.replace('"', "\"\""));

        // Execute EXPLAIN via pg-wired AsyncPool transaction.
        let setup_sql = format!("BEGIN; SET LOCAL ROLE {quoted_role}");
        let param_refs: Vec<Option<&[u8]>> =
            sql.params.iter().map(|s| Some(s.as_bytes())).collect();
        let param_oids: Vec<u32> = vec![0; sql.params.len()];
        let rows = state
            .async_pool
            .exec_transaction(&setup_sql, &sql.sql, &param_refs, &param_oids)
            .await
            .map_err(crate::error::map_wire_error)?;

        // EXPLAIN FORMAT JSON returns a single text row.
        let plan_text = rows
            .first()
            .and_then(|r| r.cell(0))
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_else(|| "[]".to_string());

        // Parse the text as JSON (PostgreSQL returns valid JSON text).
        let plan: serde_json::Value =
            serde_json::from_str(&plan_text).unwrap_or(serde_json::json!([]));

        return Ok((
            StatusCode::OK,
            [(
                header::CONTENT_TYPE.as_str(),
                "application/vnd.pgrst.plan+json",
            )],
            plan.to_string(),
        )
            .into_response());
    }

    let count_sql = if prefs.count == CountOption::Exact {
        Some(build_count_sql(&cache, &read_req, schemas)?)
    } else {
        None
    };

    let (json, total) = execute_wire_with_count(
        &state.async_pool,
        &claims,
        &state.config.database.anon_role,
        &sql,
        count_sql.as_ref(),
    )
    .await?;

    let body = json.unwrap_or_else(|| "[]".to_string());

    // Lean mode: skip ETag, Content-Range, singular, CSV — return JSON directly.
    if std::env::var("PG_REST_LEAN").is_ok() {
        return Ok((
            StatusCode::OK,
            [(header::CONTENT_TYPE.as_str(), "application/json")],
            body,
        )
            .into_response());
    }

    // ETag: simple hash of the response body.
    let etag = format!("\"{}\"", simple_hash(&body));
    if let Some(if_none_match) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        if if_none_match == etag || if_none_match == "*" {
            return Ok(StatusCode::NOT_MODIFIED.into_response());
        }
    }

    // Build Content-Range header.
    let content_range = if let Some(total) = total {
        let off = offset.unwrap_or(0);
        let row_count = count_json_array(&body);
        if row_count == 0 {
            format!("*/{total}")
        } else {
            format!("{}-{}/{total}", off, off + row_count as i64 - 1)
        }
    } else {
        "*/*".to_string()
    };

    // Singular response (application/vnd.pgrst.object+json).
    if singular {
        let obj = to_singular(&body)?;
        return Ok((
            StatusCode::OK,
            [
                (
                    header::CONTENT_TYPE.as_str(),
                    "application/vnd.pgrst.object+json",
                ),
                ("content-range", &content_range),
            ],
            obj,
        )
            .into_response());
    }

    // Content negotiation: CSV or JSON.
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json");

    let (content_type, response_body) = if accept.contains("text/csv") {
        ("text/csv", json_array_to_csv(&body))
    } else {
        ("application/json", body)
    };

    // PostgREST returns 206 Partial Content when the result is a subset of the total.
    let row_count = count_json_array(&response_body);
    let status = if let Some(total) = total {
        if row_count < total as usize {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        }
    } else {
        StatusCode::OK
    };

    Ok((
        status,
        [
            (header::CONTENT_TYPE.as_str(), content_type),
            ("content-range", &content_range),
            (header::ETAG.as_str(), &etag),
        ],
        response_body,
    )
        .into_response())
}

pub async fn handle_insert(
    State(state): State<Arc<AppState>>,
    Path(table): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Response, ApiError> {
    let claims = extract_jwt_claims(&headers, &state)?;
    let cache: Arc<SchemaCache> = state.schema_cache.borrow().clone();
    let schemas = &state.config.database.schemas;
    let prefs = parse_prefer(&headers);

    let table_meta = cache
        .find_table(&table, schemas)
        .ok_or_else(|| ApiError::TableNotFound(table.clone()))?;
    if !table_meta.insertable {
        return Err(ApiError::MethodNotAllowed);
    }
    let table_qn = table_meta.name.clone();

    let rows = body_to_rows(body)?;
    let returning = if prefs.return_pref == ReturnPreference::Representation {
        vec!["*".to_string()]
    } else if !table_meta.primary_key.is_empty() {
        // Return PK columns for the Location header even on minimal return.
        table_meta.primary_key.clone()
    } else {
        Vec::new()
    };

    let on_conflict_columns = params
        .get("on_conflict")
        .map(|s| s.split(',').map(|c| c.trim().to_string()).collect());

    let req = ApiRequest::Insert(InsertRequest {
        table: table_qn,
        rows,
        on_conflict: prefs.resolution,
        on_conflict_columns,
        returning,
    });

    let sql = build_sql(&cache, &req, schemas)?;
    let json = execute_wire(&state.async_pool, &claims, &state.anon_setup_sql, &sql).await?;

    // Build Location header from PK of the first inserted row.
    let location = json.as_deref().and_then(|body| {
        let arr: Vec<serde_json::Value> = serde_json::from_str(body).ok()?;
        let first = arr.first()?;
        let pk_filter: Vec<String> = table_meta
            .primary_key
            .iter()
            .filter_map(|pk| {
                let val = first.get(pk)?;
                let val_str = match val {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                Some(format!("{pk}=eq.{val_str}"))
            })
            .collect();
        if pk_filter.is_empty() {
            None
        } else {
            Some(format!("/{table}?{}", pk_filter.join("&")))
        }
    });

    let mut resp = match (prefs.return_pref, json) {
        (ReturnPreference::Representation, Some(j)) => (
            StatusCode::CREATED,
            [(header::CONTENT_TYPE, "application/json")],
            j,
        )
            .into_response(),
        _ => StatusCode::CREATED.into_response(),
    };

    if let Some(loc) = location {
        if let Ok(val) = loc.parse() {
            resp.headers_mut().insert(header::LOCATION, val);
        }
    }

    Ok(resp)
}

pub async fn handle_update(
    State(state): State<Arc<AppState>>,
    Path(table): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Response, ApiError> {
    let claims = extract_jwt_claims(&headers, &state)?;
    let cache: Arc<SchemaCache> = state.schema_cache.borrow().clone();
    let schemas = &state.config.database.schemas;
    let prefs = parse_prefer(&headers);

    let table_meta = cache
        .find_table(&table, schemas)
        .ok_or_else(|| ApiError::TableNotFound(table.clone()))?;
    if !table_meta.updatable {
        return Err(ApiError::MethodNotAllowed);
    }
    let table_qn = table_meta.name.clone();

    let set = match body {
        serde_json::Value::Object(m) => m,
        _ => return Err(ApiError::BadRequest("expected JSON object".into())),
    };

    let filters = extract_filters(&params)?;
    let returning = if prefs.return_pref == ReturnPreference::Representation {
        vec!["*".to_string()]
    } else {
        Vec::new()
    };

    let req = ApiRequest::Update(UpdateRequest {
        table: table_qn,
        set,
        filters,
        returning,
    });

    let sql = build_sql(&cache, &req, schemas)?;
    let json = execute_wire(&state.async_pool, &claims, &state.anon_setup_sql, &sql).await?;

    match json {
        Some(j) => Ok((
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            j,
        )
            .into_response()),
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

pub async fn handle_delete(
    State(state): State<Arc<AppState>>,
    Path(table): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let claims = extract_jwt_claims(&headers, &state)?;
    let cache: Arc<SchemaCache> = state.schema_cache.borrow().clone();
    let schemas = &state.config.database.schemas;
    let prefs = parse_prefer(&headers);

    let table_meta = cache
        .find_table(&table, schemas)
        .ok_or_else(|| ApiError::TableNotFound(table.clone()))?;
    if !table_meta.deletable {
        return Err(ApiError::MethodNotAllowed);
    }
    let table_qn = table_meta.name.clone();

    let filters = extract_filters(&params)?;
    let returning = if prefs.return_pref == ReturnPreference::Representation {
        vec!["*".to_string()]
    } else {
        Vec::new()
    };

    let req = ApiRequest::Delete(DeleteRequest {
        table: table_qn,
        filters,
        returning,
    });

    let sql = build_sql(&cache, &req, schemas)?;
    let json = execute_wire(&state.async_pool, &claims, &state.anon_setup_sql, &sql).await?;

    match json {
        Some(j) => Ok((
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            j,
        )
            .into_response()),
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

pub async fn handle_rpc(
    State(state): State<Arc<AppState>>,
    Path(function): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Option<Json<serde_json::Value>>,
) -> Result<Response, ApiError> {
    let claims = extract_jwt_claims(&headers, &state)?;
    let cache: Arc<SchemaCache> = state.schema_cache.borrow().clone();
    let schemas = &state.config.database.schemas;

    let func = cache
        .find_function(&function, schemas)
        .ok_or_else(|| ApiError::FunctionNotFound(function.clone()))?;

    let func_qn = func.name.clone();
    let is_scalar = matches!(func.return_type, ReturnType::Scalar(_) | ReturnType::Void);

    // Function params from body (POST) or query string (GET).
    let func_params = if let Some(Json(body)) = body {
        match body {
            serde_json::Value::Object(m) => m,
            _ => return Err(ApiError::BadRequest("expected JSON object".into())),
        }
    } else {
        params
            .iter()
            .filter(|(k, _)| !RESERVED_PARAMS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect()
    };

    // Optional filtering/ordering of function results.
    let select = parse_select(params.get("select").map(String::as_str).unwrap_or("*"))?;
    let order = parse_order(params.get("order").map(String::as_str).unwrap_or(""))?;

    let has_select = select.iter().any(|s| !matches!(s, SelectItem::Star));
    let read_request = if has_select || !order.is_empty() {
        Some(ReadRequest {
            table: func_qn.clone(),
            select,
            filters: FilterNode::empty(),
            order,
            limit: None,
            offset: None,
            count: CountOption::None,
        })
    } else {
        None
    };

    let req = ApiRequest::CallFunction(FunctionCall {
        function: func_qn,
        params: func_params,
        is_scalar,
        read_request,
    });

    let sql = build_sql(&cache, &req, schemas)?;
    let json = execute_wire(&state.async_pool, &claims, &state.anon_setup_sql, &sql).await?;

    // Void functions return 204 No Content (PostgREST compat).
    if matches!(func.return_type, ReturnType::Void) {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }

    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        json.unwrap_or_else(|| "null".to_string()),
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// Root — OpenAPI spec
// ---------------------------------------------------------------------------

/// Serves the OpenAPI specification at `GET /`.
/// Defaults to OpenAPI 2.0 (Swagger) for PostgREST compatibility.
/// Use `?openapi-version=3` for OpenAPI 3.0.
pub async fn handle_root(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let cached = state.openapi_cache.read().await;
    let spec = match params.get("openapi-version").map(String::as_str) {
        Some("3") | Some("3.0") => cached.1.clone(),
        _ => cached.0.clone(),
    };
    drop(cached);

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/openapi+json")],
        spec,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Schema reload
// ---------------------------------------------------------------------------

/// POST /reload — rebuild the schema cache from the database.
pub async fn handle_reload(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    // Use a one-off tokio-postgres connection for schema introspection.
    let (client, conn) = tokio_postgres::connect(&state.config.database.uri, tokio_postgres::NoTls)
        .await
        .map_err(ApiError::Database)?;
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

    // Rebuild cached OpenAPI specs.
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
// Health endpoints
// ---------------------------------------------------------------------------

pub async fn handle_live() -> StatusCode {
    StatusCode::OK
}

pub async fn handle_ready(State(state): State<Arc<AppState>>) -> StatusCode {
    match state.conn_pool.get().await {
        Ok(_) => StatusCode::OK,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// Prometheus-compatible metrics endpoint.
pub async fn handle_metrics(State(state): State<Arc<AppState>>) -> Response {
    let pool_metrics = state.conn_pool.metrics();
    let cache = state.schema_cache.borrow();

    let body = format!(
        "# HELP pg_rest_pool_size Current pool size\n\
         # TYPE pg_rest_pool_size gauge\n\
         pg_rest_pool_size {}\n\
         # HELP pg_rest_pool_available Available connections in pool\n\
         # TYPE pg_rest_pool_available gauge\n\
         pg_rest_pool_available {}\n\
         # HELP pg_rest_pool_in_use Connections currently checked out\n\
         # TYPE pg_rest_pool_in_use gauge\n\
         pg_rest_pool_in_use {}\n\
         # HELP pg_rest_pool_checkouts Total checkouts since startup\n\
         # TYPE pg_rest_pool_checkouts counter\n\
         pg_rest_pool_checkouts {}\n\
         # HELP pg_rest_pool_timeouts Total checkout timeouts since startup\n\
         # TYPE pg_rest_pool_timeouts counter\n\
         pg_rest_pool_timeouts {}\n\
         # HELP pg_rest_async_pool_size AsyncPool connection count\n\
         # TYPE pg_rest_async_pool_size gauge\n\
         pg_rest_async_pool_size {}\n\
         # HELP pg_rest_schema_tables Number of tables in schema cache\n\
         # TYPE pg_rest_schema_tables gauge\n\
         pg_rest_schema_tables {}\n\
         # HELP pg_rest_schema_functions Number of functions in schema cache\n\
         # TYPE pg_rest_schema_functions gauge\n\
         pg_rest_schema_functions {}\n",
        pool_metrics.total,
        pool_metrics.idle,
        pool_metrics.in_use,
        pool_metrics.total_checkouts,
        pool_metrics.total_timeouts,
        state.async_pool.size(),
        cache.tables.len(),
        cache.functions.len(),
    );

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        body,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// WebSocket NOTIFY forwarding
// ---------------------------------------------------------------------------

/// GET /ws?channel=my_channel — WebSocket endpoint that forwards PostgreSQL
/// NOTIFY messages to connected clients as JSON frames.
pub async fn handle_ws(
    State(state): State<Arc<AppState>>,
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
    use axum::extract::ws::Message;

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
                    _ => {} // ignore other messages
                }
            }
        }
    }
}
