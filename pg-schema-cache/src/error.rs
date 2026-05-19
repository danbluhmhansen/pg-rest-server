#[derive(Debug, thiserror::Error)]
pub enum SchemaCacheError {
    #[cfg(feature = "resolute")]
    #[error("resolute error: {0}")]
    ResoluteError(#[from] resolute::TypedError),

    #[cfg(feature = "tokio-postgres")]
    #[error("tokio-postgres error: {0}")]
    TpError(#[from] tokio_postgres::Error),

    #[error("unexpected data from database: {0}")]
    UnexpectedData(String),
}
