//! GitHub Actions Cache API service.
//!
//! Implements the `/_apis/artifactcache/` REST API that `@actions/cache`
//! (and actions like `Swatinem/rust-cache`) use to save and restore caches.
//! Blob storage is delegated to an S3-compatible backend (RustFS/MinIO).
//! Metadata is an in-memory index persisted as JSON for crash recovery.
//! Chunk uploads are assembled in local temp files, then uploaded to S3 on commit.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use anyhow::Context;
use aws_sdk_s3::Client as S3Client;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::config::CacheServiceConfig;

// ── State ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    id: i64,
    key: String,
    version: String,
    committed: bool,
    size: i64,
    s3_key: Option<String>,
    created_at: i64, // unix timestamp
}

pub struct CacheState {
    entries: RwLock<Vec<CacheEntry>>,
    /// (key, version) → entry id for fast exact-match lookup.
    key_index: RwLock<HashMap<(String, String), i64>>,
    next_id: AtomicI64,
    tmp_dir: PathBuf,
    index_path: PathBuf,
    token: String,
    s3: S3Client,
    s3_bucket: String,
}

impl CacheState {
    pub async fn new(config: &CacheServiceConfig, token: String) -> anyhow::Result<Arc<Self>> {
        let dir = PathBuf::from(&config.dir);
        let tmp_dir = dir.join("tmp");
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

        // Build S3 client for RustFS/MinIO
        let s3 = build_s3_client(config).await;

        // Verify S3 backend is reachable before proceeding
        check_s3_connectivity(&s3, &config.s3_endpoint).await?;

        // Ensure bucket exists
        ensure_bucket(&s3, &config.s3_bucket)
            .await
            .with_context(|| {
                format!(
                    "failed to ensure S3 bucket '{}' at {}",
                    config.s3_bucket, config.s3_endpoint
                )
            })?;

        let state = Arc::new(Self {
            entries: RwLock::new(entries),
            key_index: RwLock::new(key_index),
            next_id: AtomicI64::new(next_id),
            tmp_dir,
            index_path,
            token,
            s3,
            s3_bucket: config.s3_bucket.clone(),
        });

        // Clean up stale temp files from incomplete uploads
        state.cleanup_stale_temps().await;

        tracing::info!(
            dir = %config.dir,
            bucket = %config.s3_bucket,
            endpoint = %config.s3_endpoint,
            entries = state.key_index.read().await.len(),
            "cache service initialized (S3 backend)"
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

    fn s3_key(id: i64) -> String {
        format!("cache/{}", id)
    }

    fn tmp_path(&self, id: i64) -> PathBuf {
        self.tmp_dir.join(id.to_string())
    }

    /// Upload a local file to S3.
    async fn upload_to_s3(&self, id: i64, local_path: &std::path::Path) -> Result<(), StatusCode> {
        let body = aws_sdk_s3::primitives::ByteStream::from_path(local_path)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, cache_id = id, "failed to read file for S3 upload");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

        self.s3
            .put_object()
            .bucket(&self.s3_bucket)
            .key(Self::s3_key(id))
            .body(body)
            .send()
            .await
            .map_err(|e| {
                tracing::error!(error = %e, cache_id = id, "S3 upload failed");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

        Ok(())
    }

    /// Stream an object from S3.
    async fn get_from_s3(
        &self,
        id: i64,
    ) -> Result<(aws_sdk_s3::primitives::ByteStream, i64), StatusCode> {
        let resp = self
            .s3
            .get_object()
            .bucket(&self.s3_bucket)
            .key(Self::s3_key(id))
            .send()
            .await
            .map_err(|e| {
                tracing::error!(error = %e, cache_id = id, "S3 download failed");
                StatusCode::NOT_FOUND
            })?;

        let size = resp.content_length().unwrap_or(0);
        Ok((resp.body, size))
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

/// Build an S3 client configured for a custom endpoint (RustFS/MinIO).
async fn build_s3_client(config: &CacheServiceConfig) -> S3Client {
    let creds = aws_sdk_s3::config::Credentials::new(
        config.s3_access_key.as_deref().unwrap_or("minioadmin"),
        config.s3_secret_key.as_deref().unwrap_or("minioadmin"),
        None,
        None,
        "fc-runner-cache",
    );

    let s3_config = aws_sdk_s3::config::Builder::new()
        .endpoint_url(&config.s3_endpoint)
        .region(aws_sdk_s3::config::Region::new(config.s3_region.clone()))
        .credentials_provider(creds)
        .force_path_style(true)
        .behavior_version_latest()
        .build();

    S3Client::from_conf(s3_config)
}

/// Verify S3 backend is reachable by listing buckets.
async fn check_s3_connectivity(s3: &S3Client, endpoint: &str) -> anyhow::Result<()> {
    s3.list_buckets()
        .send()
        .await
        .with_context(|| format!("cache service S3 backend is not reachable at {endpoint}"))?;
    Ok(())
}

/// Create the bucket if it doesn't exist.
async fn ensure_bucket(s3: &S3Client, bucket: &str) -> anyhow::Result<()> {
    match s3.head_bucket().bucket(bucket).send().await {
        Ok(_) => Ok(()),
        Err(_) => {
            s3.create_bucket().bucket(bucket).send().await?;
            tracing::info!(bucket, "created S3 bucket");
            Ok(())
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

    tracing::info!(keys = %q.keys, version = %q.version, "cache lookup request");

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
            tracing::info!(key = %entry.key, cache_id = entry.id, "cache HIT (exact)");
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
            tracing::info!(key = %entry.key, cache_id = entry.id, match_prefix = %key, "cache HIT (prefix)");
            return Ok(Json(CacheHitResponse {
                archive_location: url,
                cache_key: entry.key.clone(),
            })
            .into_response());
        }
    }

    tracing::info!(keys = %q.keys, "cache MISS");
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

    tracing::info!(key = %body.key, version = %body.version, "cache reserve request");

    // If the exact key+version is already committed, remove the old entry
    // so the new save can proceed. This matches GitHub's hosted cache behavior
    // where saving the same key overwrites the previous entry.
    {
        let mut idx = state.key_index.write().await;
        if let Some(old_id) = idx.remove(&(body.key.clone(), body.version.clone())) {
            tracing::info!(
                key = %body.key,
                old_id,
                "replacing existing cache entry"
            );
            let mut entries = state.entries.write().await;
            entries.retain(|e| e.id != old_id);
        }
    }

    let id = state.next_id.fetch_add(1, Ordering::SeqCst);
    let entry = CacheEntry {
        id,
        key: body.key,
        version: body.version,
        committed: false,
        size: 0,
        s3_key: None,
        created_at: chrono::Utc::now().timestamp(),
    };

    // Create empty temp file for chunk uploads
    tokio::fs::File::create(state.tmp_path(id))
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "failed to create temp file for cache upload");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let key_for_log = entry.key.clone();
    state.entries.write().await.push(entry);
    state.save_index().await?;

    tracing::info!(cache_id = id, key = %key_for_log, "cache entry reserved");
    Ok((StatusCode::OK, Json(ReserveResponse { cache_id: id })))
}

// ── PATCH /caches/:id ──────────────────────────────────────────────────
// Chunked upload with Content-Range header.
// Chunks are written to a local temp file, then uploaded to S3 on commit.

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

    tracing::info!(cache_id = id, range = %range, bytes = body.len(), "cache chunk upload");

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
// Commit a cache entry: upload assembled temp file to S3, then clean up.

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
    if !tmp_path.exists() {
        return Err(StatusCode::NOT_FOUND);
    }

    // Upload assembled file to S3
    state.upload_to_s3(id, &tmp_path).await?;

    // Remove local temp file
    let _ = tokio::fs::remove_file(&tmp_path).await;

    // Mark as committed and update key index
    let s3_key = CacheState::s3_key(id);
    let (key, version) = {
        let mut entries = state.entries.write().await;
        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
            entry.committed = true;
            entry.size = body.size;
            entry.s3_key = Some(s3_key);
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
        "cache entry committed to S3"
    );
    Ok(StatusCode::NO_CONTENT)
}

// ── GET /artifacts/:id ─────────────────────────────────────────────────
// Stream the cached archive from S3 to the client.

async fn get_artifact(
    State(state): State<Arc<CacheState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, StatusCode> {
    check_bearer(&state, &headers)?;

    let (byte_stream, size) = state.get_from_s3(id).await?;

    tracing::info!(
        cache_id = id,
        size_mb = size / 1_048_576,
        "cache artifact download from S3"
    );

    let reader = byte_stream.into_async_read();
    let stream = tokio_util::io::ReaderStream::new(reader);
    let body = Body::from_stream(stream);

    Ok(axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .header("content-length", size)
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

    /// Create a test CacheState with a mock S3 client.
    /// Uses a local-only mode for testing (S3 operations will fail,
    /// but metadata and chunk upload tests work).
    async fn test_state() -> Arc<CacheState> {
        let dir = tempfile::tempdir().unwrap().keep();
        let tmp_dir = dir.join("tmp");
        tokio::fs::create_dir_all(&tmp_dir).await.unwrap();
        let index_path = dir.join("index.json");

        // Build a dummy S3 client (won't be used in metadata-only tests)
        let config = CacheServiceConfig {
            enabled: true,
            dir: dir.to_str().unwrap().to_string(),
            token: None,
            s3_endpoint: "http://localhost:19999".to_string(),
            s3_bucket: "test-bucket".to_string(),
            s3_access_key: Some("test".to_string()),
            s3_secret_key: Some("test".to_string()),
            s3_region: "us-east-1".to_string(),
        };
        let s3 = build_s3_client(&config).await;

        Arc::new(CacheState {
            entries: RwLock::new(Vec::new()),
            key_index: RwLock::new(HashMap::new()),
            next_id: AtomicI64::new(1),
            tmp_dir,
            index_path,
            token: "test-token".to_string(),
            s3,
            s3_bucket: "test-bucket".to_string(),
        })
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
    async fn reserve_returns_cache_id() {
        let state = test_state().await;
        let resp = app(state)
            .oneshot(
                Request::post("/_apis/artifactcache/caches")
                    .header(auth_header().0, auth_header().1)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"key":"test-key","version":"v1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 10000).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["cacheId"].as_i64().unwrap() > 0);
    }

    #[tokio::test]
    async fn chunk_upload_to_reserved_entry() {
        let state = test_state().await;

        // Reserve
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

        // Upload chunk
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/_apis/artifactcache/caches/{}", id))
                    .header(auth_header().0, auth_header().1)
                    .header("content-range", "bytes 0-3/*")
                    .body(Body::from(vec![1, 2, 3, 4]))
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
    async fn artifact_not_found_without_s3() {
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
        // S3 connection fails, so we get NOT_FOUND
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

    #[test]
    fn s3_key_format() {
        assert_eq!(CacheState::s3_key(42), "cache/42");
        assert_eq!(CacheState::s3_key(1), "cache/1");
    }
}
