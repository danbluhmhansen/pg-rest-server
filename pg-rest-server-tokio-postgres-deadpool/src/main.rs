use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tokio::sync::watch;

use pg_rest_server_common::config::AppConfig;
use pg_rest_server_common::state::AppState;
use pg_rest_server_tokio_postgres_deadpool::backend::DeadpoolBackend;

#[derive(Parser)]
#[command(
    name = "pg-rest-server-tokio-postgres-deadpool",
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

    // 1. Build initial schema cache.
    let cache = pg_rest_server_common::schema::build_tokio_postgres_cache(&config).await?;

    let (cache_tx, cache_rx) = watch::channel(Arc::new(cache));

    // 2. Build the typed pool from the connection URI.
    let pool = {
        let pg_cfg: tokio_postgres::Config = config
            .database
            .uri
            .parse()
            .map_err(|e| format!("invalid postgres URI: {e}"))?;
        let mgr = deadpool_postgres::Manager::from_config(
            pg_cfg,
            tokio_postgres::NoTls,
            deadpool_postgres::ManagerConfig {
                recycling_method: deadpool_postgres::RecyclingMethod::Fast,
            },
        );
        deadpool_postgres::Pool::builder(mgr)
            .max_size(config.database.pool_size.max(2))
            .build()
            .map_err(|e| format!("deadpool init failed: {e}"))?
    };
    tracing::info!(
        "deadpool-postgres pool created (max_size={})",
        config.database.pool_size.max(2)
    );

    // 3. Build application state + router.
    let bind_addr = config.server.bind_addr();
    let state =
        Arc::new(AppState::new(DeadpoolBackend { pool }, config, cache_rx, cache_tx).await?);

    state.init_openapi_cache().await;

    let app = pg_rest_server_tokio_postgres_deadpool::build_router(state.clone());

    // 4. Spawn schema listener.
    tokio::spawn(pg_rest_server_common::listener::schema_listener_loop(state));

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!("Listening on {bind_addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(pg_rest_server_common::signal::shutdown_signal())
        .await?;

    Ok(())
}
