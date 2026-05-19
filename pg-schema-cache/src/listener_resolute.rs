use std::sync::Arc;

use resolute::{Client, PgListener};
use tokio::sync::watch;

use crate::error::SchemaCacheError;
use crate::introspection_resolute;
use pg_schema_cache_types::SchemaCache;

/// Establishes a dedicated PostgreSQL connection, executes `LISTEN` on the
/// given channel, and rebuilds the [`SchemaCache`] whenever a notification
/// arrives. The new cache is broadcast to all holders of a
/// [`watch::Receiver<Arc<SchemaCache>>`].
///
/// The underlying [`PgListener`] transparently reconnects with backoff on
/// dropped connections and re-issues the `LISTEN` for `channel_name`. A
/// separate one-shot [`Client`] is used to rebuild the cache so the listener
/// connection stays parked on the socket.
pub async fn start_schema_listener(
    addr: &str,
    user: &str,
    password: &str,
    database: &str,
    schemas: Vec<String>,
    tx: watch::Sender<Arc<SchemaCache>>,
    channel_name: &str,
) -> Result<(), SchemaCacheError> {
    let mut listener = PgListener::connect(addr, user, password, database)
        .await
        .map_err(SchemaCacheError::ResoluteError)?;
    listener
        .listen(channel_name)
        .await
        .map_err(SchemaCacheError::ResoluteError)?;
    tracing::info!("Schema listener started on channel '{channel_name}'");

    loop {
        let notification = listener
            .recv()
            .await
            .map_err(SchemaCacheError::ResoluteError)?;
        if notification.channel != channel_name {
            continue;
        }
        tracing::info!("Schema reload notification received");

        // Use a fresh, short-lived client for the reload so we don't block
        // the listener socket while introspecting.
        let client = match Client::connect(addr, user, password, database).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Schema reload connect failed: {e}");
                continue;
            }
        };
        match introspection_resolute::build_schema_cache(&client, &schemas).await {
            Ok(cache) => {
                tx.send(Arc::new(cache)).ok();
                tracing::info!("Schema cache reloaded");
            }
            Err(e) => {
                tracing::error!("Schema cache reload failed: {e}");
            }
        }
    }
}
