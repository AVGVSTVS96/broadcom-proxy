use std::sync::Arc;

use axum::{
    Router,
    http::StatusCode,
    routing::{get, post},
};

pub mod accounts;
pub mod app;
pub mod config;
pub mod files;
pub mod forward;
pub mod live;
pub mod manage;
pub mod oauth;
pub mod pool;
pub mod sampler;

/// Everything the proxy answers: the Claude API, Claude Code's reachability probe, and management.
pub fn router(app: Arc<app::App>) -> Router {
    Router::new()
        .route("/v1/messages", post(forward::messages))
        .route("/v1/messages/count_tokens", post(forward::count_tokens))
        .route("/v1/models", get(forward::models))
        .route("/v1/models/{model}", get(forward::models))
        .route("/api/hello", get(|| async { StatusCode::OK }))
        .route("/healthz", get(|| async { StatusCode::OK }))
        .nest("/v8/management", manage::router(app.clone()))
        .with_state(app)
}
