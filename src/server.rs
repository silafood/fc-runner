use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::config::ServerConfig;
use crate::metrics;
use crate::pool::PoolManager;

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
    /// Pool managers indexed by name, set when running in pool mode.
    pub pools: Mutex<HashMap<String, Arc<PoolManager>>>,
}

impl ServerState {
    pub fn new(server_config: &ServerConfig) -> Self {
        Self {
            start_time: Instant::now(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            api_key: server_config.api_key.clone(),
            active_vms: Mutex::new(Vec::new()),
            mode: Mutex::new("starting".to_string()),
            pools: Mutex::new(HashMap::new()),
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

    /// Register pool managers so the API can access them.
    pub async fn set_pools(&self, pools: HashMap<String, Arc<PoolManager>>) {
        *self.pools.lock().await = pools;
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
        // Pool management endpoints
        .route("/api/v1/pools", get(list_pools_handler))
        .route("/api/v1/pools/{name}", get(get_pool_handler))
        .route("/api/v1/pools/{name}/scale", post(scale_pool_handler))
        .route("/api/v1/pools/{name}/pause", post(pause_pool_handler))
        .route("/api/v1/pools/{name}/resume", post(resume_pool_handler))
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

async fn metrics_handler(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
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

    tracing::warn!(vm_id = %vm_id, "VM kill requested via management API (not yet implemented)");

    Ok(Json(DeleteVmResponse {
        message: format!("VM {} kill requested", vm_id),
    }))
}

// ── Pool management handlers ──────────────────────────────────────────

async fn list_pools_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, StatusCode> {
    check_auth(&state, &headers)?;

    let pools = state.pools.lock().await;
    let mut statuses = Vec::new();
    for pool in pools.values() {
        statuses.push(pool.status().await);
    }
    // Sort by name for consistent output
    statuses.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(statuses))
}

async fn get_pool_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    check_auth(&state, &headers)?;

    let pools = state.pools.lock().await;
    let pool = pools.get(&name).ok_or(StatusCode::NOT_FOUND)?;
    let status = pool.status().await;
    Ok(Json(status))
}

#[derive(Deserialize)]
struct ScaleRequest {
    min_ready: Option<usize>,
    max_ready: Option<usize>,
}

#[derive(Serialize)]
struct PoolActionResponse {
    message: String,
}

async fn scale_pool_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<ScaleRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    check_auth(&state, &headers)?;

    let pools = state.pools.lock().await;
    let pool = pools.get(&name).ok_or(StatusCode::NOT_FOUND)?;
    pool.scale(body.min_ready, body.max_ready);

    Ok(Json(PoolActionResponse {
        message: format!(
            "pool '{}' scaled (min_ready: {:?}, max_ready: {:?})",
            name, body.min_ready, body.max_ready
        ),
    }))
}

async fn pause_pool_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    check_auth(&state, &headers)?;

    let pools = state.pools.lock().await;
    let pool = pools.get(&name).ok_or(StatusCode::NOT_FOUND)?;
    pool.pause();

    Ok(Json(PoolActionResponse {
        message: format!("pool '{}' paused", name),
    }))
}

async fn resume_pool_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    check_auth(&state, &headers)?;

    let pools = state.pools.lock().await;
    let pool = pools.get(&name).ok_or(StatusCode::NOT_FOUND)?;
    pool.resume();

    Ok(Json(PoolActionResponse {
        message: format!("pool '{}' resumed", name),
    }))
}
