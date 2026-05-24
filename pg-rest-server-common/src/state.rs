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
    pub fn rebuild_openapi_cache(&self) -> (String, String) {
        let cache = self.schema_cache.borrow().clone();
        let v2 = crate::openapi::generate_v2(&cache, &self.config).to_string();
        let v3 = crate::openapi::generate_v3(&cache, &self.config).to_string();
        (v2, v3)
    }
}
