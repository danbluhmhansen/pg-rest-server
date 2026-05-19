use std::sync::Arc;

use tokio::sync::watch;

use crate::config::AppConfig;
use pg_schema_cache::SchemaCache;

pub struct AppState {
    /// Deadpool-postgres pool. The data path runs entirely on
    /// tokio-postgres; no `pg-wired` or `pg-pool`.
    pub pool: deadpool_postgres::Pool,
    pub schema_cache: watch::Receiver<Arc<SchemaCache>>,
    pub schema_cache_tx: watch::Sender<Arc<SchemaCache>>,
    /// Cached OpenAPI specs: (v2_json, v3_json). Regenerated on schema reload.
    pub openapi_cache: tokio::sync::RwLock<(String, String)>,
    pub config: AppConfig,
    pub jwt_decoding_key: jsonwebtoken::DecodingKey,
    pub jwt_validation: jsonwebtoken::Validation,
    pub jwt_cache: crate::auth::JwtCache,
    /// Pre-computed anon setup SQL: "BEGIN; SET LOCAL ROLE \"web_anon\""
    pub anon_setup_sql: String,
}

impl AppState {
    /// Regenerate the cached OpenAPI specs from the current schema cache.
    pub fn rebuild_openapi_cache(&self) -> (String, String) {
        let cache = self.schema_cache.borrow().clone();
        let v2 = crate::openapi::generate_v2(&cache, &self.config).to_string();
        let v3 = crate::openapi::generate_v3(&cache, &self.config).to_string();
        (v2, v3)
    }
}
