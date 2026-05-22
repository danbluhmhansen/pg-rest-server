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
    Database(resolute::TypedError),
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
                if let Some(pg_err) = pg_error(e) {
                    write!(f, "database error: {}: {}", pg_err.code, pg_err.message)
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
                let status = if let Some(pg_err) = pg_error(e) {
                    match pg_err.code.as_str() {
                        "42501" => StatusCode::UNAUTHORIZED, // insufficient privilege (PostgREST compat)
                        "23505" => StatusCode::CONFLICT,     // unique violation
                        "23503" => StatusCode::CONFLICT,     // FK violation
                        "23502" => StatusCode::BAD_REQUEST,  // not null violation
                        "23514" => StatusCode::BAD_REQUEST,  // check violation
                        "42P01" => StatusCode::NOT_FOUND,    // undefined table
                        "42883" => StatusCode::NOT_FOUND,    // undefined function
                        c if c.starts_with("P0") => StatusCode::BAD_REQUEST, // user RAISE
                        c if c.starts_with("23") => StatusCode::BAD_REQUEST, // integrity
                        c if c.starts_with("22") => StatusCode::BAD_REQUEST, // data exception
                        _ => StatusCode::INTERNAL_SERVER_ERROR,
                    }
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                };
                (status, self.to_string())
            }
            Self::Pool(_) => (StatusCode::SERVICE_UNAVAILABLE, self.to_string()),
        };

        // PostgREST-compatible error format.
        let body = if let Self::Database(e) = &self {
            if let Some(pg_err) = pg_error(e) {
                serde_json::json!({
                    "code": pg_err.code,
                    "message": pg_err.message,
                    "details": pg_err.detail,
                    "hint": pg_err.hint,
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

impl From<resolute::TypedError> for ApiError {
    fn from(e: resolute::TypedError) -> Self {
        match &e {
            resolute::TypedError::Pool(_) => Self::Pool(e.to_string()),
            _ => Self::Database(e),
        }
    }
}

impl From<pg_schema_cache::SchemaCacheError> for ApiError {
    fn from(e: pg_schema_cache::SchemaCacheError) -> Self {
        Self::BadRequest(format!("schema cache error: {e}"))
    }
}

impl From<pg_rest_server_common::handlers::HandlerError> for ApiError {
    fn from(e: pg_rest_server_common::handlers::HandlerError) -> Self {
        match e {
            pg_rest_server_common::handlers::HandlerError::BadRequest(msg) => Self::BadRequest(msg),
            pg_rest_server_common::handlers::HandlerError::NotAcceptable(msg) => {
                Self::NotAcceptable(msg)
            }
            pg_rest_server_common::handlers::HandlerError::Parse(e) => Self::Parse(e),
        }
    }
}

/// Drill into a [`resolute::TypedError`] looking for a server-reported
/// `ErrorResponse`. Returns `None` for non-PG-server errors (timeouts, decode,
/// pool, I/O, config).
fn pg_error(e: &resolute::TypedError) -> Option<&pg_wired::PgError> {
    let mut cur = e;
    loop {
        match cur {
            resolute::TypedError::Wire(w) => match w.as_ref() {
                pg_wired::PgWireError::Pg(p) => return Some(p),
                _ => return None,
            },
            resolute::TypedError::QueryFailed { source, .. } => {
                cur = source.as_ref();
            }
            _ => return None,
        }
    }
}
