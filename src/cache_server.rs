//! GitHub Actions Cache API service.
//!
//! Implements the `/_apis/artifactcache/` REST API that `@actions/cache`
//! (and actions like `Swatinem/rust-cache`) use to save and restore caches.
//! Blob storage is local filesystem; metadata is an in-memory index
//! persisted as JSON for crash recovery.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

// ── State ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    id: i64,
    key: String,
    version: String,
    committed: bool,
    size: i64,
    created_at: i64, // unix timestamp
}

pub struct CacheState {
    entries: RwLock<Vec<CacheEntry>>,
    /// (key, version) → entry id for fast exact-match lookup.
    key_index: RwLock<HashMap<(String, String), i64>>,
    next_id: AtomicI64,
    blob_dir: PathBuf,
    tmp_dir: PathBuf,
    index_path: PathBuf,
    token: String,
}

impl CacheState {
    pub async fn new(dir: PathBuf, token: String) -> anyhow::Result<Arc<Self>> {
        let blob_dir = dir.join("blobs");
        let tmp_dir = dir.join("tmp");
        tokio::fs::create_dir_all(&blob_dir).await?;
        tokio::fs::create_dir_all(&tmp_dir).await?;

        let index_path = dir.join("index.json");
        let entries: Vec<CacheEntry> = if index_path.exists() {
            let data = tokio::fs::read_to_string(&index_path).await?;
            serde_json::from_str(&data).unwrap_or_default()
        } else {
            Vec::new()
        };

        let next_id = entries.iter().map(|e| e.id).max().unwrap_or(0) + 1;

        let mut key_index = HashMap::new();
        for e in &entries {
            if e.committed {
                key_index.insert((e.key.clone(), e.version.clone()), e.id);
            }
        }

        let state = Arc::new(Self {
            entries: RwLock::new(entries),
            key_index: RwLock::new(key_index),
            next_id: AtomicI64::new(next_id),
            blob_dir,
            tmp_dir,
            index_path,
            token,
        });

        // Clean up stale temp files from incomplete uploads
        state.cleanup_stale_temps().await;

        tracing::info!(
            dir = %dir.display(),
            entries = state.key_index.read().await.len(),
            "cache service initialized"
        );
        Ok(state)
    }

