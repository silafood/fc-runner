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

use crate::cache_server::CacheState;
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

pub async fn start(
    listen_addr: SocketAddr,
    state: Arc<ServerState>,
    cache_state: Option<Arc<CacheState>>,
) -> anyhow::Result<()> {
    let mut app = Router::new()
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

    // Merge cache service routes if enabled
    if let Some(cs) = cache_state {
        app = app.merge(crate::cache_server::router(cs));
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_state() -> Arc<ServerState> {
        Arc::new(ServerState::new(&ServerConfig {
            listen_addr: "127.0.0.1:0".to_string(),
            enabled: true,
            api_key: None,
        }))
    }

    fn test_state_with_key(key: &str) -> Arc<ServerState> {
        Arc::new(ServerState::new(&ServerConfig {
            listen_addr: "127.0.0.1:0".to_string(),
            enabled: true,
            api_key: Some(key.to_string()),
        }))
    }

    fn app(state: Arc<ServerState>) -> Router {
        Router::new()
            .route("/metrics", get(metrics_handler))
            .route("/healthz", get(healthz_handler))
            .route("/api/v1/status", get(status_handler))
            .route("/api/v1/vms", get(list_vms_handler))
            .route("/api/v1/vms/{id}", delete(delete_vm_handler))
            .route("/api/v1/pools", get(list_pools_handler))
            .route("/api/v1/pools/{name}", get(get_pool_handler))
            .route("/api/v1/pools/{name}/scale", post(scale_pool_handler))
            .route("/api/v1/pools/{name}/pause", post(pause_pool_handler))
            .route("/api/v1/pools/{name}/resume", post(resume_pool_handler))
            .with_state(state)
    }

    #[allow(dead_code)]
    fn app_with_cache(state: Arc<ServerState>, cache: Arc<CacheState>) -> Router {
        app(state).merge(crate::cache_server::router(cache))
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        let state = test_state();
        let resp = app(state)
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn status_returns_json() {
        let state = test_state();
        state.set_mode("jit").await;
        let resp = app(state)
            .oneshot(Request::get("/api/v1/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 10000).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["mode"], "jit");
        assert_eq!(json["active_vms"], 0);
        assert!(json["version"].is_string());
    }

    #[tokio::test]
    async fn list_vms_empty() {
        let state = test_state();
        let resp = app(state)
            .oneshot(Request::get("/api/v1/vms").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 10000).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_vms_with_registered_vm() {
        let state = test_state();
        state
            .register_vm(VmInfo {
                vm_id: "fc-100-slot0".to_string(),
                job_id: 100,
                repo: "test-repo".to_string(),
                slot: 0,
                started_at: "1234567890".to_string(),
            })
            .await;
        let resp = app(state)
            .oneshot(Request::get("/api/v1/vms").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 10000).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let vms = json.as_array().unwrap();
        assert_eq!(vms.len(), 1);
        assert_eq!(vms[0]["vm_id"], "fc-100-slot0");
        assert_eq!(vms[0]["job_id"], 100);
        assert_eq!(vms[0]["repo"], "test-repo");
    }

    #[tokio::test]
    async fn unregister_vm_removes_it() {
        let state = test_state();
        state
            .register_vm(VmInfo {
                vm_id: "fc-100-slot0".to_string(),
                job_id: 100,
                repo: "test-repo".to_string(),
                slot: 0,
                started_at: "1234567890".to_string(),
            })
            .await;
        state.unregister_vm("fc-100-slot0").await;
        assert!(state.active_vms.lock().await.is_empty());
    }

    #[tokio::test]
    async fn delete_vm_not_found() {
        let state = test_state();
        let resp = app(state)
            .oneshot(
                Request::delete("/api/v1/vms/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_vm_found() {
        let state = test_state();
        state
            .register_vm(VmInfo {
                vm_id: "fc-200-slot1".to_string(),
                job_id: 200,
                repo: "r".to_string(),
                slot: 1,
                started_at: "0".to_string(),
            })
            .await;
        let resp = app(state)
            .oneshot(
                Request::delete("/api/v1/vms/fc-200-slot1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn api_key_auth_rejects_missing_key() {
        let state = test_state_with_key("secret");
        let resp = app(state)
            .oneshot(Request::get("/api/v1/vms").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn api_key_auth_rejects_wrong_key() {
        let state = test_state_with_key("secret");
        let resp = app(state)
            .oneshot(
                Request::get("/api/v1/vms")
                    .header("x-api-key", "wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn api_key_auth_accepts_correct_key() {
        let state = test_state_with_key("secret");
        let resp = app(state)
            .oneshot(
                Request::get("/api/v1/vms")
                    .header("x-api-key", "secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn healthz_no_auth_required() {
        let state = test_state_with_key("secret");
        let resp = app(state)
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn status_no_auth_required() {
        let state = test_state_with_key("secret");
        let resp = app(state)
            .oneshot(Request::get("/api/v1/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn list_pools_empty() {
        let state = test_state();
        let resp = app(state)
            .oneshot(Request::get("/api/v1/pools").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 10000).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn get_pool_not_found() {
        let state = test_state();
        let resp = app(state)
            .oneshot(
                Request::get("/api/v1/pools/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn set_mode_updates_status() {
        let state = test_state();
        assert_eq!(*state.mode.lock().await, "starting");
        state.set_mode("pools").await;
        assert_eq!(*state.mode.lock().await, "pools");
    }

    #[tokio::test]
    async fn metrics_endpoint_returns_text() {
        let state = test_state();
        let resp = app(state)
            .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.contains("text/plain"));
    }

    #[tokio::test]
    async fn pools_auth_required_with_key() {
        let state = test_state_with_key("secret");
        let resp = app(state)
            .oneshot(Request::get("/api/v1/pools").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
