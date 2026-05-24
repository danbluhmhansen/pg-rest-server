use axum::http::StatusCode;
use pg_query_engine::SqlOutput;
use pg_schema_cache::SchemaCache;

use crate::auth::JwtClaims;
use crate::config::AppConfig;
use crate::error::ApiError;

/// Abstracts database execution across different backends.
// Only used via static dispatch (generics), not object-safe.
pub trait Backend: Send + Sync + 'static {
    /// Execute a single SQL statement with setup/role SQL, return optional JSON body.
    fn exec_query(
        &self,
        claims: &Option<JwtClaims>,
        anon_setup_sql: &str,
        sql: &SqlOutput,
    ) -> impl std::future::Future<Output = Result<Option<String>, ApiError>> + Send;

    /// Execute a data query and optionally a count query, returning (json, count).
    fn exec_query_with_count(
        &self,
        claims: &Option<JwtClaims>,
        anon_setup_sql: &str,
        sql: &SqlOutput,
        count_sql: Option<&SqlOutput>,
    ) -> impl std::future::Future<Output = Result<(Option<String>, Option<i64>), ApiError>> + Send;

    /// Health check — returns OK if the backend pool is healthy.
    fn check_health(&self) -> impl std::future::Future<Output = StatusCode> + Send;

    /// Prometheus-style metrics text.
    fn format_metrics(
        &self,
        cache: &SchemaCache,
    ) -> impl std::future::Future<Output = String> + Send;

    /// Build a fresh schema cache directly from the database (bypasses the pool).
    fn build_schema_cache(
        &self,
        config: &AppConfig,
    ) -> impl std::future::Future<Output = Result<SchemaCache, ApiError>> + Send;

    /// Spawn a background LISTEN/NOTIFY listener and return a receiver for
    /// `(channel, payload)` tuples. The listener manages its own connection.
    fn spawn_listener<'a>(
        &'a self,
        uri: &'a str,
        channel: &'a str,
    ) -> impl std::future::Future<
        Output = Result<tokio::sync::mpsc::UnboundedReceiver<(String, String)>, ApiError>,
    > + Send
           + 'a;
}
