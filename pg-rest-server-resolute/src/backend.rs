use std::sync::Arc;

use axum::http::StatusCode;
use pg_query_engine::SqlOutput;
use pg_rest_server_common::auth::JwtClaims;
use pg_rest_server_common::backend::Backend;
use pg_rest_server_common::config::AppConfig;
use pg_rest_server_common::error::ApiError;
use pg_rest_server_common::handlers::build_setup_sql;
use pg_schema_cache::SchemaCache;
use resolute::{Client, PgListener, SharedPool};

pub struct ResoluteBackend {
    pub pool: Arc<SharedPool>,
}

impl Backend for ResoluteBackend {
    async fn exec_query(
        &self,
        claims: &Option<JwtClaims>,
        anon_setup_sql: &str,
        sql: &SqlOutput,
    ) -> Result<Option<String>, ApiError> {
        let setup_sql = build_setup_sql(claims, anon_setup_sql);
        let param_refs: Vec<Option<&[u8]>> =
            sql.params.iter().map(|s| Some(s.as_bytes())).collect();
        let param_oids: Vec<u32> = vec![0; sql.params.len()];
        let rows = self
            .pool
            .exec_transaction(&setup_sql, &sql.sql, &param_refs, &param_oids)
            .await
            .map_err(ApiError::from)?;
        Ok(rows
            .first()
            .and_then(|r| r.cell(0))
            .map(|b| String::from_utf8_lossy(b).into_owned()))
    }

    async fn exec_query_with_count(
        &self,
        claims: &Option<JwtClaims>,
        anon_setup_sql: &str,
        sql: &SqlOutput,
        count_sql: Option<&SqlOutput>,
    ) -> Result<(Option<String>, Option<i64>), ApiError> {
        let json = self.exec_query(claims, anon_setup_sql, sql).await?;
        let total = if let Some(csql) = count_sql {
            let setup_sql = build_setup_sql(claims, anon_setup_sql);
            let cpr: Vec<Option<&[u8]>> = csql.params.iter().map(|s| Some(s.as_bytes())).collect();
            let co: Vec<u32> = vec![0; csql.params.len()];
            let crows = self
                .pool
                .exec_transaction(&setup_sql, &csql.sql, &cpr, &co)
                .await
                .map_err(ApiError::from)?;
            crows
                .first()
                .and_then(|r| r.cell(0))
                .and_then(|b| String::from_utf8_lossy(b).parse::<i64>().ok())
        } else {
            None
        };
        Ok((json, total))
    }

    async fn check_health(&self) -> StatusCode {
        if self.pool.alive_count().await > 0 {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        }
    }

    async fn format_metrics(&self, cache: &SchemaCache) -> String {
        let pool_size = self.pool.size();
        let pool_alive = self.pool.alive_count().await;
        format!(
            "# HELP pg_rest_pool_size Configured pool size\n\
             # TYPE pg_rest_pool_size gauge\n\
             pg_rest_pool_size {pool_size}\n\
             # HELP pg_rest_pool_alive Live connections in the shared pool\n\
             # TYPE pg_rest_pool_alive gauge\n\
             pg_rest_pool_alive {pool_alive}\n\
             # HELP pg_rest_schema_tables Number of tables in schema cache\n\
             # TYPE pg_rest_schema_tables gauge\n\
             pg_rest_schema_tables {}\n\
             # HELP pg_rest_schema_functions Number of functions in schema cache\n\
             # TYPE pg_rest_schema_functions gauge\n\
             pg_rest_schema_functions {}\n",
            cache.tables.len(),
            cache.functions.len(),
        )
    }

    async fn build_schema_cache(&self, config: &AppConfig) -> Result<SchemaCache, ApiError> {
        let client = Client::connect_from_str(&config.database.uri).await?;
        let cache =
            pg_schema_cache::resolute::build_schema_cache(&client, &config.database.schemas)
                .await?;
        Ok(cache)
    }

    async fn spawn_listener<'a>(
        &'a self,
        uri: &'a str,
        channel: &'a str,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<(String, String)>, ApiError> {
        let (user, password, host, port, database) = parse_pg_uri_for_pool(uri)
            .ok_or_else(|| ApiError::BadRequest("invalid database URI".into()))?;
        let addr = format!("{host}:{port}");

        let mut listener = PgListener::connect(&addr, &user, &password, &database).await?;
        listener.listen(channel).await?;

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok(n) = listener.recv().await {
                if tx.send((n.channel, n.payload)).is_err() {
                    break;
                }
            }
        });

        Ok(rx)
    }
}

/// Parse a postgres:// URI into (user, password, host, port, database).
fn parse_pg_uri_for_pool(uri: &str) -> Option<(String, String, String, u16, String)> {
    let rest = uri
        .strip_prefix("postgres://")
        .or_else(|| uri.strip_prefix("postgresql://"))?;
    let rest = rest.split('?').next().unwrap_or(rest);
    let (auth, hostdb) = rest.split_once('@').unwrap_or(("postgres:postgres", rest));
    let (user, password) = auth.split_once(':').unwrap_or((auth, ""));
    let (hostport, database) = hostdb.split_once('/').unwrap_or((hostdb, "postgres"));
    let (host, port_str) = hostport.split_once(':').unwrap_or((hostport, "5432"));
    let port: u16 = port_str.parse().unwrap_or(5432);
    Some((
        user.to_string(),
        password.to_string(),
        host.to_string(),
        port,
        database.to_string(),
    ))
}
