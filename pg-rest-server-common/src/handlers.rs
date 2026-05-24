use std::collections::HashMap;

use axum::http::{header, HeaderMap};
use pg_query_engine::{parse_filter, parse_logic_filter, ConflictAction, CountOption, FilterNode};

use crate::auth::JwtClaims;

// ---------------------------------------------------------------------------
// Query-string params that are NOT filters
// ---------------------------------------------------------------------------

pub const RESERVED_PARAMS: &[&str] = &["select", "order", "limit", "offset"];

// ---------------------------------------------------------------------------
// Prefer header
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReturnPreference {
    Minimal,
    HeadersOnly,
    Representation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandlingMode {
    Lenient,
    Strict,
}

pub struct Preferences {
    pub return_pref: ReturnPreference,
    pub count: CountOption,
    pub resolution: Option<ConflictAction>,
    pub handling: HandlingMode,
}

pub fn parse_prefer(headers: &HeaderMap) -> Preferences {
    let mut prefs = Preferences {
        return_pref: ReturnPreference::Minimal,
        count: CountOption::None,
        resolution: None,
        handling: HandlingMode::Lenient,
    };

    for value in headers.get_all("prefer") {
        if let Ok(s) = value.to_str() {
            for part in s.split(',') {
                match part.trim() {
                    "return=representation" => prefs.return_pref = ReturnPreference::Representation,
                    "return=headers-only" => prefs.return_pref = ReturnPreference::HeadersOnly,
                    "return=minimal" => prefs.return_pref = ReturnPreference::Minimal,
                    "count=exact" => prefs.count = CountOption::Exact,
                    "count=planned" => prefs.count = CountOption::Planned,
                    "count=estimated" => prefs.count = CountOption::Estimated,
                    "resolution=merge-duplicates" => {
                        prefs.resolution = Some(ConflictAction::MergeDuplicates)
                    }
                    "resolution=ignore-duplicates" => {
                        prefs.resolution = Some(ConflictAction::IgnoreDuplicates)
                    }
                    "handling=strict" => prefs.handling = HandlingMode::Strict,
                    "handling=lenient" => prefs.handling = HandlingMode::Lenient,
                    _ => {}
                }
            }
        }
    }

    prefs
}

// ---------------------------------------------------------------------------
// Range header
// ---------------------------------------------------------------------------

pub fn parse_range(headers: &HeaderMap) -> (Option<i64>, Option<i64>) {
    let s = match headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        Some(s) => s,
        None => return (None, None),
    };
    let (start_s, end_s) = match s.split_once('-') {
        Some(pair) => pair,
        None => return (None, None),
    };
    let start: i64 = match start_s.parse() {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    let end: i64 = match end_s.parse() {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    (Some(end - start + 1), Some(start))
}

// ---------------------------------------------------------------------------
// Parse filters from query params
// ---------------------------------------------------------------------------

pub fn extract_filters(params: &HashMap<String, String>) -> Result<FilterNode, HandlerError> {
    let mut nodes: Vec<FilterNode> = Vec::new();

    for (key, value) in params {
        match key.as_str() {
            "or" | "and" => {
                nodes.push(parse_logic_filter(key, value)?);
            }
            k if RESERVED_PARAMS.contains(&k) => continue,
            column => {
                nodes.push(FilterNode::Condition(parse_filter(column, value)?));
            }
        }
    }

    Ok(FilterNode::And(nodes))
}

/// Like extract_filters but works with Vec<(String,String)> to support duplicate keys.
pub fn extract_filters_multi(params: &[(String, String)]) -> Result<FilterNode, HandlerError> {
    let mut nodes: Vec<FilterNode> = Vec::new();
    for (key, value) in params {
        match key.as_str() {
            "or" | "and" => {
                nodes.push(parse_logic_filter(key, value)?);
            }
            k if RESERVED_PARAMS.contains(&k) => continue,
            column => {
                nodes.push(FilterNode::Condition(parse_filter(column, value)?));
            }
        }
    }
    Ok(FilterNode::And(nodes))
}

// ---------------------------------------------------------------------------
// Query string parsing
// ---------------------------------------------------------------------------

/// Parse raw query string into (key, value) pairs, preserving duplicates.
pub fn parse_query_pairs(query: &str) -> Vec<(String, String)> {
    if query.is_empty() {
        return Vec::new();
    }
    query
        .split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((urlencoding_decode(k), urlencoding_decode(v)))
        })
        .collect()
}

