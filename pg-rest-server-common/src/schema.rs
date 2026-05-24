use crate::config::AppConfig;

pub async fn build_tokio_postgres_cache(
    config: &AppConfig,
) -> Result<pg_schema_cache::SchemaCache, Box<dyn std::error::Error>> {
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
    Ok(cache)
}
