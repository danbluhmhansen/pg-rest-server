use std::sync::Arc;

use tokio::sync::watch;

use crate::config::AppConfig;
use pg_schema_cache::SchemaCache;
use resolute::SharedPool;

pub struct AppState {
    /// Multiplexed shared pool: many concurrent requests share each
    /// connection's writer task. Used for the request hot path so the
    /// per-request `BEGIN; SET LOCAL ROLE; ...; COMMIT` ships as a single
    /// pipelined batch instead of paying per-statement round-trips on a
    /// held connection.
    pub pool: Arc<SharedPool>,
    pub schema_cache: watch::Receiver<Arc<SchemaCache>>,
    pub schema_cache_tx: watch::Sender<Arc<SchemaCache>>,
    /// Cached OpenAPI specs: (v2_json, v3_json). Regenerated on schema reload.
    pub openapi_cache: tokio::sync::RwLock<(String, String)>,
    pub config: AppConfig,
    pub jwt_decoding_key: jsonwebtoken::DecodingKey,
    pub jwt_validation: jsonwebtoken::Validation,
    pub jwt_cache: crate::auth::JwtCache,
    /// Pre-quoted anon role identifier (e.g. `"web_anon"`) for SET LOCAL ROLE.
    pub anon_role_quoted: String,
    /// Pre-computed anon setup SQL: `BEGIN; SET LOCAL ROLE "web_anon"`.
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
