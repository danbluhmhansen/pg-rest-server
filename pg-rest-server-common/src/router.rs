use axum::Router;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use crate::config::ServerConfig;

/// Build a CORS layer from the configured origins.
///
/// An empty list or `["*"]` produces a permissive layer. Otherwise the
/// provided origins are used with `AllowOrigin::list`.
pub fn build_cors(origins: &[String]) -> CorsLayer {
    if origins.is_empty() || (origins.len() == 1 && origins[0] == "*") {
        return CorsLayer::permissive();
    }

    let allowed: Vec<axum::http::HeaderValue> =
        origins.iter().filter_map(|o| o.parse().ok()).collect();

    CorsLayer::new()
        .allow_origin(AllowOrigin::list(allowed))
        .allow_methods(tower_http::cors::Any)
        .allow_headers(tower_http::cors::Any)
}

/// Apply the standard middleware stack to a router: body limits, tracing,
/// CORS, and optional rate limiting.
///
/// The `PG_REST_LEAN` env var disables the body-limit / tracing / CORS
/// middleware for reduced overhead.
pub fn apply_server_middleware<S: Clone + Send + Sync + 'static>(
    app: Router<S>,
    config: &ServerConfig,
) -> Router<S> {
    let cors = build_cors(&config.cors_origins);

    let mut app = app;

    if std::env::var("PG_REST_LEAN").is_err() {
        // Full middleware stack: body limits, tracing, CORS.
        // These layers clone per-connection — adds overhead at high throughput.
        app = app
            .layer(RequestBodyLimitLayer::new(config.body_limit))
            .layer(TraceLayer::new_for_http())
            .layer(cors);
    }

    // Rate limiting (requests/sec, 0 = unlimited).
    // Applied via ConcurrencyLimit as a simpler alternative that's Clone-compatible.
    if config.rate_limit > 0 {
        app = app.layer(ConcurrencyLimitLayer::new(config.rate_limit as usize));
    }

    app
}
