use std::sync::Arc;

use tokio::sync::watch;

use pg_schema_cache::SchemaCache;

use crate::auth::JwtCache;
use crate::backend::Backend;
use crate::config::AppConfig;

pub struct AppState<B: Backend> {
    pub backend: B,
    pub schema_cache: watch::Receiver<Arc<SchemaCache>>,
    pub schema_cache_tx: watch::Sender<Arc<SchemaCache>>,
    pub openapi_cache: tokio::sync::RwLock<(String, String)>,
    pub config: AppConfig,
    pub jwt_decoding_key: jsonwebtoken::DecodingKey,
    pub jwt_validation: jsonwebtoken::Validation,
    pub jwt_cache: JwtCache,
    pub anon_role_quoted: String,
    pub anon_setup_sql: String,
}

impl<B: Backend> AppState<B> {
    pub fn new(
        backend: B,
        config: AppConfig,
        schema_cache: watch::Receiver<Arc<SchemaCache>>,
        schema_cache_tx: watch::Sender<Arc<SchemaCache>>,
    ) -> Self {
        let anon_role_quoted = format!("\"{}\"", config.database.anon_role.replace('"', "\"\""));
        let anon_setup_sql = format!("BEGIN; SET LOCAL ROLE {anon_role_quoted}");
        let jwt_decoding_key = jsonwebtoken::DecodingKey::from_secret(config.jwt.secret.as_bytes());
        let jwt_validation = {
            let mut v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
            v.required_spec_claims = Default::default();
            v
        };
        Self {
            backend,
            schema_cache,
            schema_cache_tx,
            openapi_cache: tokio::sync::RwLock::new(("".into(), "".into())),
            anon_role_quoted,
            anon_setup_sql,
            config,
            jwt_decoding_key,
            jwt_validation,
            jwt_cache: JwtCache::new(),
        }
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
