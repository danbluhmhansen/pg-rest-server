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

use crate::backend::PgWiredBackend;
use crate::handlers::{handle_reload, handle_ws};
use pg_rest_server_common::state::AppState;

pub fn build_router(state: Arc<AppState<PgWiredBackend>>) -> Router {
    let app = Router::new()
        .route("/", get(handle_root::<PgWiredBackend>))
        .route("/live", get(handle_live))
        .route("/ready", get(handle_ready::<PgWiredBackend>))
        .route("/metrics", get(handle_metrics::<PgWiredBackend>))
        .route("/reload", post(handle_reload))
        .route("/ws", get(handle_ws))
        .route(
            "/rpc/{function}",
            get(handle_rpc::<PgWiredBackend>).post(handle_rpc::<PgWiredBackend>),
        )
        .route(
            "/{table}",
            get(handle_read::<PgWiredBackend>)
                .post(handle_insert::<PgWiredBackend>)
                .patch(handle_update::<PgWiredBackend>)
                .delete(handle_delete::<PgWiredBackend>),
        );

    pg_rest_server_common::router::apply_server_middleware(app, &state.config.server)
        .with_state(state)
}
