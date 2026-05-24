pub mod backend;
pub mod handlers;

pub use pg_rest_server_common::error::ApiError;
pub use pg_rest_server_common::handlers::{
    handle_delete, handle_insert, handle_live, handle_metrics, handle_read, handle_ready,
    handle_root, handle_rpc, handle_update,
};

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;

use crate::backend::ResoluteBackend;
use crate::handlers::{handle_reload, handle_ws};
use pg_rest_server_common::state::AppState;

pub fn build_router(state: Arc<AppState<ResoluteBackend>>) -> Router {
    let app = Router::new()
        .route("/", get(handle_root::<ResoluteBackend>))
        .route("/live", get(handle_live))
        .route("/ready", get(handle_ready::<ResoluteBackend>))
        .route("/metrics", get(handle_metrics::<ResoluteBackend>))
        .route("/reload", post(handle_reload))
        .route("/ws", get(handle_ws))
        .route(
            "/rpc/{function}",
            get(handle_rpc::<ResoluteBackend>).post(handle_rpc::<ResoluteBackend>),
        )
        .route(
            "/{table}",
            get(handle_read::<ResoluteBackend>)
                .post(handle_insert::<ResoluteBackend>)
                .patch(handle_update::<ResoluteBackend>)
                .delete(handle_delete::<ResoluteBackend>),
        );

    pg_rest_server_common::router::apply_server_middleware(app, &state.config.server)
        .with_state(state)
}