fn urlencoding_decode(s: &str) -> String {
    let mut bytes = Vec::with_capacity(s.len());
    let mut iter = s.bytes();
    while let Some(b) = iter.next() {
        match b {
            b'%' => {
                let hi = iter.next().and_then(hex_val);
                let lo = iter.next().and_then(hex_val);
                if let (Some(h), Some(l)) = (hi, lo) {
                    bytes.push(h << 4 | l);
                }
            }
            b'+' => bytes.push(b' '),
            _ => bytes.push(b),
        }
    }
    String::from_utf8(bytes).unwrap_or_default()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Schema resolution via Accept-Profile / Content-Profile
// ---------------------------------------------------------------------------

/// Parse `Accept-Profile` (reads) or `Content-Profile` (writes) header
/// to select a specific schema.
pub fn resolve_schemas<'a>(
    headers: &HeaderMap,
    config_schemas: &'a [String],
) -> Result<&'a [String], HandlerError> {
    let profile = headers
        .get("accept-profile")
        .or_else(|| headers.get("content-profile"))
        .and_then(|v| v.to_str().ok());

    if let Some(profile) = profile {
        if config_schemas.iter().any(|s| s == profile) {
            Ok(config_schemas)
        } else {
            Err(HandlerError::BadRequest(format!(
                "schema '{profile}' is not in the configured search path"
            )))
        }
    } else {
        Ok(config_schemas)
    }
}

// ---------------------------------------------------------------------------
// Singular response detection
// ---------------------------------------------------------------------------

/// Check if the Accept header requests a singular (single-object) response.
pub fn wants_singular(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("application/vnd.pgrst.object+json"))
        .unwrap_or(false)
}

/// Unwrap a JSON array to a single object for singular responses.
pub fn to_singular(json_body: &str) -> Result<String, HandlerError> {
    let arr: Vec<serde_json::Value> = serde_json::from_str(json_body)
        .map_err(|_| HandlerError::NotAcceptable("invalid JSON array".into()))?;
    match arr.len() {
        0 => Err(HandlerError::NotAcceptable(
            "no rows returned for singular response".into(),
        )),
        1 => Ok(arr.into_iter().next().unwrap().to_string()),
        n => Err(HandlerError::NotAcceptable(format!(
            "expected single row but got {n} rows"
        ))),
    }
}

// ---------------------------------------------------------------------------
// EXPLAIN plan detection
// ---------------------------------------------------------------------------

/// Check if the Accept header requests a query plan.
pub fn wants_explain(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("application/vnd.pgrst.plan+json"))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// SETUP SQL builder
// ---------------------------------------------------------------------------

/// Build the `BEGIN; SET LOCAL ROLE ...` setup SQL for a request. Authenticated
/// requests also seed `request.jwt.claims` so RLS policies that read the JWT see
/// the same claim set the JWT was issued with.
pub fn build_setup_sql(claims: &Option<JwtClaims>, anon_setup_sql: &str) -> String {
    let Some(claims) = claims else {
        return anon_setup_sql.to_string();
    };
    let quoted_role = format!("\"{}\"", claims.role.replace('"', "\"\""));
    let escaped = claims.raw.replace('\'', "''");
    format!(
        "BEGIN; SET LOCAL ROLE {quoted_role}; \
         SELECT set_config('request.jwt.claims', '{escaped}', true)"
    )
}

// ---------------------------------------------------------------------------
// Body → rows
// ---------------------------------------------------------------------------

pub fn body_to_rows(
    body: serde_json::Value,
) -> Result<Vec<serde_json::Map<String, serde_json::Value>>, HandlerError> {
    match body {
        serde_json::Value::Array(arr) => arr
            .into_iter()
            .map(|v| match v {
                serde_json::Value::Object(m) => Ok(m),
                _ => Err(HandlerError::BadRequest("expected array of objects".into())),
            })
            .collect(),
        serde_json::Value::Object(m) => Ok(vec![m]),
        _ => Err(HandlerError::BadRequest(
            "expected JSON object or array".into(),
        )),
    }
}

// ---------------------------------------------------------------------------
// CSV conversion
// ---------------------------------------------------------------------------

/// Convert a JSON array of objects to CSV format.
pub fn json_array_to_csv(json_str: &str) -> String {
    let arr: Vec<serde_json::Map<String, serde_json::Value>> = match serde_json::from_str(json_str)
    {
        Ok(a) => a,
        Err(_) => return String::new(),
    };

    if arr.is_empty() {
        return String::new();
    }

    // Collect all column names from the first row (preserving order).
    let columns: Vec<&String> = arr[0].keys().collect();

    let mut out = String::new();

    for (i, col) in columns.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&csv_escape(col));
    }
    out.push('\n');

    for row in &arr {
        for (i, col) in columns.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            match row.get(*col) {
                Some(serde_json::Value::Null) | None => {}
                Some(serde_json::Value::String(s)) => out.push_str(&csv_escape(s)),
                Some(v) => out.push_str(&v.to_string()),
            }
        }
        out.push('\n');
    }

    out
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// Hashing (FNV-1a for ETags)
// ---------------------------------------------------------------------------

