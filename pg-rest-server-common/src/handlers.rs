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
