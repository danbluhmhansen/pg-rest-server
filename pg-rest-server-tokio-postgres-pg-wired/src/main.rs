use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tokio::sync::watch;

use pg_rest_server_common::config::AppConfig;
use pg_rest_server_common::state::AppState;
use pg_rest_server_tokio_postgres_pg_wired::backend::PgWiredBackend;

#[derive(Parser)]
#[command(
    name = "pg-rest-server-tokio-postgres-pg-wired",
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
    let (user, password, host, port, database) =
        pg_rest_server_common::uri::parse_pg_uri(&config.database.uri)
            .ok_or("invalid database URI")?;
    let addr = format!("{host}:{port}");
    let conn_pool = {
        let mut cfg = pg_pool::ConnPoolConfig::default();
        cfg.addr = addr.clone();
        cfg.user = user.clone();
        cfg.password = password.clone();
        cfg.database = database.clone();
        cfg.min_idle = 1;
        cfg.max_size = config.database.pool_size.max(2);
        pg_pool::ConnPool::<pg_pool::wire::WirePoolable>::new(
            cfg,
            pg_pool::LifecycleHooks::default(),
        )
        .await
        .map_err(|e| format!("ConnPool init failed: {e}"))?
    };
    tracing::info!(
        "ConnPool created (max_size={})",
        config.database.pool_size.max(2)
    );

    let async_pool_size = config.database.pool_size.min(8);
    let async_pool =
        pg_wired::AsyncPool::connect(&addr, &user, &password, &database, async_pool_size).await?;
    tracing::info!("AsyncPool created with {} connections", async_pool_size);

    // 3. Build application state + router.
    let bind_addr = config.server.bind_addr();
    let state = Arc::new(AppState::new(
        PgWiredBackend {
            conn_pool,
            async_pool,
        },
        config,
        cache_rx,
        cache_tx,
    ));

    state.init_openapi_cache().await;

    let app = pg_rest_server_tokio_postgres_pg_wired::build_router(state.clone());

    // 4. Spawn schema listener.
    tokio::spawn(pg_rest_server_common::listener::schema_listener_loop(state));

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!("Listening on {bind_addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(pg_rest_server_common::signal::shutdown_signal())
        .await?;

    Ok(())
}