/// Simple FNV-1a hash for ETag generation (not cryptographic).
pub fn simple_hash(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in s.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

// ---------------------------------------------------------------------------
// Lightweight JSON array element counting
// ---------------------------------------------------------------------------

/// Quickly count top-level elements of a JSON array string without full parsing.
/// Counts commas at depth 1 (inside the outer array brackets).
pub fn count_json_array(s: &str) -> usize {
    let s = s.trim();
    if s.len() < 2 || s == "[]" {
        return 0;
    }
    let mut depth = 0i32;
    let mut count = 1usize;
    let mut in_string = false;
    let mut prev = 0u8;
    for &b in s.as_bytes() {
        if in_string {
            if b == b'"' && prev != b'\\' {
                in_string = false;
            }
        } else {
            match b {
                b'"' => in_string = true,
                b'[' | b'{' => depth += 1,
                b']' | b'}' => depth -= 1,
                b',' if depth == 1 => count += 1,
                _ => {}
            }
        }
        prev = b;
    }
    count
}

// ---------------------------------------------------------------------------
// Handler error type
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum HandlerError {
    BadRequest(String),
    NotAcceptable(String),
    Parse(pg_query_engine::ParseError),
}

impl std::fmt::Display for HandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest(msg) => write!(f, "bad request: {msg}"),
            Self::NotAcceptable(msg) => write!(f, "not acceptable: {msg}"),
            Self::Parse(e) => write!(f, "parse error: {e}"),
        }
    }
}

impl std::error::Error for HandlerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse(e) => Some(e),
            _ => None,
        }
    }
}

impl From<pg_query_engine::ParseError> for HandlerError {
    fn from(e: pg_query_engine::ParseError) -> Self {
        Self::Parse(e)
    }
}

// ---------------------------------------------------------------------------
// Route handlers (generic over any Backend)
// ---------------------------------------------------------------------------

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Json, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use pg_query_engine::{
    build_count_sql, build_sql, parse_order, parse_select, ApiRequest, DeleteRequest, FunctionCall,
    InsertRequest, ReadRequest, SelectItem, UpdateRequest,
};
use pg_schema_cache::{ReturnType, SchemaCache};

use crate::auth::extract_jwt_claims_for_state;
use crate::backend::Backend;
use crate::error::ApiError;
use crate::state::AppState;

pub async fn handle_read<B: Backend>(
    State(state): State<Arc<AppState<B>>>,
    Path(table): Path<String>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let params = parse_query_pairs(raw_query.as_deref().unwrap_or(""));
    let claims = extract_jwt_claims_for_state::<B>(&headers, &state)?;
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

    if wants_explain(&headers) {
        sql.sql = format!("EXPLAIN (FORMAT JSON) {}", sql.sql);
        let plan_json = state
            .backend
            .exec_query(&claims, &state.anon_setup_sql, &sql)
            .await?;
        let plan_text = plan_json.unwrap_or_else(|| "[]".to_string());
        let plan: serde_json::Value =
            serde_json::from_str(&plan_text).unwrap_or(serde_json::json!([]));
        return Ok((
            StatusCode::OK,
            [("content-type", "application/vnd.pgrst.plan+json")],
            plan.to_string(),
        )
            .into_response());
    }

    let count_sql = if prefs.count == CountOption::Exact {
        Some(build_count_sql(&cache, &read_req, schemas)?)
    } else {
        None
    };

    let (json, total) = state
        .backend
        .exec_query_with_count(&claims, &state.anon_setup_sql, &sql, count_sql.as_ref())
        .await?;

    let body = json.unwrap_or_else(|| "[]".to_string());

    if std::env::var("PG_REST_LEAN").is_ok() {
        return Ok((StatusCode::OK, [("content-type", "application/json")], body).into_response());
    }

    let etag = format!("\"{}\"", simple_hash(&body));
    if let Some(if_none_match) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        if if_none_match == etag || if_none_match == "*" {
            return Ok(StatusCode::NOT_MODIFIED.into_response());
        }
    }

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

    if singular {
        let obj = to_singular(&body)?;
        return Ok((
            StatusCode::OK,
            [
                ("content-type", "application/vnd.pgrst.object+json"),
                ("content-range", &content_range),
            ],
            obj,
        )
            .into_response());
    }

    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json");

    let (content_type, response_body) = if accept.contains("text/csv") {
        ("text/csv", json_array_to_csv(&body))
    } else {
        ("application/json", body)
    };

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
            ("content-type", content_type),
            ("content-range", &content_range),
            ("etag", &etag),
        ],
        response_body,
    )
        .into_response())
}

