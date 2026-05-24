//! Integration tests against a real PostgreSQL instance.
//!
//! Requires:
//!   docker compose up -d
//!
//! Run with:
//!   cargo test -p pg-rest-server-tokio-postgres-pg-wired --test integration

use std::sync::Arc;

use tokio::sync::watch;

use pg_rest_server_common::config::AppConfig;
use pg_rest_server_common::state::AppState;
use pg_rest_server_test as suite;
use pg_rest_server_tokio_postgres_pg_wired::backend::PgWiredBackend;

async fn setup() -> axum::Router {
    let config = AppConfig {
        database: pg_rest_server_common::config::DatabaseConfig {
            uri: suite::DB_URI.to_string(),
            schemas: vec!["api".to_string()],
            anon_role: "web_anon".to_string(),
            pool_size: 5,
            prepared_statements: true,
        },
        server: pg_rest_server_common::config::ServerConfig::default(),
        auth: pg_rest_server_common::config::AuthConfig::Secret(
            pg_rest_server_common::config::SecretAuth {
                secret: suite::JWT_SECRET.to_string(),
            },
        ),
    };

    let (client, conn) = tokio_postgres::connect(&config.database.uri, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        conn.await.ok();
    });
    let cache =
        pg_schema_cache::tokio_postgres::build_schema_cache(&client, &config.database.schemas)
            .await
            .unwrap();
    drop(client);

    let (cache_tx, cache_rx) = watch::channel(Arc::new(cache));

    let mut pool_cfg = pg_pool::ConnPoolConfig::default();
    pool_cfg.addr = "127.0.0.1:54322".to_string();
    pool_cfg.user = "authenticator".to_string();
    pool_cfg.password = "authenticator".to_string();
    pool_cfg.database = "postgrest_test".to_string();
    pool_cfg.min_idle = 1;
    pool_cfg.max_size = 5;
    let conn_pool = pg_pool::ConnPool::<pg_pool::wire::WirePoolable>::new(
        pool_cfg,
        pg_pool::LifecycleHooks::default(),
    )
    .await
    .unwrap();

    let async_pool = pg_wired::AsyncPool::connect(
        "127.0.0.1:54322",
        "authenticator",
        "authenticator",
        "postgrest_test",
        4,
    )
    .await
    .unwrap();

    let state = Arc::new(AppState {
        backend: PgWiredBackend {
            conn_pool,
            async_pool,
        },
        schema_cache: cache_rx,
        schema_cache_tx: cache_tx,
        openapi_cache: tokio::sync::RwLock::new(("".into(), "".into())),
        config,
        auth_source: pg_rest_server_common::auth::AuthSource::from_secret(suite::JWT_SECRET),
        jwt_cache: pg_rest_server_common::auth::JwtCache::new(),
        anon_role_quoted: "\"web_anon\"".to_string(),
        anon_setup_sql: "BEGIN; SET LOCAL ROLE \"web_anon\"".to_string(),
    });

    {
        let specs = state.rebuild_openapi_cache();
        *state.openapi_cache.write().await = specs;
    }

    pg_rest_server_tokio_postgres_pg_wired::build_router(state)
}

// ===========================================================================
// Schema cache tests
// ===========================================================================

#[tokio::test]
async fn test_schema_cache_loads_tables() {
    suite::test_schema_cache_loads_tables(&setup().await).await;
}

// ===========================================================================
// Read (GET) tests
// ===========================================================================

#[tokio::test]
async fn test_read_all_authors() {
    suite::test_read_all_authors(&setup().await).await;
}

#[tokio::test]
async fn test_read_select_columns() {
    suite::test_read_select_columns(&setup().await).await;
}

#[tokio::test]
async fn test_read_filter_eq() {
    suite::test_read_filter_eq(&setup().await).await;
}

#[tokio::test]
async fn test_read_filter_gt() {
    suite::test_read_filter_gt(&setup().await).await;
}

#[tokio::test]
async fn test_read_filter_in() {
    suite::test_read_filter_in(&setup().await).await;
}

#[tokio::test]
async fn test_read_filter_is_null() {
    suite::test_read_filter_is_null(&setup().await).await;
}

#[tokio::test]
async fn test_read_order() {
    suite::test_read_order(&setup().await).await;
}

#[tokio::test]
async fn test_read_limit_offset() {
    suite::test_read_limit_offset(&setup().await).await;
}

#[tokio::test]
async fn test_read_count_exact() {
    suite::test_read_count_exact(&setup().await).await;
}

#[tokio::test]
async fn test_read_count_exact_content_range() {
    suite::test_read_count_exact_content_range(&setup().await).await;
}

#[tokio::test]
async fn test_read_csv() {
    suite::test_read_csv(&setup().await).await;
}

#[tokio::test]
async fn test_read_nonexistent_table() {
    suite::test_read_nonexistent_table(&setup().await).await;
}

// ===========================================================================
// Embedding tests
// ===========================================================================

#[tokio::test]
async fn test_embed_one_to_many() {
    suite::test_embed_one_to_many(&setup().await).await;
}

#[tokio::test]
async fn test_embed_many_to_one() {
    suite::test_embed_many_to_one(&setup().await).await;
}

// ===========================================================================
// Insert (POST) tests
// ===========================================================================

#[tokio::test]
async fn test_insert_and_return() {
    suite::test_insert_and_return(&setup().await).await;
}

