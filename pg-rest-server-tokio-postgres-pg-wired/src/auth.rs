use axum::http::{header, HeaderMap};

use pg_rest_server_common::auth::JwtClaims;

use crate::error::ApiError;
use crate::state::AppState;

/// Extract JWT claims from the Authorization header.
/// Uses a cache to skip HMAC validation for repeated tokens.
/// Returns `Ok(None)` for anonymous requests (no token).
pub fn extract_jwt_claims(
    headers: &HeaderMap,
    state: &AppState,
) -> Result<Option<JwtClaims>, ApiError> {
    let auth_value = match headers.get(header::AUTHORIZATION) {
        Some(v) => v,
        None => return Ok(None),
    };

    let auth_str = auth_value
        .to_str()
        .map_err(|_| ApiError::Unauthorized("invalid authorization header".into()))?;

    let token = auth_str
        .strip_prefix("Bearer ")
        .ok_or_else(|| ApiError::Unauthorized("expected Bearer token".into()))?;

    if let Some(claims) = state.jwt_cache.get(token) {
        return Ok(Some(claims));
    }

    let data = jsonwebtoken::decode::<serde_json::Value>(
        token,
        &state.jwt_decoding_key,
        &state.jwt_validation,
    )
    .map_err(|e| ApiError::Unauthorized(e.to_string()))?;

    let role = data
        .claims
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or(&state.config.database.anon_role)
        .to_string();

    let raw = serde_json::to_string(&data.claims).unwrap_or_default();

    let claims = JwtClaims { role, raw };
    state.jwt_cache.insert(token, claims.clone());
    Ok(Some(claims))
}