pub async fn handle_insert<B: Backend>(
    State(state): State<Arc<AppState<B>>>,
    Path(table): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Response, ApiError> {
    let claims = extract_jwt_claims_for_state::<B>(&headers, &state)?;
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
    let json = state
        .backend
        .exec_query(&claims, &state.anon_setup_sql, &sql)
        .await?;

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
            [("content-type", "application/json")],
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

pub async fn handle_update<B: Backend>(
    State(state): State<Arc<AppState<B>>>,
    Path(table): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Response, ApiError> {
    let claims = extract_jwt_claims_for_state::<B>(&headers, &state)?;
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
    let json = state
        .backend
        .exec_query(&claims, &state.anon_setup_sql, &sql)
        .await?;

    match json {
        Some(j) => Ok((StatusCode::OK, [("content-type", "application/json")], j).into_response()),
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

pub async fn handle_delete<B: Backend>(
    State(state): State<Arc<AppState<B>>>,
    Path(table): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let claims = extract_jwt_claims_for_state::<B>(&headers, &state)?;
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
    let json = state
        .backend
        .exec_query(&claims, &state.anon_setup_sql, &sql)
        .await?;

    match json {
        Some(j) => Ok((StatusCode::OK, [("content-type", "application/json")], j).into_response()),
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

pub async fn handle_rpc<B: Backend>(
    State(state): State<Arc<AppState<B>>>,
    Path(function): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Option<Json<serde_json::Value>>,
) -> Result<Response, ApiError> {
    let claims = extract_jwt_claims_for_state::<B>(&headers, &state)?;
    let cache: Arc<SchemaCache> = state.schema_cache.borrow().clone();
    let schemas = &state.config.database.schemas;

    let func = cache
        .find_function(&function, schemas)
        .ok_or_else(|| ApiError::FunctionNotFound(function.clone()))?;

    let func_qn = func.name.clone();
    let is_scalar = matches!(func.return_type, ReturnType::Scalar(_) | ReturnType::Void);

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
    let json = state
        .backend
        .exec_query(&claims, &state.anon_setup_sql, &sql)
        .await?;

    if matches!(func.return_type, ReturnType::Void) {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }

    Ok((
        StatusCode::OK,
        [("content-type", "application/json")],
        json.unwrap_or_else(|| "null".to_string()),
    )
        .into_response())
}

pub async fn handle_root<B: Backend>(
    State(state): State<Arc<AppState<B>>>,
    Query(params): Query<HashMap<String, String>>,
) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    let cached = state.openapi_cache.read().await;
    let spec = match params.get("openapi-version").map(String::as_str) {
        Some("3") | Some("3.0") => cached.1.clone(),
        _ => cached.0.clone(),
    };
    drop(cached);

    (
        StatusCode::OK,
        [("content-type", "application/openapi+json")],
        spec,
    )
}

pub async fn handle_live() -> StatusCode {
    StatusCode::OK
}

pub async fn handle_ready<B: Backend>(State(state): State<Arc<AppState<B>>>) -> StatusCode {
    state.backend.check_health().await
}

pub async fn handle_metrics<B: Backend>(
    State(state): State<Arc<AppState<B>>>,
) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    let cache = state.schema_cache.borrow().clone();
    let body = state.backend.format_metrics(&cache).await;
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4")],
        body,
    )
}

// ---------------------------------------------------------------------------
// Schema reload (generic)
// ---------------------------------------------------------------------------

pub async fn handle_reload<B: Backend>(
    State(state): State<Arc<AppState<B>>>,
) -> Result<Response, ApiError> {
    let cache = state.backend.build_schema_cache(&state.config).await?;

    let tables = cache.tables.len();
    let functions = cache.functions.len();
    state.schema_cache_tx.send(Arc::new(cache)).ok();

    let specs = state.rebuild_openapi_cache();
    *state.openapi_cache.write().await = specs;

    Ok((
        StatusCode::OK,
        [("content-type", "application/json")],
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
// WebSocket NOTIFY forwarding (generic)
// ---------------------------------------------------------------------------

pub async fn handle_ws<B: Backend>(
    State(state): State<Arc<AppState<B>>>,
    Query(params): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    let channel = params
        .get("channel")
        .cloned()
        .unwrap_or_else(|| "pgrst".to_string());
    let uri = state.config.database.uri.clone();
    let state = Arc::clone(&state);

    ws.on_upgrade(move |mut socket| async move {
        match state.backend.spawn_listener(&uri, &channel).await {
            Ok(rx) => ws_handler_generic(socket, rx).await,
            Err(e) => {
                let _ = socket
                    .send(Message::Text(
                        serde_json::json!({"error": e.to_string()})
                            .to_string()
                            .into(),
                    ))
                    .await;
            }
        }
    })
}

async fn ws_handler_generic(
    mut socket: WebSocket,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<(String, String)>,
) {
    loop {
        tokio::select! {
            Some((ch, payload)) = rx.recv() => {
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
