use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::config::ServerConfig;
use crate::metrics;

// ── Shared state ───────────────────────────────────────────────────────

/// Active VM info tracked by the orchestrator.
#[derive(Clone, Serialize)]
pub struct VmInfo {
    pub vm_id: String,
    pub job_id: u64,
    pub repo: String,
    pub slot: usize,
    pub started_at: String,
}

/// Shared state between the server and orchestrator.
pub struct ServerState {
    pub start_time: Instant,
    pub version: String,
    pub api_key: Option<String>,
    pub active_vms: Mutex<Vec<VmInfo>>,
    pub mode: Mutex<String>,
}

impl ServerState {
    pub fn new(server_config: &ServerConfig) -> Self {
        Self {
            start_time: Instant::now(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            api_key: server_config.api_key.clone(),
            active_vms: Mutex::new(Vec::new()),
            mode: Mutex::new("starting".to_string()),
        }
    }

    pub async fn register_vm(&self, info: VmInfo) {
        self.active_vms.lock().await.push(info);
    }

    pub async fn unregister_vm(&self, vm_id: &str) {
        self.active_vms.lock().await.retain(|v| v.vm_id != vm_id);
    }

    pub async fn set_mode(&self, mode: &str) {
        *self.mode.lock().await = mode.to_string();
    }
}

// ── Server ─────────────────────────────────────────────────────────────

pub async fn start(listen_addr: SocketAddr, state: Arc<ServerState>) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/healthz", get(healthz_handler))
        .route("/api/v1/status", get(status_handler))
        .route("/api/v1/vms", get(list_vms_handler))
        .route("/api/v1/vms/{id}", delete(delete_vm_handler))
        .with_state(state);

    tracing::info!(%listen_addr, "starting management HTTP server");

    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

// ── Auth middleware helper ──────────────────────────────────────────────

fn check_auth(state: &ServerState, headers: &HeaderMap) -> Result<(), StatusCode> {
    if let Some(expected_key) = &state.api_key {
        let provided = headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if provided != expected_key {
            return Err(StatusCode::UNAUTHORIZED);
        }
    }
    Ok(())
}

// ── Handlers ───────────────────────────────────────────────────────────

async fn metrics_handler(
    State(state): State<Arc<ServerState>>,
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

#[derive(Serialize)]
struct StatusResponse {
    version: String,
    uptime_seconds: u64,
    mode: String,
    active_vms: usize,
}

async fn status_handler(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    let resp = StatusResponse {
        version: state.version.clone(),
        uptime_seconds: state.start_time.elapsed().as_secs(),
        mode: state.mode.lock().await.clone(),
        active_vms: state.active_vms.lock().await.len(),
    };
    Json(resp)
}

async fn list_vms_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, StatusCode> {
    check_auth(&state, &headers)?;
    let vms = state.active_vms.lock().await.clone();
    Ok(Json(vms))
}

#[derive(Serialize)]
struct DeleteVmResponse {
    message: String,
}

async fn delete_vm_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Path(vm_id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    check_auth(&state, &headers)?;

    let vms = state.active_vms.lock().await;
    if !vms.iter().any(|v| v.vm_id == vm_id) {
        return Err(StatusCode::NOT_FOUND);
    }
    drop(vms);

    // Note: Actually killing the VM process requires integration with the
    // orchestrator. For now, we log the request. Full kill support will be
    // added when the pool manager tracks child process handles.
    tracing::warn!(vm_id = %vm_id, "VM kill requested via management API (not yet implemented)");

    Ok(Json(DeleteVmResponse {
        message: format!("VM {} kill requested", vm_id),
    }))
}
