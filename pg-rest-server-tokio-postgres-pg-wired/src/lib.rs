pub mod auth;
pub mod error;
pub mod handlers;
pub mod state;

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;

use handlers::*;
use state::AppState;

/// Build the Axum router with all routes and middleware.
pub fn build_router(state: Arc<AppState>) -> Router {
    let app = Router::new()
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

    pg_rest_server_common::router::apply_server_middleware(app, &state.config.server)
        .with_state(state)
}
