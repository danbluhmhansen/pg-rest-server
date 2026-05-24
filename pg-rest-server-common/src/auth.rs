use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use axum::http::{header, HeaderMap};

#[derive(Debug, Clone)]
pub struct JwtClaims {
    pub role: String,
    /// Raw JSON string of all claims, forwarded to PostgreSQL as a GUC.
    pub raw: String,
}

/// LRU-style JWT cache: token string → validated claims.
/// Avoids redundant HMAC-SHA256 for repeated tokens.
pub struct JwtCache {
    entries: Mutex<HashMap<u64, JwtClaims>>,
}

impl Default for JwtCache {
    fn default() -> Self {
        Self::new()
    }
}

impl JwtCache {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::with_capacity(256)),
        }
    }

    fn hash_token(token: &str) -> u64 {
        let mut hash: u64 = 0xcbf29ce484222325;
        for byte in token.bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash
    }

    pub fn get(&self, token: &str) -> Option<JwtClaims> {
        let key = Self::hash_token(token);
        let cache = self.entries.lock().unwrap();
        cache.get(&key).cloned()
    }

    pub fn insert(&self, token: &str, claims: JwtClaims) {
        let key = Self::hash_token(token);
        let mut cache = self.entries.lock().unwrap();
        if cache.len() >= 1024 {
            cache.clear();
        }
        cache.insert(key, claims);
    }
}

// ---------------------------------------------------------------------------
// JWT extraction
// ---------------------------------------------------------------------------

/// Errors that can occur during JWT extraction.
#[derive(Debug)]
pub enum AuthError {
    Unauthorized(String),
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthorized(msg) => write!(f, "unauthorized: {msg}"),
        }
    }
}

impl std::error::Error for AuthError {}

/// Extract JWT claims from the Authorization header.
/// Uses a cache to skip HMAC validation for repeated tokens.
/// Returns `Ok(None)` for anonymous requests (no token).
pub fn extract_jwt_claims(
    headers: &HeaderMap,
    jwt_cache: &JwtCache,
    jwt_decoding_key: &jsonwebtoken::DecodingKey,
    jwt_validation: &jsonwebtoken::Validation,
    anon_role: &str,
) -> Result<Option<JwtClaims>, AuthError> {
    let auth_value = match headers.get(header::AUTHORIZATION) {
        Some(v) => v,
        None => return Ok(None),
    };

    let auth_str = auth_value
        .to_str()
        .map_err(|_| AuthError::Unauthorized("invalid authorization header".into()))?;

    let token = auth_str
        .strip_prefix("Bearer ")
        .ok_or_else(|| AuthError::Unauthorized("expected Bearer token".into()))?;

    if let Some(claims) = jwt_cache.get(token) {
        return Ok(Some(claims));
    }

    let data = jsonwebtoken::decode::<serde_json::Value>(token, jwt_decoding_key, jwt_validation)
        .map_err(|e| AuthError::Unauthorized(e.to_string()))?;

    let role = data
        .claims
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or(anon_role)
        .to_string();

    let raw = serde_json::to_string(&data.claims).unwrap_or_default();

    let claims = JwtClaims { role, raw };
    jwt_cache.insert(token, claims.clone());
    Ok(Some(claims))
}

/// Convenience wrapper: extract JWT claims from an `AppState` for any backend.
pub fn extract_jwt_claims_for_state<B: crate::backend::Backend>(
    headers: &HeaderMap,
    state: &crate::state::AppState<B>,
) -> Result<Option<JwtClaims>, crate::error::ApiError> {
    extract_jwt_claims(
        headers,
        &state.jwt_cache,
        &state.jwt_decoding_key,
        &state.jwt_validation,
        &state.config.database.anon_role,
    )
    .map_err(|e| crate::error::ApiError::Unauthorized(e.to_string()))
}
