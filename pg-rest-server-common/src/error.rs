/// Trait abstracting over PostgreSQL database error types from different
/// driver backends (resolute, tokio-postgres).
///
/// Implementations provide access to the SQLSTATE code, message, detail, and
/// hint fields reported by the PostgreSQL server, as well as a way to
/// distinguish pool-level errors from query-level errors.
pub trait DbErrorDetail: std::error::Error + Send + Sync + 'static {
    /// SQLSTATE code (e.g. `"42P01"` for undefined_table), if the error was
    /// reported by the PostgreSQL server.
    fn code(&self) -> Option<&str>;

    /// Primary human-readable error message from the server, if available.
    fn message(&self) -> Option<&str>;

    /// Optional detail text from the server.
    fn detail(&self) -> Option<&str>;

    /// Optional hint text from the server.
    fn hint(&self) -> Option<&str>;

    /// Whether this error represents a connection-pool failure rather than a
    /// query-level error.  When `true` the caller should typically surface a
    /// `503 Service Unavailable` instead of a database-level error.
    fn is_pool_error(&self) -> bool;
}

// ---------------------------------------------------------------------------
// Helper: format the Display output for a database error
// ---------------------------------------------------------------------------

pub fn format_database_error(e: &dyn DbErrorDetail) -> String {
    if let (Some(code), Some(msg)) = (e.code(), e.message()) {
        format!("database error: {code}: {msg}")
    } else {
        format!("database error: {e}")
    }
}

// ---------------------------------------------------------------------------
// Helper: map a SQLSTATE code to an HTTP status code
// ---------------------------------------------------------------------------

pub fn database_error_status(e: &dyn DbErrorDetail) -> axum::http::StatusCode {
    e.code().map_or(
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        |code| match code {
            "42501" => axum::http::StatusCode::UNAUTHORIZED,
            "23505" | "23503" => axum::http::StatusCode::CONFLICT,
            "23502" | "23514" => axum::http::StatusCode::BAD_REQUEST,
            "42P01" | "42883" => axum::http::StatusCode::NOT_FOUND,
            c if c.starts_with("P0") => axum::http::StatusCode::BAD_REQUEST,
            c if c.starts_with("23") => axum::http::StatusCode::BAD_REQUEST,
            c if c.starts_with("22") => axum::http::StatusCode::BAD_REQUEST,
            _ => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        },
    )
}

/// Build a PostgREST-compatible JSON error body for a database error.
pub fn database_error_json(
    e: &dyn DbErrorDetail,
    status: axum::http::StatusCode,
) -> serde_json::Value {
    if let (Some(code), Some(msg)) = (e.code(), e.message()) {
        serde_json::json!({
            "code": code,
            "message": msg,
            "details": e.detail(),
            "hint": e.hint(),
        })
    } else {
        serde_json::json!({
            "code": status.as_str(),
            "message": format_database_error(e),
        })
    }
}

// ---------------------------------------------------------------------------
// Shared API error type used by all pg-rest-server backends
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ApiError {
    TableNotFound(String),
    FunctionNotFound(String),
    MethodNotAllowed,
    Unauthorized(String),
    BadRequest(String),
    QueryEngine(pg_query_engine::QueryEngineError),
    Parse(pg_query_engine::ParseError),
    Database(Box<dyn DbErrorDetail>),
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
            Self::Database(e) => write!(f, "{}", format_database_error(e.as_ref())),
            Self::Pool(msg) => write!(f, "connection pool error: {msg}"),
        }
    }
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match &self {
            Self::TableNotFound(_) => (axum::http::StatusCode::NOT_FOUND, self.to_string()),
            Self::FunctionNotFound(_) => (axum::http::StatusCode::NOT_FOUND, self.to_string()),
            Self::MethodNotAllowed => {
                (axum::http::StatusCode::METHOD_NOT_ALLOWED, self.to_string())
            }
            Self::NotAcceptable(_) => (axum::http::StatusCode::NOT_ACCEPTABLE, self.to_string()),
            Self::Unauthorized(_) => (axum::http::StatusCode::UNAUTHORIZED, self.to_string()),
            Self::BadRequest(_) | Self::Parse(_) => {
                (axum::http::StatusCode::BAD_REQUEST, self.to_string())
            }
            Self::QueryEngine(e) => match e {
                pg_query_engine::QueryEngineError::TableNotFound(_) => {
                    (axum::http::StatusCode::NOT_FOUND, self.to_string())
                }
                _ => (axum::http::StatusCode::BAD_REQUEST, self.to_string()),
            },
            Self::Database(e) => {
                let status = database_error_status(e.as_ref());
                (status, self.to_string())
            }
            Self::Pool(_) => (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                self.to_string(),
            ),
        };

        let body = if let Self::Database(e) = &self {
            database_error_json(e.as_ref(), status)
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
            [(axum::http::header::CONTENT_TYPE, "application/json")],
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

impl From<crate::handlers::HandlerError> for ApiError {
    fn from(e: crate::handlers::HandlerError) -> Self {
        match e {
            crate::handlers::HandlerError::BadRequest(msg) => Self::BadRequest(msg),
            crate::handlers::HandlerError::NotAcceptable(msg) => Self::NotAcceptable(msg),
            crate::handlers::HandlerError::Parse(e) => Self::Parse(e),
        }
    }
}

/// Blanket conversion: any type implementing `DbErrorDetail` can be converted
/// to `ApiError`. Pool-level errors become `ApiError::Pool`, others become
/// `ApiError::Database`.
impl<T: DbErrorDetail + 'static> From<T> for ApiError {
    fn from(e: T) -> Self {
        if e.is_pool_error() {
            Self::Pool(e.to_string())
        } else {
            Self::Database(Box::new(e))
        }
    }
}

