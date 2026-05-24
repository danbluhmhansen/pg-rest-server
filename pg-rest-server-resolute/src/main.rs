use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tokio::sync::watch;

use pg_rest_server_common::config::AppConfig;
use pg_rest_server_common::state::AppState;
use pg_rest_server_resolute::backend::ResoluteBackend;
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

    pg_rest_server_common::tracing::init(&config);

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

    // 2. Build the typed pool from the connection URI.
    let (user, password, host, port, database) =
        pg_rest_server_common::uri::parse_pg_uri(&config.database.uri)
            .ok_or("invalid database URI")?;
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

    // 3. Build application state + router.
    let bind_addr = config.server.bind_addr();
    let state = Arc::new(AppState::new(
        ResoluteBackend { pool },
        config,
        cache_rx,
        cache_tx,
    ));

    state.init_openapi_cache().await;

    let app = pg_rest_server_resolute::build_router(state.clone());

    // 4. Spawn schema listener (resolute::PgListener handles reconnection).
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
        .with_graceful_shutdown(pg_rest_server_common::signal::shutdown_signal())
        .await?;

    Ok(())
}

async fn schema_listener_loop(
    addr: String,
    user: String,
    password: String,
    database: String,
    state: Arc<AppState<ResoluteBackend>>,
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
