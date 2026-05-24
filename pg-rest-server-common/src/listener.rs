use std::sync::Arc;

use tokio::sync::watch;

use crate::backend::Backend;
use crate::state::AppState;

pub async fn run_schema_listener(
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

pub async fn schema_listener_loop<B: Backend>(state: Arc<AppState<B>>) {
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