    async fn save_index(&self) -> Result<(), StatusCode> {
        let entries = self.entries.read().await;
        let data = serde_json::to_string(&*entries).map_err(|e| {
            tracing::error!(error = %e, "failed to serialize cache index");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        let tmp = self.index_path.with_extension("tmp");
        tokio::fs::write(&tmp, data).await.map_err(|e| {
            tracing::error!(error = %e, "failed to write cache index");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        tokio::fs::rename(&tmp, &self.index_path)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "failed to rename cache index");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
        Ok(())
    }

    fn blob_path(&self, id: i64) -> PathBuf {
        self.blob_dir.join(id.to_string())
    }

    fn tmp_path(&self, id: i64) -> PathBuf {
        self.tmp_dir.join(id.to_string())
    }

    /// Remove temp files that have no matching reserved (uncommitted) entry.
    async fn cleanup_stale_temps(&self) {
        let entries = self.entries.read().await;
        let reserved: std::collections::HashSet<i64> = entries
            .iter()
            .filter(|e| !e.committed)
            .map(|e| e.id)
            .collect();
        drop(entries);

        if let Ok(mut dir) = tokio::fs::read_dir(&self.tmp_dir).await {
            while let Ok(Some(entry)) = dir.next_entry().await {
                if let Some(name) = entry.file_name().to_str()
                    && let Ok(id) = name.parse::<i64>()
                    && !reserved.contains(&id)
                {
                    let _ = tokio::fs::remove_file(entry.path()).await;
                }
            }
        }
    }
}

// ── Auth ───────────────────────────────────────────────────────────────

fn check_bearer(state: &CacheState, headers: &HeaderMap) -> Result<(), StatusCode> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if let Some(token) = auth.strip_prefix("Bearer ")
        && token == state.token
    {
        return Ok(());
    }
    Err(StatusCode::UNAUTHORIZED)
}

// ── GET /cache ─────────────────────────────────────────────────────────
// Lookup by keys (comma-separated) and version hash.
// Returns 200 with { archiveLocation, cacheKey } on hit, 204 on miss.

#[derive(Deserialize)]
struct LookupQuery {
    keys: String,
    version: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CacheHitResponse {
    archive_location: String,
    cache_key: String,
}

async fn lookup_cache(
    State(state): State<Arc<CacheState>>,
    headers: HeaderMap,
    Query(q): Query<LookupQuery>,
) -> Result<axum::response::Response, StatusCode> {
    check_bearer(&state, &headers)?;

    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost:9090");

    let entries = state.entries.read().await;
    let committed: Vec<&CacheEntry> = entries.iter().filter(|e| e.committed).collect();
    let keys: Vec<&str> = q.keys.split(',').map(|k| k.trim()).collect();

    // Exact match first (key + version)
    for key in &keys {
        if let Some(entry) = committed
            .iter()
            .find(|e| e.key == *key && e.version == q.version)
        {
            let url = format!("http://{}/_apis/artifactcache/artifacts/{}", host, entry.id);
            return Ok(Json(CacheHitResponse {
                archive_location: url,
                cache_key: entry.key.clone(),
            })
            .into_response());
        }
    }

    // Prefix match (restore keys) — pick the most recent match
    for key in &keys {
        if let Some(entry) = committed
            .iter()
            .filter(|e| e.key.starts_with(*key) && e.version == q.version)
            .max_by_key(|e| e.created_at)
        {
            let url = format!("http://{}/_apis/artifactcache/artifacts/{}", host, entry.id);
            return Ok(Json(CacheHitResponse {
                archive_location: url,
                cache_key: entry.key.clone(),
            })
            .into_response());
        }
    }

    Ok(StatusCode::NO_CONTENT.into_response())
}

// ── POST /caches ───────────────────────────────────────────────────────
// Reserve a cache entry; returns { cacheId }.

#[derive(Deserialize)]
struct ReserveRequest {
    key: String,
    version: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReserveResponse {
    cache_id: i64,
}

async fn reserve_cache(
    State(state): State<Arc<CacheState>>,
    headers: HeaderMap,
    Json(body): Json<ReserveRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    check_bearer(&state, &headers)?;

    // Reject if exact key+version already committed
    {
        let idx = state.key_index.read().await;
        if idx.contains_key(&(body.key.clone(), body.version.clone())) {
            return Err(StatusCode::CONFLICT);
        }
    }

    let id = state.next_id.fetch_add(1, Ordering::SeqCst);
    let entry = CacheEntry {
        id,
        key: body.key,
        version: body.version,
        committed: false,
        size: 0,
        created_at: chrono::Utc::now().timestamp(),
    };

    // Create empty temp file for chunk uploads
    tokio::fs::File::create(state.tmp_path(id))
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "failed to create temp file for cache upload");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    state.entries.write().await.push(entry);
    state.save_index().await?;

    tracing::debug!(cache_id = id, "cache entry reserved");
    Ok((StatusCode::OK, Json(ReserveResponse { cache_id: id })))
}

// ── PATCH /caches/:id ──────────────────────────────────────────────────
// Chunked upload with Content-Range header.

async fn upload_chunk(
    State(state): State<Arc<CacheState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, StatusCode> {
    check_bearer(&state, &headers)?;

    let range = headers
        .get("content-range")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let start = parse_content_range_start(range).ok_or(StatusCode::BAD_REQUEST)?;

    let tmp_path = state.tmp_path(id);
    if !tmp_path.exists() {
        return Err(StatusCode::NOT_FOUND);
    }

    use tokio::io::{AsyncSeekExt, AsyncWriteExt};
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&tmp_path)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, cache_id = id, "failed to open temp file");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    file.seek(std::io::SeekFrom::Start(start))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    file.write_all(&body)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(StatusCode::NO_CONTENT)
}

/// Parse the start offset from a Content-Range header.
/// Formats: `bytes 0-1023/*`, `bytes 0-1023/2048`
fn parse_content_range_start(range: &str) -> Option<u64> {
    let rest = range.strip_prefix("bytes ")?;
    let dash = rest.find('-')?;
    rest[..dash].parse().ok()
}

// ── POST /caches/:id ──────────────────────────────────────────────────
// Commit a cache entry after all chunks are uploaded.

#[derive(Deserialize)]
struct CommitRequest {
    size: i64,
}

async fn commit_cache(
    State(state): State<Arc<CacheState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(body): Json<CommitRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    check_bearer(&state, &headers)?;

    let tmp_path = state.tmp_path(id);
    let blob_path = state.blob_path(id);

    if !tmp_path.exists() {
        return Err(StatusCode::NOT_FOUND);
    }

    // Move temp to final location
    tokio::fs::rename(&tmp_path, &blob_path)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, cache_id = id, "failed to finalize cache blob");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Mark as committed and update key index
    let (key, version) = {
        let mut entries = state.entries.write().await;
        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
            entry.committed = true;
            entry.size = body.size;
            (entry.key.clone(), entry.version.clone())
        } else {
            return Err(StatusCode::NOT_FOUND);
        }
    };

