use std::sync::Arc;

use tokio::sync::watch;

use pg_schema_cache::SchemaCache;

use crate::auth::{AuthSource, IntrospectionClient, JwtCache};
use crate::backend::Backend;
use crate::config::AppConfig;

pub struct AppState<B: Backend> {
    pub backend: B,
    pub schema_cache: watch::Receiver<Arc<SchemaCache>>,
    pub schema_cache_tx: watch::Sender<Arc<SchemaCache>>,
    pub openapi_cache: tokio::sync::RwLock<(String, String)>,
    pub config: AppConfig,
    pub auth_source: AuthSource,
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

        let auth_source = resolve_auth_source(&config.auth).await?;

        Ok(Self {
            backend,
            schema_cache,
            schema_cache_tx,
            openapi_cache: tokio::sync::RwLock::new(("".into(), "".into())),
            anon_role_quoted,
            anon_setup_sql,
            config,
            auth_source,
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

/// Resolve [`AuthConfig`] into a [`AuthSource`].
///
/// Auto-detection rules for `Secret` variant:
/// 1. Value starts with `http://` or `https://` → fetch JWKS from URL.
/// 2. Value starts with `{` → inline JWKS JSON.
/// 3. Otherwise → HMAC secret.
async fn resolve_auth_source(
    config: &crate::config::AuthConfig,
) -> Result<AuthSource, Box<dyn std::error::Error>> {
    match config {
        crate::config::AuthConfig::Introspection(inner) => {
            Ok(AuthSource::Introspection {
                client: IntrospectionClient::new(
                    &inner.introspection.url,
                    &inner.introspection.client_id,
                    &inner.introspection.client_secret,
                ),
                cache_ttl: inner.introspection.cache_ttl.unwrap_or(30),
            })
        }

        crate::config::AuthConfig::JwksUrl(inner) => {
            AuthSource::from_jwks_url(&inner.jwks_url).await
        }

        crate::config::AuthConfig::Secret(inner) => {
            let trimmed = inner.secret.trim();
            if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
                return AuthSource::from_jwks_url(trimmed).await;
            }
            if trimmed.starts_with('{') {
                if let Ok(source) = AuthSource::from_jwks_json(trimmed) {
                    return Ok(source);
                }
            }
            Ok(AuthSource::from_secret(trimmed))
        }
    }
}
