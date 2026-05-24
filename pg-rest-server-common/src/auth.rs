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
// JWT key source: how the server resolves keys for verification
// ---------------------------------------------------------------------------

/// Determines how JWT verification keys are obtained.
pub enum JwtKeySource {
    /// Single HMAC secret (HS256/HS384/HS512 symmetric key).
    Secret {
        decoding_key: jsonwebtoken::DecodingKey,
        validation: Box<jsonwebtoken::Validation>,
    },
    /// JWKS key set (supports RSA, EC, EdDSA, and symmetric keys).
    Jwks { keys: Vec<jsonwebtoken::jwk::Jwk> },
}

impl JwtKeySource {
    /// Create a `Secret` variant from an HMAC secret string.
    /// Defaults to HS256 with no required spec claims.
    pub fn from_secret(secret: &str) -> Self {
        let decoding_key = jsonwebtoken::DecodingKey::from_secret(secret.as_bytes());
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
        validation.required_spec_claims = Default::default();
        Self::Secret {
            decoding_key,
            validation: Box::new(validation),
        }
    }

    /// Parse inline JWKS JSON (`{"keys": [...]}`) into a `Jwks` variant.
    pub fn from_jwks_json(jwks_json: &str) -> Result<Self, Box<dyn std::error::Error>> {
        #[derive(serde::Deserialize)]
        struct JwksSet {
            keys: Vec<jsonwebtoken::jwk::Jwk>,
        }
        let set: JwksSet = serde_json::from_str(jwks_json)?;
        if set.keys.is_empty() {
            return Err("JWKS must contain at least one key".into());
        }
        Ok(Self::Jwks { keys: set.keys })
    }

    /// Fetch a JWKS from a URL and parse it into a `Jwks` variant.
    pub async fn from_jwks_url(url: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let resp = reqwest::get(url).await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("JWKS URL returned {status}").into());
        }
        let body = resp.text().await?;
        Self::from_jwks_json(&body)
    }
}

impl fmt::Debug for JwtKeySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Secret { .. } => f.debug_struct("Secret").finish_non_exhaustive(),
            Self::Jwks { keys } => f
                .debug_struct("Jwks")
                .field("key_count", &keys.len())
                .finish_non_exhaustive(),
        }
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

/// Determine the [`jsonwebtoken::Algorithm`] from a JWK.
///
/// Prefers the JWK's `key_algorithm` field. Falls back to inferring from the
/// key type:
/// - RSA → RS256
/// - EC → ES256
/// - oct (symmetric) → HS256
/// - OKP → EdDSA
fn algorithm_from_jwk(jwk: &jsonwebtoken::jwk::Jwk) -> Result<jsonwebtoken::Algorithm, AuthError> {
    use jsonwebtoken::jwk::AlgorithmParameters;

    if let Some(ref key_alg) = jwk.common.key_algorithm {
        let alg_str = key_alg.to_string();
        return alg_str
            .parse::<jsonwebtoken::Algorithm>()
            .map_err(|_| AuthError::Unauthorized(format!("unsupported key algorithm: {alg_str}")));
    }
    match jwk.algorithm {
        AlgorithmParameters::RSA(_) => Ok(jsonwebtoken::Algorithm::RS256),
        AlgorithmParameters::EllipticCurve(_) => Ok(jsonwebtoken::Algorithm::ES256),
        AlgorithmParameters::OctetKey(_) => Ok(jsonwebtoken::Algorithm::HS256),
        AlgorithmParameters::OctetKeyPair(_) => Ok(jsonwebtoken::Algorithm::EdDSA),
    }
}

/// Extract JWT claims from the Authorization header.
/// Uses a cache to skip HMAC validation for repeated tokens.
/// Returns `Ok(None)` for anonymous requests (no token).
pub fn extract_jwt_claims(
    headers: &HeaderMap,
    jwt_cache: &JwtCache,
    jwt_key_source: &JwtKeySource,
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

    let data = match jwt_key_source {
        JwtKeySource::Secret {
            decoding_key,
            validation,
        } => jsonwebtoken::decode::<serde_json::Value>(token, decoding_key, validation)
            .map_err(|e| AuthError::Unauthorized(e.to_string()))?,

        JwtKeySource::Jwks { keys } => {
            let header = jsonwebtoken::decode_header(token)
                .map_err(|e| AuthError::Unauthorized(e.to_string()))?;

            let kid = header
                .kid
                .as_ref()
                .ok_or_else(|| AuthError::Unauthorized("missing kid in JWT header".into()))?;

            let jwk = keys
                .iter()
                .find(|k| k.common.key_id.as_deref() == Some(kid))
                .ok_or_else(|| AuthError::Unauthorized(format!("no JWK found for kid: {kid}")))?;

            let decoding_key = jsonwebtoken::DecodingKey::from_jwk(jwk)
                .map_err(|e| AuthError::Unauthorized(e.to_string()))?;

            let alg = algorithm_from_jwk(jwk)?;
            let mut validation = jsonwebtoken::Validation::new(alg);
            validation.required_spec_claims = Default::default();

            jsonwebtoken::decode::<serde_json::Value>(token, &decoding_key, &validation)
                .map_err(|e| AuthError::Unauthorized(e.to_string()))?
        }
    };

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
        &state.jwt_key_source,
        &state.config.database.anon_role,
    )
    .map_err(|e| crate::error::ApiError::Unauthorized(e.to_string()))
}
