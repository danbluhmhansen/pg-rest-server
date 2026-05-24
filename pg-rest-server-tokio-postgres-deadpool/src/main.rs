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

    tracing::info!("Loading schema cache…");
    let (client, conn) =
        tokio_postgres::connect(&config.database.uri, tokio_postgres::NoTls).await?;
    tokio::spawn(async move {
        conn.await.ok();
    });
    let cache =
        pg_schema_cache::tokio_postgres::build_schema_cache(&client, &config.database.schemas)
            .await?;
    drop(client);
    tracing::info!(
        "Schema cache loaded: {} tables, {} functions",
        cache.tables.len(),
        cache.functions.len()
    );

    let (cache_tx, cache_rx) = watch::channel(Arc::new(cache));

    let jwt_decoding_key = jsonwebtoken::DecodingKey::from_secret(config.jwt.secret.as_bytes());
    let jwt_validation = {
        let mut v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
        v.required_spec_claims = Default::default();
        v
    };

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

    let bind_addr = format!("{}:{}", config.server.host, config.server.port);
    let anon_role_quoted = format!("\"{}\"", config.database.anon_role.replace('"', "\"\""));
    let anon_setup_sql = format!("BEGIN; SET LOCAL ROLE {anon_role_quoted}");
    let state = Arc::new(AppState {
        backend: DeadpoolBackend { pool },
        schema_cache: cache_rx,
        schema_cache_tx: cache_tx,
        openapi_cache: tokio::sync::RwLock::new(("".into(), "".into())),
        anon_role_quoted,
        anon_setup_sql,
        config,
        jwt_decoding_key,
        jwt_validation,
        jwt_cache: pg_rest_server_common::auth::JwtCache::new(),
    });

    {
        let specs = state.rebuild_openapi_cache();
        *state.openapi_cache.write().await = specs;
    }

    let app = pg_rest_server_tokio_postgres_deadpool::build_router(state.clone());

    tokio::spawn(schema_listener_loop(state));

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!("Listening on {bind_addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn schema_listener_loop(state: Arc<AppState<DeadpoolBackend>>) {
    let mut backoff = std::time::Duration::from_secs(1);
    loop {
        let uri = &state.config.database.uri;
        let schemas = &state.config.database.schemas;

        match run_schema_listener(uri, schemas, &state.schema_cache_tx).await {
            Ok(()) => break,
            Err(e) => {
                tracing::error!("Schema listener error: {e}, retrying in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
            }
        }
    }
}

async fn run_schema_listener(
    uri: &str,
    schemas: &[String],
    tx: &watch::Sender<Arc<pg_schema_cache::SchemaCache>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (client, mut connection) = tokio_postgres::connect(uri, tokio_postgres::NoTls).await?;

    let (notify_tx, mut notify_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            match std::future::poll_fn(|cx| connection.poll_message(cx)).await {
                Some(Ok(tokio_postgres::AsyncMessage::Notification(n))) => {
                    if notify_tx.send(n).is_err() {
                        break;
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e),
                None => return Ok(()),
            }
        }
        Ok(())
    });

    let quoted = format!("\"{}\"", "pgrst".replace('"', "\"\""));
    client.execute(&format!("LISTEN {quoted}"), &[]).await?;
    tracing::info!("Schema listener connected");

    while let Some(notification) = notify_rx.recv().await {
        if notification.channel() == "pgrst" {
            tracing::info!("Schema reload notification received");
            match pg_schema_cache::tokio_postgres::build_schema_cache(&client, schemas).await {
                Ok(cache) => {
                    tx.send(Arc::new(cache)).ok();
                    tracing::info!("Schema cache reloaded via NOTIFY");
                }
                Err(e) => tracing::error!("Schema reload failed: {e}"),
            }
        }
    }

    Ok(())
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to listen for ctrl-c");
    tracing::info!("Shutdown signal received");
}
