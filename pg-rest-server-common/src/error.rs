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
