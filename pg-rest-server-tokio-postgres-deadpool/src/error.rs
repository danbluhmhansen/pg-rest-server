use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

#[derive(Debug)]
pub enum ApiError {
    TableNotFound(String),
    FunctionNotFound(String),
    MethodNotAllowed,
    Unauthorized(String),
    BadRequest(String),
    QueryEngine(pg_query_engine::QueryEngineError),
    Parse(pg_query_engine::ParseError),
    Database(tokio_postgres::Error),
    NotAcceptable(String),
    Pool(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAcceptable(m) => write!(f, "not acceptable: {m}"),
            Self::TableNotFound(t) => write!(f, "table or view not found: {t}"),
            Self::FunctionNotFound(t) => write!(f, "function not found: {t}"),
            Self::MethodNotAllowed => write!(f, "method not allowed"),
            Self::Unauthorized(m) => write!(f, "unauthorized: {m}"),
            Self::BadRequest(m) => write!(f, "{m}"),
            Self::QueryEngine(e) => write!(f, "{e}"),
            Self::Parse(e) => write!(f, "{e}"),
            Self::Database(e) => {
                if let Some(db_err) = e.as_db_error() {
                    write!(
                        f,
                        "database error: {}: {}",
                        db_err.code().code(),
                        db_err.message()
                    )
                } else {
                    write!(f, "database error: {e}")
                }
            }
            Self::Pool(msg) => write!(f, "connection pool error: {msg}"),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            Self::TableNotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            Self::FunctionNotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            Self::MethodNotAllowed => (StatusCode::METHOD_NOT_ALLOWED, self.to_string()),
            Self::NotAcceptable(_) => (StatusCode::NOT_ACCEPTABLE, self.to_string()),
            Self::Unauthorized(_) => (StatusCode::UNAUTHORIZED, self.to_string()),
            Self::BadRequest(_) | Self::Parse(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            Self::QueryEngine(e) => match e {
                pg_query_engine::QueryEngineError::TableNotFound(_) => {
                    (StatusCode::NOT_FOUND, self.to_string())
                }
                _ => (StatusCode::BAD_REQUEST, self.to_string()),
            },
            Self::Database(e) => {
                let code = e.code();
                let status = match code.map(|c| c.code()) {
                    Some("42501") => StatusCode::UNAUTHORIZED, // insufficient privilege (PostgREST compat)
                    Some("23505") => StatusCode::CONFLICT,     // unique violation
                    Some("23503") => StatusCode::CONFLICT,     // FK violation
                    Some("23502") => StatusCode::BAD_REQUEST,  // not null violation
                    Some("23514") => StatusCode::BAD_REQUEST,  // check violation
                    Some("42P01") => StatusCode::NOT_FOUND,    // undefined table
                    Some("42883") => StatusCode::NOT_FOUND,    // undefined function
                    Some(c) if c.starts_with("P0") => StatusCode::BAD_REQUEST, // user RAISE
                    Some(c) if c.starts_with("23") => StatusCode::BAD_REQUEST, // integrity
                    Some(c) if c.starts_with("22") => StatusCode::BAD_REQUEST, // data exception
                    _ => StatusCode::INTERNAL_SERVER_ERROR,
                };
                (status, self.to_string())
            }
            Self::Pool(_) => (StatusCode::SERVICE_UNAVAILABLE, self.to_string()),
        };

        // PostgREST-compatible error format.
        let body = if let Self::Database(e) = &self {
            if let Some(db_err) = e.as_db_error() {
                serde_json::json!({
                    "code": db_err.code().code(),
                    "message": db_err.message(),
                    "details": db_err.detail(),
                    "hint": db_err.hint(),
                })
            } else {
                serde_json::json!({
                    "code": status.as_str(),
                    "message": message,
                })
            }
        } else {
            let code = match &self {
                Self::TableNotFound(_) | Self::FunctionNotFound(_) => "PGRST200",
                Self::MethodNotAllowed => "PGRST105",
                Self::NotAcceptable(_) => "PGRST107",
                Self::Unauthorized(_) => "PGRST301",
                Self::BadRequest(_) | Self::Parse(_) => "PGRST100",
                Self::QueryEngine(_) => "PGRST100",
                Self::Pool(_) => "PGRST003",
                Self::Database(_) => unreachable!(),
            };
            serde_json::json!({
                "code": code,
                "message": message,
            })
        };

        (
            status,
            [(header::CONTENT_TYPE, "application/json")],
            body.to_string(),
        )
            .into_response()
    }
}

impl From<pg_query_engine::QueryEngineError> for ApiError {
    fn from(e: pg_query_engine::QueryEngineError) -> Self {
        Self::QueryEngine(e)
    }
}

impl From<pg_query_engine::ParseError> for ApiError {
    fn from(e: pg_query_engine::ParseError) -> Self {
        Self::Parse(e)
    }
}

impl From<tokio_postgres::Error> for ApiError {
    fn from(e: tokio_postgres::Error) -> Self {
        Self::Database(e)
    }
}

impl From<pg_schema_cache::SchemaCacheError> for ApiError {
    fn from(e: pg_schema_cache::SchemaCacheError) -> Self {
        Self::BadRequest(format!("schema cache error: {e}"))
    }
}
