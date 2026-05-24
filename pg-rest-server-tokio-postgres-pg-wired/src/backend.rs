use std::sync::Arc;

use axum::http::StatusCode;
use pg_query_engine::SqlOutput;
use pg_rest_server_common::auth::JwtClaims;
use pg_rest_server_common::backend::Backend;
use pg_rest_server_common::config::AppConfig;
use pg_rest_server_common::error::ApiError;
use pg_rest_server_common::handlers::build_setup_sql;
use pg_schema_cache::SchemaCache;
use tokio_postgres::NoTls;

pub struct PgWiredBackend {
    pub conn_pool: Arc<pg_pool::ConnPool<pg_pool::wire::WirePoolable>>,
    pub async_pool: Arc<pg_wired::AsyncPool>,
}

impl Backend for PgWiredBackend {
    async fn exec_query(
        &self,
        claims: &Option<JwtClaims>,
        anon_setup_sql: &str,
        sql: &SqlOutput,
    ) -> Result<Option<String>, ApiError> {
        let param_refs: Vec<Option<&[u8]>> =
            sql.params.iter().map(|s| Some(s.as_bytes())).collect();
        let param_oids: Vec<u32> = vec![0; sql.params.len()];

        let setup_sql = if claims.is_none() {
            anon_setup_sql.to_string()
        } else {
            build_setup_sql(claims, anon_setup_sql)
        };

        let rows = self
            .async_pool
            .exec_transaction(&setup_sql, &sql.sql, &param_refs, &param_oids)
            .await?;

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
            let cpr: Vec<Option<&[u8]>> = csql.params.iter().map(|s| Some(s.as_bytes())).collect();
            let co: Vec<u32> = vec![0; csql.params.len()];
            let setup_sql = build_setup_sql(claims, anon_setup_sql);
            let rows = self
                .async_pool
                .exec_transaction(&setup_sql, &csql.sql, &cpr, &co)
                .await?;
            rows.first()
                .and_then(|r| r.cell(0))
                .and_then(|b| String::from_utf8_lossy(b).parse::<i64>().ok())
        } else {
            None
        };

        Ok((json, total))
    }

    async fn check_health(&self) -> StatusCode {
        match self.conn_pool.get().await {
            Ok(_) => StatusCode::OK,
            Err(_) => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    async fn format_metrics(&self, cache: &SchemaCache) -> String {
        let pool_metrics = self.conn_pool.metrics();
        format!(
            "# HELP pg_rest_pool_size Current pool size\n\
             # TYPE pg_rest_pool_size gauge\n\
             pg_rest_pool_size {}\n\
             # HELP pg_rest_pool_available Available connections in pool\n\
             # TYPE pg_rest_pool_available gauge\n\
             pg_rest_pool_available {}\n\
             # HELP pg_rest_pool_in_use Connections currently checked out\n\
             # TYPE pg_rest_pool_in_use gauge\n\
             pg_rest_pool_in_use {}\n\
             # HELP pg_rest_pool_checkouts Total checkouts since startup\n\
             # TYPE pg_rest_pool_checkouts counter\n\
             pg_rest_pool_checkouts {}\n\
             # HELP pg_rest_pool_timeouts Total checkout timeouts since startup\n\
             # TYPE pg_rest_pool_timeouts counter\n\
             pg_rest_pool_timeouts {}\n\
             # HELP pg_rest_async_pool_size AsyncPool connection count\n\
             # TYPE pg_rest_async_pool_size gauge\n\
             pg_rest_async_pool_size {}\n\
             # HELP pg_rest_schema_tables Number of tables in schema cache\n\
             # TYPE pg_rest_schema_tables gauge\n\
             pg_rest_schema_tables {}\n\
             # HELP pg_rest_schema_functions Number of functions in schema cache\n\
             # TYPE pg_rest_schema_functions gauge\n\
             pg_rest_schema_functions {}\n",
            pool_metrics.total,
            pool_metrics.idle,
            pool_metrics.in_use,
            pool_metrics.total_checkouts,
            pool_metrics.total_timeouts,
            self.async_pool.size(),
            cache.tables.len(),
            cache.functions.len(),
        )
    }

    async fn build_schema_cache(&self, config: &AppConfig) -> Result<SchemaCache, ApiError> {
        let (client, conn) = tokio_postgres::connect(&config.database.uri, NoTls).await?;
        tokio::spawn(async move {
            conn.await.ok();
        });
        let cache =
            pg_schema_cache::tokio_postgres::build_schema_cache(&client, &config.database.schemas)
                .await?;
        drop(client);
        Ok(cache)
    }

    async fn spawn_listener<'a>(
        &'a self,
        uri: &'a str,
        channel: &'a str,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<(String, String)>, ApiError> {
        let (client, mut connection) = tokio_postgres::connect(uri, NoTls).await?;

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(Ok(tokio_postgres::AsyncMessage::Notification(n))) =
                std::future::poll_fn(|cx| connection.poll_message(cx)).await
            {
                if tx
                    .send((n.channel().to_string(), n.payload().to_string()))
                    .is_err()
                {
                    break;
                }
            }
        });

        let quoted = format!("\"{}\"", channel.replace('"', "\"\""));
        client.execute(&format!("LISTEN {quoted}"), &[]).await?;

        Ok(rx)
    }
}
