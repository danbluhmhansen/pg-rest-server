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
use resolute::{Client, SharedPool};

use crate::auth::extract_jwt_claims;
use crate::error::ApiError;
use crate::state::AppState;

/// Execute the request as a single pipelined batched transaction.
/// Returns the (JSON body, optional count) tuple.
///
/// The count query, when requested, runs as a separate pipelined transaction
/// so its role/JWT context matches the data query (RLS sees the same rows).
async fn run_request(
    pool: &SharedPool,
    claims: &Option<JwtClaims>,
    anon_setup_sql: &str,
    sql: &SqlOutput,
    count_sql: Option<&SqlOutput>,
) -> Result<(Option<String>, Option<i64>), ApiError> {
    let setup_sql = build_setup_sql(claims, anon_setup_sql);

    let param_refs: Vec<Option<&[u8]>> = sql.params.iter().map(|s| Some(s.as_bytes())).collect();
    let param_oids: Vec<u32> = vec![0; sql.params.len()];

    let rows = pool
        .exec_transaction(&setup_sql, &sql.sql, &param_refs, &param_oids)
        .await
        .map_err(ApiError::from)?;
    let json = rows
        .first()
        .and_then(|r| r.cell(0))
        .map(|b| String::from_utf8_lossy(b).into_owned());

    let total = if let Some(csql) = count_sql {
        let cpr: Vec<Option<&[u8]>> = csql.params.iter().map(|s| Some(s.as_bytes())).collect();
        let co: Vec<u32> = vec![0; csql.params.len()];
        let crows = pool
            .exec_transaction(&setup_sql, &csql.sql, &cpr, &co)
            .await
            .map_err(ApiError::from)?;
        crows
            .first()
            .and_then(|r| r.cell(0))
            .and_then(|b| String::from_utf8_lossy(b).parse::<i64>().ok())
    } else {
        None
    };

    Ok((json, total))
}

/// Execute a request that doesn't need a count.
async fn run_request_no_count(
    pool: &SharedPool,
    claims: &Option<JwtClaims>,
    anon_setup_sql: &str,
    sql: &SqlOutput,
) -> Result<Option<String>, ApiError> {
    let (json, _) = run_request(pool, claims, anon_setup_sql, sql, None).await?;
    Ok(json)
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
        let plan_json =
            run_request_no_count(&state.pool, &claims, &state.anon_setup_sql, &sql).await?;
        let plan_text = plan_json.unwrap_or_else(|| "[]".to_string());
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

    let (json, total) = run_request(
        &state.pool,
        &claims,
        &state.anon_setup_sql,
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
    let json = run_request_no_count(&state.pool, &claims, &state.anon_setup_sql, &sql).await?;

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
    let json = run_request_no_count(&state.pool, &claims, &state.anon_setup_sql, &sql).await?;

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
    let json = run_request_no_count(&state.pool, &claims, &state.anon_setup_sql, &sql).await?;

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
    let json = run_request_no_count(&state.pool, &claims, &state.anon_setup_sql, &sql).await?;

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
    // Use a one-off resolute Client for schema introspection.
    let client = Client::connect_from_str(&state.config.database.uri).await?;
    let cache =
        pg_schema_cache::resolute::build_schema_cache(&client, &state.config.database.schemas)
            .await?;

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
    if state.pool.alive_count().await > 0 {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

/// Prometheus-compatible metrics endpoint.
pub async fn handle_metrics(State(state): State<Arc<AppState>>) -> Response {
    let pool_size = state.pool.size();
    let pool_alive = state.pool.alive_count().await;
    let cache = state.schema_cache.borrow();

    let body = format!(
        "# HELP pg_rest_pool_size Configured pool size\n\
         # TYPE pg_rest_pool_size gauge\n\
         pg_rest_pool_size {}\n\
         # HELP pg_rest_pool_alive Live connections in the shared pool\n\
         # TYPE pg_rest_pool_alive gauge\n\
         pg_rest_pool_alive {}\n\
         # HELP pg_rest_schema_tables Number of tables in schema cache\n\
         # TYPE pg_rest_schema_tables gauge\n\
         pg_rest_schema_tables {}\n\
         # HELP pg_rest_schema_functions Number of functions in schema cache\n\
         # TYPE pg_rest_schema_functions gauge\n\
         pg_rest_schema_functions {}\n",
        pool_size,
        pool_alive,
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
                    _ => {} // ignore other messages
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
