use axum::http::StatusCode;
use deadpool_postgres::Pool;
use pg_query_engine::SqlOutput;
use pg_rest_server_common::auth::JwtClaims;
use pg_rest_server_common::backend::Backend;
use pg_rest_server_common::error::ApiError;
use pg_rest_server_common::handlers::build_setup_sql;
use pg_schema_cache::SchemaCache;

/// Extract column 0 from a row as a String, trying `text` first and falling
/// back to `json`/`jsonb` so that `EXPLAIN (FORMAT JSON)` works regardless of
/// how the server reports the column type.
fn row_col_as_string(row: &tokio_postgres::Row) -> Option<String> {
    if let Ok(v) = row.try_get::<_, Option<String>>(0) {
        return v;
    }
    if let Ok(v) = row.try_get::<_, Option<serde_json::Value>>(0) {
        return v.map(|j| j.to_string());
    }
    None
}

pub struct DeadpoolBackend {
    pub pool: Pool,
}

/// Bind `sql.params: Vec<String>` as text params for tokio-postgres.
fn as_text_params(sql: &SqlOutput) -> Vec<&(dyn tokio_postgres::types::ToSql + Sync)> {
    sql.params
        .iter()
        .map(|s| s as &(dyn tokio_postgres::types::ToSql + Sync))
        .collect()
}

impl Backend for DeadpoolBackend {
    async fn exec_query(
        &self,
        claims: &Option<JwtClaims>,
        anon_setup_sql: &str,
        sql: &SqlOutput,
    ) -> Result<Option<String>, ApiError> {
        let mut client = self
            .pool
            .get()
            .await
            .map_err(|e| ApiError::Pool(e.to_string()))?;
        let tx = client.transaction().await?;

        let setup_full = build_setup_sql(claims, anon_setup_sql);
        let setup_inner = setup_full.strip_prefix("BEGIN; ").unwrap_or(&setup_full);
        if !setup_inner.is_empty() {
            tx.batch_execute(setup_inner).await?;
        }

        let params = as_text_params(sql);
        let rows = tx.query(&sql.sql, &params).await?;
        tx.commit().await?;

        Ok(rows.first().and_then(row_col_as_string))
    }

    async fn exec_query_with_count(
        &self,
        claims: &Option<JwtClaims>,
        anon_setup_sql: &str,
        sql: &SqlOutput,
        count_sql: Option<&SqlOutput>,
    ) -> Result<(Option<String>, Option<i64>), ApiError> {
        let mut client = self
            .pool
            .get()
            .await
            .map_err(|e| ApiError::Pool(e.to_string()))?;
        let tx = client.transaction().await?;

        let setup_full = build_setup_sql(claims, anon_setup_sql);
        let setup_inner = setup_full.strip_prefix("BEGIN; ").unwrap_or(&setup_full);
        if !setup_inner.is_empty() {
            tx.batch_execute(setup_inner).await?;
        }

        let params = as_text_params(sql);
        let rows = tx.query(&sql.sql, &params).await?;
        let json = rows.first().and_then(row_col_as_string);

        let total = if let Some(csql) = count_sql {
            let cparams = as_text_params(csql);
            let crows = tx.query(&csql.sql, &cparams).await?;
            crows
                .first()
                .and_then(|r| r.try_get::<_, Option<i64>>(0).ok().flatten())
        } else {
            None
        };

        tx.commit().await?;
        Ok((json, total))
    }

    async fn check_health(&self) -> StatusCode {
        match self.pool.get().await {
            Ok(_) => StatusCode::OK,
            Err(_) => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    async fn format_metrics(&self, cache: &SchemaCache) -> String {
        let status = self.pool.status();
        format!(
            "# HELP pg_rest_pool_size Current pool size\n\
             # TYPE pg_rest_pool_size gauge\n\
             pg_rest_pool_size {}\n\
             # HELP pg_rest_pool_available Available connections in pool\n\
             # TYPE pg_rest_pool_available gauge\n\
             pg_rest_pool_available {}\n\
             # HELP pg_rest_pool_max_size Configured pool max size\n\
             # TYPE pg_rest_pool_max_size gauge\n\
             pg_rest_pool_max_size {}\n\
             # HELP pg_rest_pool_waiting Number of waiting checkouts\n\
             # TYPE pg_rest_pool_waiting gauge\n\
             pg_rest_pool_waiting {}\n\
             # HELP pg_rest_schema_tables Number of tables in schema cache\n\
             # TYPE pg_rest_schema_tables gauge\n\
             pg_rest_schema_tables {}\n\
             # HELP pg_rest_schema_functions Number of functions in schema cache\n\
             # TYPE pg_rest_schema_functions gauge\n\
             pg_rest_schema_functions {}\n",
            status.size,
            status.available,
            status.max_size,
            status.waiting,
            cache.tables.len(),
            cache.functions.len(),
        )
    }
}
