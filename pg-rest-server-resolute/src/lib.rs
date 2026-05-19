#![allow(clippy::result_large_err)]

pub mod auth;
pub mod error;
pub mod handlers;
pub mod state;

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use handlers::*;
use state::AppState;

/// Build the Axum router with all routes and middleware.
pub fn build_router(state: Arc<AppState>) -> Router {
    let cors = build_cors(&state.config.server.cors_origins);

    let mut app = Router::new()
        .route("/", get(handle_root))
        .route("/live", get(handle_live))
        .route("/ready", get(handle_ready))
        .route("/metrics", get(handle_metrics))
        .route("/reload", post(handle_reload))
        .route("/ws", get(handle_ws))
        .route("/rpc/{function}", get(handle_rpc).post(handle_rpc))
        .route(
            "/{table}",
            get(handle_read)
                .post(handle_insert)
                .patch(handle_update)
                .delete(handle_delete),
        );

    if std::env::var("PG_REST_LEAN").is_err() {
        // Full middleware stack: body limits, tracing, CORS.
        // These layers clone per-connection — adds overhead at high throughput.
        app = app
            .layer(RequestBodyLimitLayer::new(state.config.server.body_limit))
            .layer(TraceLayer::new_for_http())
            .layer(cors);
    }

    // Rate limiting (requests/sec, 0 = unlimited).
    // Applied via ConcurrencyLimit as a simpler alternative that's Clone-compatible.
    if state.config.server.rate_limit > 0 {
        app = app.layer(tower::limit::ConcurrencyLimitLayer::new(
            state.config.server.rate_limit as usize,
        ));
    }

    app.with_state(state)
}

fn build_cors(origins: &[String]) -> CorsLayer {
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