    state
        .key_index
        .write()
        .await
        .insert((key.clone(), version), id);
    state.save_index().await?;

    tracing::info!(
        cache_id = id,
        key = %key,
        size_mb = body.size / 1_048_576,
        "cache entry committed"
    );
    Ok(StatusCode::NO_CONTENT)
}

// ── GET /artifacts/:id ─────────────────────────────────────────────────
// Stream the cached archive to the client.

async fn get_artifact(
    State(state): State<Arc<CacheState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, StatusCode> {
    check_bearer(&state, &headers)?;

    let blob_path = state.blob_path(id);
    if !blob_path.exists() {
        return Err(StatusCode::NOT_FOUND);
    }

    let meta = tokio::fs::metadata(&blob_path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let file = tokio::fs::File::open(&blob_path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let stream = tokio_util::io::ReaderStream::new(file);
    let body = Body::from_stream(stream);

    Ok(axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .header("content-length", meta.len())
        .body(body)
        .unwrap())
}

// ── Router ─────────────────────────────────────────────────────────────

pub fn router(state: Arc<CacheState>) -> Router {
    Router::new()
        .route("/_apis/artifactcache/cache", get(lookup_cache))
        .route("/_apis/artifactcache/caches", post(reserve_cache))
        .route(
            "/_apis/artifactcache/caches/{id}",
            patch(upload_chunk).post(commit_cache),
        )
        .route("/_apis/artifactcache/artifacts/{id}", get(get_artifact))
        .with_state(state)
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn test_state() -> Arc<CacheState> {
        let dir = tempfile::tempdir().unwrap();
        CacheState::new(dir.keep(), "test-token".to_string())
            .await
            .unwrap()
    }

    fn auth_header() -> (&'static str, &'static str) {
        ("authorization", "Bearer test-token")
    }

    fn app(state: Arc<CacheState>) -> Router {
        router(state)
    }

    #[tokio::test]
    async fn cache_miss_returns_204() {
        let state = test_state().await;
        let resp = app(state)
            .oneshot(
                Request::get("/_apis/artifactcache/cache?keys=nonexistent&version=abc")
                    .header(auth_header().0, auth_header().1)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn unauthorized_without_token() {
        let state = test_state().await;
        let resp = app(state)
            .oneshot(
                Request::get("/_apis/artifactcache/cache?keys=k&version=v")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn full_save_restore_cycle() {
        let state = test_state().await;

        // 1. Reserve
        let resp = app(state.clone())
            .oneshot(
                Request::post("/_apis/artifactcache/caches")
                    .header(auth_header().0, auth_header().1)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({
                            "key": "rust-cache-linux-x64-abc123",
                            "version": "v1hash"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 10000).await.unwrap();
        let reserve: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let cache_id = reserve["cacheId"].as_i64().unwrap();
        assert!(cache_id > 0);

        // 2. Upload chunk
        let data = b"fake-archive-data-for-testing";
        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/_apis/artifactcache/caches/{}", cache_id))
                    .header(auth_header().0, auth_header().1)
                    .header("content-range", format!("bytes 0-{}/*", data.len() - 1))
                    .header("content-type", "application/octet-stream")
                    .body(Body::from(data.to_vec()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // 3. Commit
        let resp = app(state.clone())
            .oneshot(
                Request::post(format!("/_apis/artifactcache/caches/{}", cache_id))
                    .header(auth_header().0, auth_header().1)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({
                            "size": data.len()
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // 4. Lookup — exact match
        let resp = app(state.clone())
            .oneshot(
                Request::get(
                    "/_apis/artifactcache/cache?keys=rust-cache-linux-x64-abc123&version=v1hash",
                )
                .header(auth_header().0, auth_header().1)
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 10000).await.unwrap();
        let hit: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(hit["cacheKey"], "rust-cache-linux-x64-abc123");
        assert!(
            hit["archiveLocation"]
                .as_str()
                .unwrap()
                .contains("artifacts")
        );

        // 5. Lookup — prefix match
        let resp = app(state.clone())
            .oneshot(
                Request::get("/_apis/artifactcache/cache?keys=rust-cache-linux-x64&version=v1hash")
                    .header(auth_header().0, auth_header().1)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // 6. Download artifact
        let archive_url = hit["archiveLocation"].as_str().unwrap();
        let path = archive_url
            .split("://")
            .nth(1)
            .and_then(|s| s.find('/').map(|i| &s[i..]))
            .unwrap();
        let resp = app(state.clone())
            .oneshot(
                Request::get(path)
                    .header(auth_header().0, auth_header().1)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 100000)
            .await
            .unwrap();
        assert_eq!(&body[..], data);
    }

    #[tokio::test]
    async fn duplicate_key_returns_conflict() {
        let state = test_state().await;

        // Reserve first
        let resp = app(state.clone())
            .oneshot(
                Request::post("/_apis/artifactcache/caches")
                    .header(auth_header().0, auth_header().1)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"key":"k","version":"v"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 10000).await.unwrap();
        let id = serde_json::from_slice::<serde_json::Value>(&body).unwrap()["cacheId"]
            .as_i64()
            .unwrap();

        // Commit it
        let _ = app(state.clone())
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/_apis/artifactcache/caches/{}", id))
                    .header(auth_header().0, auth_header().1)
                    .header("content-range", "bytes 0-0/*")
                    .header("content-type", "application/octet-stream")
                    .body(Body::from(vec![0u8]))
                    .unwrap(),
            )
            .await
            .unwrap();
        let _ = app(state.clone())
            .oneshot(
                Request::post(format!("/_apis/artifactcache/caches/{}", id))
                    .header(auth_header().0, auth_header().1)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"size":1}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Try to reserve again — should get 409
        let resp = app(state)
            .oneshot(
                Request::post("/_apis/artifactcache/caches")
                    .header(auth_header().0, auth_header().1)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"key":"k","version":"v"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn version_mismatch_returns_miss() {
        let state = test_state().await;

        // Reserve + commit with version "v1"
        let resp = app(state.clone())
            .oneshot(
                Request::post("/_apis/artifactcache/caches")
                    .header(auth_header().0, auth_header().1)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"key":"k","version":"v1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 10000).await.unwrap();
        let id = serde_json::from_slice::<serde_json::Value>(&body).unwrap()["cacheId"]
            .as_i64()
            .unwrap();
        let _ = app(state.clone())
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/_apis/artifactcache/caches/{}", id))
                    .header(auth_header().0, auth_header().1)
                    .header("content-range", "bytes 0-0/*")
                    .body(Body::from(vec![0u8]))
                    .unwrap(),
            )
            .await
            .unwrap();
        let _ = app(state.clone())
            .oneshot(
                Request::post(format!("/_apis/artifactcache/caches/{}", id))
                    .header(auth_header().0, auth_header().1)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"size":1}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Lookup with different version — should miss
        let resp = app(state)
            .oneshot(
                Request::get("/_apis/artifactcache/cache?keys=k&version=v2")
                    .header(auth_header().0, auth_header().1)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn upload_to_nonexistent_cache_returns_404() {
        let state = test_state().await;
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/_apis/artifactcache/caches/99999")
                    .header(auth_header().0, auth_header().1)
                    .header("content-range", "bytes 0-3/*")
                    .body(Body::from(vec![1, 2, 3, 4]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn artifact_not_found() {
        let state = test_state().await;
        let resp = app(state)
            .oneshot(
                Request::get("/_apis/artifactcache/artifacts/99999")
                    .header(auth_header().0, auth_header().1)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn parse_content_range_variants() {
        assert_eq!(parse_content_range_start("bytes 0-1023/*"), Some(0));
        assert_eq!(
            parse_content_range_start("bytes 1024-2047/4096"),
            Some(1024)
        );
        assert_eq!(parse_content_range_start("bytes 0-0/*"), Some(0));
        assert_eq!(parse_content_range_start("invalid"), None);
        assert_eq!(parse_content_range_start(""), None);
    }
}
