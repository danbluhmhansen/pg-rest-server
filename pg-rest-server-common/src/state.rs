use std::sync::Arc;

use tokio::sync::watch;

use pg_schema_cache::SchemaCache;

use crate::auth::{JwtCache, JwtKeySource};
use crate::backend::Backend;
use crate::config::AppConfig;

pub struct AppState<B: Backend> {
    pub backend: B,
    pub schema_cache: watch::Receiver<Arc<SchemaCache>>,
    pub schema_cache_tx: watch::Sender<Arc<SchemaCache>>,
    pub openapi_cache: tokio::sync::RwLock<(String, String)>,
    pub config: AppConfig,
    pub jwt_key_source: JwtKeySource,
    pub jwt_cache: JwtCache,
    pub anon_role_quoted: String,
    pub anon_setup_sql: String,
}

impl<B: Backend> AppState<B> {
    pub async fn new(
        backend: B,
        config: AppConfig,
        schema_cache: watch::Receiver<Arc<SchemaCache>>,
        schema_cache_tx: watch::Sender<Arc<SchemaCache>>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let anon_role_quoted = format!("\"{}\"", config.database.anon_role.replace('"', "\"\""));
        let anon_setup_sql = format!("BEGIN; SET LOCAL ROLE {anon_role_quoted}");

        let jwt_key_source = resolve_jwt_config(&config.jwt).await?;

        Ok(Self {
            backend,
            schema_cache,
            schema_cache_tx,
            openapi_cache: tokio::sync::RwLock::new(("".into(), "".into())),
            anon_role_quoted,
            anon_setup_sql,
            config,
            jwt_key_source,
            jwt_cache: JwtCache::new(),
        })
    }

    pub async fn init_openapi_cache(&self) {
        let specs = self.rebuild_openapi_cache();
        *self.openapi_cache.write().await = specs;
    }

    pub fn rebuild_openapi_cache(&self) -> (String, String) {
        let cache = self.schema_cache.borrow().clone();
        let v2 = crate::openapi::generate_v2(&cache, &self.config).to_string();
        let v3 = crate::openapi::generate_v3(&cache, &self.config).to_string();
        (v2, v3)
    }
}

/// Resolve the [`JwtConfig`] into a [`JwtKeySource`].
///
/// Auto-detection rules for `config.jwt.secret`:
/// 1. If it starts with `http://` or `https://` → treat as JWKS URL (fetch at startup).
/// 2. If it parses as a JSON object → treat as inline JWKS.
/// 3. Otherwise → treat as HMAC secret.
///
/// If `config.jwt.jwks_url` is set (and `secret` is absent), it is used directly as a JWKS URL.
async fn resolve_jwt_config(
    config: &crate::config::JwtConfig,
) -> Result<JwtKeySource, Box<dyn std::error::Error>> {
    match (&config.secret, &config.jwks_url) {
        (Some(_), Some(_)) => Err("jwt.secret and jwt.jwks_url are mutually exclusive".into()),
        (None, None) => Err("either jwt.secret or jwt.jwks_url must be configured".into()),
        (None, Some(url)) => JwtKeySource::from_jwks_url(url).await,
        (Some(val), None) => {
            let trimmed = val.trim();
            // URL → fetch JWKS
            if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
                return JwtKeySource::from_jwks_url(trimmed).await;
            }
            // JSON → inline JWKS
            if trimmed.starts_with('{') {
                match JwtKeySource::from_jwks_json(trimmed) {
                    Ok(source) => return Ok(source),
                    Err(_) => {
                        // Parsing as JWKS failed; fall through to treat as HMAC secret
                    }
                }
            }
            // HMAC secret (default)
            Ok(JwtKeySource::from_secret(val))
        }
    }
}
