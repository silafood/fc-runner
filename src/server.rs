use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;

use crate::metrics;

struct AppState {
    start_time: Instant,
    version: String,
}

pub async fn start(listen_addr: SocketAddr) -> anyhow::Result<()> {
    let state = Arc::new(AppState {
        start_time: Instant::now(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    });

    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/healthz", get(healthz_handler))
        .with_state(state);

    tracing::info!(%listen_addr, "starting metrics/health HTTP server");

    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn metrics_handler(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse {
    // Update uptime before gathering
    metrics::UPTIME_SECONDS
        .with_label_values(&[&state.version])
        .set(state.start_time.elapsed().as_secs_f64());

    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        metrics::gather(),
    )
}

async fn healthz_handler() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}
