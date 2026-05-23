use axum::http::HeaderMap;

use pg_rest_server_common::auth::{extract_jwt_claims as common_extract, JwtClaims};

use crate::state::AppState;
use crate::ApiError;

/// Extract JWT claims from the Authorization header.
/// Delegates to the shared implementation in `pg-rest-server-common`.
pub fn extract_jwt_claims(
    headers: &HeaderMap,
    state: &AppState,
) -> Result<Option<JwtClaims>, ApiError> {
    common_extract(
        headers,
        &state.jwt_cache,
        &state.jwt_decoding_key,
        &state.jwt_validation,
        &state.config.database.anon_role,
    )
    .map_err(|e| ApiError::Unauthorized(e.to_string()))
}
