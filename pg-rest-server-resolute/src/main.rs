#![allow(clippy::result_large_err)]

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tokio::sync::watch;

use pg_rest_server_resolute::config::AppConfig;
use pg_rest_server_resolute::state::AppState;
use resolute::{Client, SharedPool};

#[derive(Parser)]
#[command(
    name = "pg-rest-server-resolute",
    about = "Automatic REST API for PostgreSQL"
)]
struct Cli {
    /// Path to TOML config file
    #[arg(long, default_value = "pg-rest.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let config = AppConfig::load(&cli.config)?;

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,tower_http=debug".into());

    if config.server.log_format == "json" {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(env_filter)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    }

    // 1. Build initial schema cache via a one-off resolute Client.
    tracing::info!("Loading schema cache...");
    let bootstrap = Client::connect_from_str(&config.database.uri).await?;
    let cache =
        pg_schema_cache::resolute::build_schema_cache(&bootstrap, &config.database.schemas).await?;
    drop(bootstrap);
    tracing::info!(
        "Schema cache loaded: {} tables, {} functions",
        cache.tables.len(),
        cache.functions.len()
    );

    let (cache_tx, cache_rx) = watch::channel(Arc::new(cache));

    // 2. Build JWT decoding key.
    let jwt_decoding_key = jsonwebtoken::DecodingKey::from_secret(config.jwt.secret.as_bytes());
    let jwt_validation = {
        let mut v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
        v.required_spec_claims = Default::default();
        v
    };

    // 3. Build the typed pool from the connection URI.
    let (user, password, host, port, database) =
        parse_pg_uri_for_pool(&config.database.uri).ok_or("invalid database URI")?;
    let addr = format!("{host}:{port}");
    let pool = SharedPool::connect(
        &addr,
        &user,
        &password,
        &database,
        config.database.pool_size.max(2),
    )
    .await
    .map_err(|e| format!("pool init failed: {e}"))?;
    let pool = Arc::new(pool);
    tracing::info!(
        "SharedPool created (size={})",
        config.database.pool_size.max(2)
    );

    // 4. Build application state + router.
    let bind_addr = format!("{}:{}", config.server.host, config.server.port);
    let anon_role_quoted = format!("\"{}\"", config.database.anon_role.replace('"', "\"\""));
    let anon_setup_sql = format!("BEGIN; SET LOCAL ROLE {anon_role_quoted}");
    let state = Arc::new(AppState {
        pool,
        schema_cache: cache_rx,
        schema_cache_tx: cache_tx,
        openapi_cache: tokio::sync::RwLock::new(("".into(), "".into())),
        anon_role_quoted,
        anon_setup_sql,
        config,
        jwt_decoding_key,
        jwt_validation,
        jwt_cache: pg_rest_server_resolute::auth::JwtCache::new(),
    });

    {
        let specs = state.rebuild_openapi_cache();
        *state.openapi_cache.write().await = specs;
    }

    let app = pg_rest_server_resolute::build_router(state.clone());

    // 5. Spawn schema listener (resolute::PgListener handles reconnection).
    tokio::spawn(schema_listener_loop(
        addr.clone(),
        user,
        password,
        database,
        state.clone(),
    ));

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!("Listening on {bind_addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn schema_listener_loop(
    addr: String,
    user: String,
    password: String,
    database: String,
    state: Arc<AppState>,
) {
    let mut backoff = std::time::Duration::from_secs(1);
    loop {
        let schemas = state.config.database.schemas.clone();
        let result = pg_schema_cache::resolute::start_schema_listener(
            &addr,
            &user,
            &password,
            &database,
            schemas,
            state.schema_cache_tx.clone(),
            "pgrst",
        )
        .await;
        match result {
            Ok(()) => break,
            Err(e) => {
                tracing::error!("Schema listener error: {e}, retrying in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
            }
        }
    }
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to listen for ctrl-c");
    tracing::info!("Shutdown signal received");
}

/// Parse a postgres:// URI into the components SharedPool::connect expects.
/// Falls back to the `parse_connection_string` helper resolute uses internally.
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
