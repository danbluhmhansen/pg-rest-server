use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use axum::http::{header, HeaderMap};

#[derive(Debug, Clone)]
pub struct JwtClaims {
    pub role: String,
    /// Raw JSON string of all claims, forwarded to PostgreSQL as a GUC.
    pub raw: String,
    /// Unix epoch seconds when these claims expire (None = no expiry).
    pub exp: Option<i64>,
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

    /// Retrieve cached claims. Expired entries are removed and treated as a miss.
    pub fn get(&self, token: &str) -> Option<JwtClaims> {
        let key = Self::hash_token(token);
        let mut cache = self.entries.lock().unwrap();
        if let Some(claims) = cache.get(&key) {
            if let Some(exp) = claims.exp {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                if exp <= now {
                    cache.remove(&key);
                    return None;
                }
            }
            return Some(claims.clone());
        }
        None
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
pub enum AuthSource {
    /// Single HMAC secret (HS256/HS384/HS512 symmetric key).
    Secret {
        decoding_key: jsonwebtoken::DecodingKey,
        validation: Box<jsonwebtoken::Validation>,
    },
    /// JWKS key set (supports RSA, EC, EdDSA, and symmetric keys).
    Jwks { keys: Vec<jsonwebtoken::jwk::Jwk> },
    /// OIDC Token Introspection (RFC 7662). Delegates validation to an
    /// external authorization server via HTTP.
    Introspection {
        client: IntrospectionClient,
        /// Fallback cache TTL (seconds) when the introspection response
        /// does not include an `exp` claim.
        cache_ttl: u64,
    },
}

// ---------------------------------------------------------------------------
// OIDC Token Introspection (RFC 7662) client
// ---------------------------------------------------------------------------

/// HTTP client for RFC 7662 Token Introspection.
///
/// Sends the token to the configured endpoint and parses the response.
/// The endpoint must support Basic auth with the configured client
/// credentials.
pub struct IntrospectionClient {
    url: String,
    client_id: String,
    client_secret: String,
    http: reqwest::Client,
}

impl fmt::Debug for IntrospectionClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IntrospectionClient")
            .field("url", &self.url)
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

impl IntrospectionClient {
    pub fn new(url: &str, client_id: &str, client_secret: &str) -> Self {
        Self {
            url: url.to_string(),
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            http: reqwest::Client::new(),
        }
    }

    /// POST the token to the introspection endpoint.
    ///
    /// Returns the full JSON response on success (regardless of `active`
    /// status). The caller is responsible for checking the `active` field.
    pub async fn introspect(&self, token: &str) -> Result<serde_json::Value, AuthError> {
        let auth_bytes = format!("{}:{}", self.client_id, self.client_secret);
        let auth_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            auth_bytes.as_bytes(),
        );

        let params = [("token", token), ("token_type_hint", "access_token")];

        let resp = self
            .http
            .post(&self.url)
            .header("Authorization", format!("Basic {auth_b64}"))
            .form(&params)
            .send()
            .await
            .map_err(|e| AuthError::Unauthorized(format!("introspection request failed: {e}")))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| AuthError::Unauthorized(format!("introspection read failed: {e}")))?;

        if !status.is_success() {
            return Err(AuthError::Unauthorized(format!(
                "introspection endpoint returned HTTP {status}: {body}"
            )));
        }

        serde_json::from_str(&body)
            .map_err(|e| AuthError::Unauthorized(format!("invalid introspection JSON: {e}")))
    }
}

impl AuthSource {
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

impl fmt::Debug for AuthSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Secret { .. } => f.debug_struct("Secret").finish_non_exhaustive(),
            Self::Jwks { keys } => f
                .debug_struct("Jwks")
                .field("key_count", &keys.len())
                .finish_non_exhaustive(),
            Self::Introspection { client, cache_ttl } => f
                .debug_struct("Introspection")
                .field("client", client)
                .field("cache_ttl", cache_ttl)
                .finish(),
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

/// Shared extraction logic after JWT decode — used by both `Secret` and `Jwks`
/// paths.
fn decode_and_extract(
    token: &str,
    decoding_key: &jsonwebtoken::DecodingKey,
    validation: &jsonwebtoken::Validation,
    anon_role: &str,
) -> Result<JwtClaims, AuthError> {
    let data = jsonwebtoken::decode::<serde_json::Value>(token, decoding_key, validation)
        .map_err(|e| AuthError::Unauthorized(e.to_string()))?;

    let role = data
        .claims
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or(anon_role)
        .to_string();

    let raw = serde_json::to_string(&data.claims).unwrap_or_default();

    let exp = data.claims.get("exp").and_then(|v| v.as_i64());

    Ok(JwtClaims { role, raw, exp })
}

/// Extract claims from an introspection response.
fn parse_introspect_response(
    body: &serde_json::Value,
    anon_role: &str,
) -> Result<Option<JwtClaims>, AuthError> {
    let active = body
        .get("active")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if !active {
        return Ok(None);
    }

    let role = body
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or(anon_role)
        .to_string();

    let raw = serde_json::to_string(body).unwrap_or_default();
    let exp = body.get("exp").and_then(|v| v.as_i64());

    Ok(Some(JwtClaims { role, raw, exp }))
}

/// Extract JWT claims from the Authorization header.
/// Uses a cache to skip repeated token validation.
/// Returns `Ok(None)` for anonymous requests (no token).
pub async fn extract_jwt_claims(
    headers: &HeaderMap,
    jwt_cache: &JwtCache,
    auth_source: &AuthSource,
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

    let claims = match auth_source {
        AuthSource::Secret {
            decoding_key,
            validation,
        } => decode_and_extract(token, decoding_key, validation, anon_role)?,

        AuthSource::Jwks { keys } => {
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

            decode_and_extract(token, &decoding_key, &validation, anon_role)?
        }

        AuthSource::Introspection { client, cache_ttl } => {
            let body = client.introspect(token).await?;
            let mut claims = match parse_introspect_response(&body, anon_role)? {
                Some(c) => c,
                None => return Ok(None),
            };
            if claims.exp.is_none() {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                claims.exp = Some(now + *cache_ttl as i64);
            }
            claims
        }
    };

    jwt_cache.insert(token, claims.clone());
    Ok(Some(claims))
}

/// Convenience wrapper: extract JWT claims from an `AppState` for any backend.
pub async fn extract_jwt_claims_for_state<B: crate::backend::Backend>(
    headers: &HeaderMap,
    state: &crate::state::AppState<B>,
) -> Result<Option<JwtClaims>, crate::error::ApiError> {
    extract_jwt_claims(
        headers,
        &state.jwt_cache,
        &state.auth_source,
        &state.config.database.anon_role,
    )
    .await
    .map_err(|e| crate::error::ApiError::Unauthorized(e.to_string()))
}