impl From<pg_schema_cache::SchemaCacheError> for ApiError {
    fn from(e: pg_schema_cache::SchemaCacheError) -> Self {
        Self::BadRequest(format!("schema cache error: {e}"))
    }
}

// ---------------------------------------------------------------------------
// Implementation for tokio-postgres
// ---------------------------------------------------------------------------

#[cfg(feature = "tokio-postgres")]
impl DbErrorDetail for tokio_postgres::Error {
    fn code(&self) -> Option<&str> {
        self.as_db_error().map(|e| e.code().code())
    }

    fn message(&self) -> Option<&str> {
        self.as_db_error().map(|e| e.message())
    }

    fn detail(&self) -> Option<&str> {
        self.as_db_error().and_then(|e| e.detail())
    }

    fn hint(&self) -> Option<&str> {
        self.as_db_error().and_then(|e| e.hint())
    }

    fn is_pool_error(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Implementation for resolute
// ---------------------------------------------------------------------------

#[cfg(feature = "resolute")]
impl DbErrorDetail for resolute::TypedError {
    fn code(&self) -> Option<&str> {
        pg_error(self).map(|e| e.code.as_str())
    }

    fn message(&self) -> Option<&str> {
        pg_error(self).map(|e| e.message.as_str())
    }

    fn detail(&self) -> Option<&str> {
        pg_error(self).and_then(|e| e.detail.as_deref())
    }

    fn hint(&self) -> Option<&str> {
        pg_error(self).and_then(|e| e.hint.as_deref())
    }

    fn is_pool_error(&self) -> bool {
        matches!(self, resolute::TypedError::Pool(_))
    }
}

/// Drill into a [`resolute::TypedError`] looking for a server-reported
/// `ErrorResponse`. Returns `None` for non-PG-server errors (timeouts, decode,
/// pool, I/O, config).
#[cfg(feature = "resolute")]
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

// ---------------------------------------------------------------------------
// Implementation for pg-wired (standalone, without resolute)
// ---------------------------------------------------------------------------

#[cfg(feature = "pg-wired")]
impl DbErrorDetail for pg_wired::PgWireError {
    fn code(&self) -> Option<&str> {
        match self {
            pg_wired::PgWireError::Pg(ref e) => Some(&e.code),
            _ => None,
        }
    }

    fn message(&self) -> Option<&str> {
        match self {
            pg_wired::PgWireError::Pg(ref e) => Some(&e.message),
            _ => None,
        }
    }

    fn detail(&self) -> Option<&str> {
        match self {
            pg_wired::PgWireError::Pg(ref e) => e.detail.as_deref(),
            _ => None,
        }
    }

    fn hint(&self) -> Option<&str> {
        match self {
            pg_wired::PgWireError::Pg(ref e) => e.hint.as_deref(),
            _ => None,
        }
    }

    fn is_pool_error(&self) -> bool {
        !matches!(self, pg_wired::PgWireError::Pg(_))
    }
}
