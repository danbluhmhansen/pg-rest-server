use std::sync::Arc;

use tokio::sync::watch;
use tokio_postgres::NoTls;

use crate::error::SchemaCacheError;
use crate::introspection_tp;
use pg_schema_cache_types::SchemaCache;

/// Establishes a dedicated PostgreSQL connection, executes `LISTEN` on the
/// given channel, and rebuilds the [`SchemaCache`] whenever a notification
/// arrives. The new cache is broadcast to all holders of a
/// [`watch::Receiver<Arc<SchemaCache>>`].
///
/// This function runs indefinitely until the connection drops.
pub async fn start_schema_listener(
    connection_string: &str,
    schemas: Vec<String>,
    tx: watch::Sender<Arc<SchemaCache>>,
    channel_name: &str,
) -> Result<(), SchemaCacheError> {
    let (client, mut connection) = tokio_postgres::connect(connection_string, NoTls).await?;

    // Drive the connection manually so we can intercept notifications.
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
                Some(Err(e)) => {
                    tracing::error!("Schema listener connection error: {e}");
                    break;
                }
                None => {
                    tracing::info!("Schema listener connection closed");
                    break;
                }
            }
        }
    });

    // Identifier-quote the channel name to prevent injection.
    let quoted = format!("\"{}\"", channel_name.replace('"', "\"\""));
    client.execute(&format!("LISTEN {quoted}"), &[]).await?;

    tracing::info!("Schema listener started on channel '{channel_name}'");

    while let Some(notification) = notify_rx.recv().await {
        if notification.channel() == channel_name {
            tracing::info!("Schema reload notification received");
            match introspection_tp::build_schema_cache(&client, &schemas).await {
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

    Ok(())
}