#[tokio::test]
async fn test_insert_minimal() {
    suite::test_insert_minimal(&setup().await).await;
}

// ===========================================================================
// Update (PATCH) tests
// ===========================================================================

#[tokio::test]
async fn test_update_with_filter() {
    suite::test_update_with_filter(&setup().await).await;
}

// ===========================================================================
// Delete (DELETE) tests
// ===========================================================================

#[tokio::test]
async fn test_delete_with_filter() {
    suite::test_delete_with_filter(&setup().await).await;
}

// ===========================================================================
// Upsert tests
// ===========================================================================

#[tokio::test]
async fn test_upsert_merge_duplicates() {
    suite::test_upsert_merge_duplicates(&setup().await).await;
}

// ===========================================================================
// RPC (function call) tests
// ===========================================================================

#[tokio::test]
async fn test_rpc_scalar() {
    suite::test_rpc_scalar(&setup().await).await;
}

#[tokio::test]
async fn test_rpc_setof() {
    suite::test_rpc_setof(&setup().await).await;
}

#[tokio::test]
async fn test_rpc_default_param() {
    suite::test_rpc_default_param(&setup().await).await;
}

#[tokio::test]
async fn test_rpc_get_immutable() {
    suite::test_rpc_get_immutable(&setup().await).await;
}

// ===========================================================================
// RLS tests
// ===========================================================================

#[tokio::test]
async fn test_rls_anon_sees_only_published() {
    suite::test_rls_anon_sees_only_published(&setup().await).await;
}

#[tokio::test]
async fn test_rls_user_sees_all() {
    suite::test_rls_user_sees_all(&setup().await).await;
}

// ===========================================================================
// Health endpoints
// ===========================================================================

#[tokio::test]
async fn test_live() {
    suite::test_live(&setup().await).await;
}

#[tokio::test]
async fn test_ready() {
    suite::test_ready(&setup().await).await;
}

// ===========================================================================
// OpenAPI spec tests
// ===========================================================================

#[tokio::test]
async fn test_openapi_v2() {
    suite::test_openapi_v2(&setup().await).await;
}

#[tokio::test]
async fn test_openapi_v3() {
    suite::test_openapi_v3(&setup().await).await;
}

// ===========================================================================
// Logical operators (or/and)
// ===========================================================================

#[tokio::test]
async fn test_filter_or() {
    suite::test_filter_or(&setup().await).await;
}

#[tokio::test]
async fn test_filter_nested_and_or() {
    suite::test_filter_nested_and_or(&setup().await).await;
}

// ===========================================================================
// not.is.null
// ===========================================================================

#[tokio::test]
async fn test_filter_not_is_null() {
    suite::test_filter_not_is_null(&setup().await).await;
}

// ===========================================================================
// Select type cast
// ===========================================================================

#[tokio::test]
async fn test_select_cast() {
    suite::test_select_cast(&setup().await).await;
}

// ===========================================================================
// Singular response
// ===========================================================================

#[tokio::test]
async fn test_singular_response() {
    suite::test_singular_response(&setup().await).await;
}

#[tokio::test]
async fn test_singular_response_406_multiple() {
    suite::test_singular_response_406_multiple(&setup().await).await;
}

// ===========================================================================
// Spread embed
// ===========================================================================

#[tokio::test]
async fn test_spread_embed() {
    suite::test_spread_embed(&setup().await).await;
}

// ===========================================================================
// EXPLAIN
// ===========================================================================

#[tokio::test]
async fn test_explain() {
    suite::test_explain(&setup().await).await;
}

// ===========================================================================
// Generated columns
// ===========================================================================

#[tokio::test]
async fn test_generated_column_excluded_from_insert() {
    suite::test_generated_column_excluded_from_insert(&setup().await).await;
}

// ===========================================================================
// on_conflict with specific columns
// ===========================================================================

#[tokio::test]
async fn test_on_conflict_specific_columns() {
    suite::test_on_conflict_specific_columns(&setup().await).await;
}

// ===========================================================================
// Edge cases
// ===========================================================================

#[tokio::test]
async fn test_empty_table_returns_empty_array() {
    suite::test_empty_table_returns_empty_array(&setup().await).await;
}

#[tokio::test]
async fn test_special_characters_in_filter_value() {
    suite::test_special_characters_in_filter_value(&setup().await).await;
}

#[tokio::test]
async fn test_select_nonexistent_column_still_works() {
    suite::test_select_nonexistent_column_still_works(&setup().await).await;
}

#[tokio::test]
async fn test_filter_like_with_percent() {
    suite::test_filter_like_with_percent(&setup().await).await;
}

#[tokio::test]
async fn test_filter_ilike() {
    suite::test_filter_ilike(&setup().await).await;
}

#[tokio::test]
async fn test_multiple_filters_anded() {
    suite::test_multiple_filters_anded(&setup().await).await;
}

#[tokio::test]
async fn test_insert_with_null_value() {
    suite::test_insert_with_null_value(&setup().await).await;
}

#[tokio::test]
async fn test_read_view() {
    suite::test_read_view(&setup().await).await;
}

// ===========================================================================
// Admin endpoints
// ===========================================================================

#[tokio::test]
async fn test_reload_endpoint() {
    suite::test_reload_endpoint(&setup().await).await;
}

#[tokio::test]
async fn test_metrics_endpoint() {
    suite::test_metrics_endpoint(&setup().await).await;
}
