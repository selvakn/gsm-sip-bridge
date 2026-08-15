pub mod events;
pub mod handlers;
pub mod state;

use axum::routing::get;
use axum::routing::post;
use axum::Router;

use state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(handlers::health))
        .route("/status", get(handlers::status))
        .route(
            "/calls",
            post(handlers::place_call).get(handlers::list_calls),
        )
        .route("/calls/{id}", get(handlers::get_call))
        .route("/calls/{id}/answer", post(handlers::answer_call))
        .route("/calls/{id}/reject", post(handlers::reject_call))
        .route(
            "/policy/inbound",
            get(handlers::get_inbound_policy).put(handlers::put_inbound_policy),
        )
        .route("/events", get(handlers::events))
        .with_state(state)
}
