pub mod backend;

pub use pg_rest_server_common::error::ApiError;
pub use pg_rest_server_common::handlers::{
    handle_delete, handle_insert, handle_live, handle_metrics, handle_read, handle_ready,
    handle_reload, handle_root, handle_rpc, handle_update, handle_ws,
};

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;

use crate::backend::DeadpoolBackend;
use pg_rest_server_common::state::AppState;

pub fn build_router(state: Arc<AppState<DeadpoolBackend>>) -> Router {
    let app = Router::new()
        .route("/", get(handle_root::<DeadpoolBackend>))
        .route("/live", get(handle_live))
        .route("/ready", get(handle_ready::<DeadpoolBackend>))
        .route("/metrics", get(handle_metrics::<DeadpoolBackend>))
        .route("/reload", post(handle_reload::<DeadpoolBackend>))
        .route("/ws", get(handle_ws::<DeadpoolBackend>))
        .route(
            "/rpc/{function}",
            get(handle_rpc::<DeadpoolBackend>).post(handle_rpc::<DeadpoolBackend>),
        )
        .route(
            "/{table}",
            get(handle_read::<DeadpoolBackend>)
                .post(handle_insert::<DeadpoolBackend>)
                .patch(handle_update::<DeadpoolBackend>)
                .delete(handle_delete::<DeadpoolBackend>),
        );

    pg_rest_server_common::router::apply_server_middleware(app, &state.config.server)
        .with_state(state)
}

fn _assert_sync_send() {
    fn assert_sync<T: Sync>() {}
    fn assert_send<T: Send>() {}
    assert_sync::<crate::backend::DeadpoolBackend>();
    assert_send::<crate::backend::DeadpoolBackend>();
}
