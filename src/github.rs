use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::Context;
use reqwest::Client;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use tokio::sync::RwLock;
use tokio::time::Duration;

use crate::config::GitHubConfig;
use crate::metrics;

// ── Auth provider ──────────────────────────────────────────────────────

enum AuthProvider {
    Pat(SecretString),
    App {
        app_id: u64,
        installation_id: u64,
        private_key: Vec<u8>,
        cached_token: RwLock<Option<CachedToken>>,
    },
}

struct CachedToken {
    token: String,
    expires_at: chrono::DateTime<chrono::Utc>,
}

impl AuthProvider {
    fn from_config(config: &GitHubConfig) -> anyhow::Result<Self> {
        if let Some(app) = &config.app {
            let private_key = std::fs::read(&app.private_key_path).with_context(|| {
                format!("reading GitHub App private key: {}", app.private_key_path)
            })?;
            tracing::info!(
                app_id = app.app_id,
                installation_id = app.installation_id,
                "using GitHub App authentication"
            );
            Ok(AuthProvider::App {
                app_id: app.app_id,
                installation_id: app.installation_id,
                private_key,
                cached_token: RwLock::new(None),
            })
        } else if let Some(token) = &config.token {
            tracing::info!("using PAT authentication");
            Ok(AuthProvider::Pat(token.clone()))
        } else {
            anyhow::bail!("no authentication method configured");
        }
    }

    /// Get a valid bearer token. For PAT, returns it directly.
    /// For App, generates JWT and exchanges for installation token with caching.
    async fn get_token(&self, client: &Client) -> anyhow::Result<String> {
        match self {
            AuthProvider::Pat(token) => Ok(token.expose_secret().to_string()),
            AuthProvider::App {
                app_id,
                installation_id,
                private_key,
                cached_token,
            } => {
                // Check cache first
                {
                    let cache = cached_token.read().await;
                    if let Some(ct) = cache.as_ref() {
                        // Refresh 5 minutes before expiry
                        if ct.expires_at > chrono::Utc::now() + chrono::Duration::minutes(5) {
                            return Ok(ct.token.clone());
                        }
                    }
                }

                // Generate new token
                let token = Self::exchange_installation_token(
                    client,
                    *app_id,
                    *installation_id,
                    private_key,
                )
                .await?;

                // Cache it (installation tokens are valid for 1 hour)
                let mut cache = cached_token.write().await;
                *cache = Some(CachedToken {
                    token: token.clone(),
                    expires_at: chrono::Utc::now() + chrono::Duration::minutes(55),
                });

                Ok(token)
            }
        }
    }

    /// Generate a JWT and exchange it for an installation access token.
    async fn exchange_installation_token(
        client: &Client,
        app_id: u64,
        installation_id: u64,
        private_key: &[u8],
    ) -> anyhow::Result<String> {
        let now = chrono::Utc::now();
        let claims = serde_json::json!({
            "iat": (now - chrono::Duration::seconds(60)).timestamp(),
            "exp": (now + chrono::Duration::minutes(10)).timestamp(),
            "iss": app_id,
        });

        let encoding_key = jsonwebtoken::EncodingKey::from_rsa_pem(private_key)
            .context("parsing GitHub App private key as RSA PEM")?;

        let jwt = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
            &claims,
            &encoding_key,
        )
        .context("encoding JWT")?;

        let url = format!(
            "https://api.github.com/app/installations/{}/access_tokens",
            installation_id
        );

        let resp = client
            .post(&url)
            .bearer_auth(&jwt)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "fc-runner/0.1")
            .send()
            .await
            .context("requesting installation token")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "installation token request failed (HTTP {}): {}",
                status,
                body
            );
        }

        #[derive(Deserialize)]
        struct InstallationToken {
            token: String,
        }

        let data = resp.json::<InstallationToken>().await?;
        tracing::info!("GitHub App installation token acquired");
        Ok(data.token)
    }
}

// ── Response types ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct WorkflowRunsResponse {
    pub workflow_runs: Vec<WorkflowRun>,
}

#[derive(Debug, Deserialize)]
pub struct WorkflowRun {
    pub id: u64,
}

#[derive(Debug, Deserialize)]
pub struct JobsResponse {
    pub jobs: Vec<Job>,
}

#[derive(Debug, Deserialize)]
pub struct Job {
    pub id: u64,
    pub run_id: u64,
    pub labels: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct JitConfigResponse {
    pub encoded_jit_config: String,
}

#[derive(Debug, Deserialize)]
pub struct RunnersResponse {
    pub runners: Vec<Runner>,
}

#[derive(Debug, Deserialize)]
pub struct Runner {
    pub id: u64,
    pub name: String,
    pub status: String,
    /// Whether the runner is currently executing a job.
    #[serde(default)]
    pub busy: bool,
}

// ── Client ─────────────────────────────────────────────────────────────

pub struct GitHubClient {
    client: Client,
    config: GitHubConfig,
    auth: AuthProvider,
    rate_limit_remaining: Arc<AtomicU32>,
}

impl GitHubClient {
    pub fn new(config: GitHubConfig) -> anyhow::Result<Self> {
        let auth = AuthProvider::from_config(&config)?;
        let client = Client::builder()
            .user_agent("fc-runner/0.1")
            .timeout(Duration::from_secs(30))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            client,
            config,
            auth,
            rate_limit_remaining: Arc::new(AtomicU32::new(5000)),
        })
    }

    /// Returns the base API URL for a given repo.
    fn repo_url(&self, repo: &str) -> String {
        format!(
            "https://api.github.com/repos/{}/{}",
            self.config.owner, repo
        )
    }

    /// Returns the base API URL for the organization (if configured).
    fn org_url(&self) -> Option<String> {
        self.config
            .organization
            .as_ref()
            .map(|org| format!("https://api.github.com/orgs/{}", org))
    }

    /// Whether this client operates in org-level runner mode.
    pub fn is_org_mode(&self) -> bool {
        self.config.organization.is_some()
    }

    /// Returns the list of repos to poll.
    pub fn repos(&self) -> Vec<String> {
        self.config.all_repos()
    }

    /// Returns the owner name.
    pub fn owner(&self) -> &str {
        &self.config.owner
    }

    async fn request(
        &self,
        method: reqwest::Method,
        url: &str,
    ) -> anyhow::Result<reqwest::RequestBuilder> {
        let token = self.auth.get_token(&self.client).await?;
        Ok(self
            .client
            .request(method, url)
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28"))
    }

    /// Check rate limit headers and warn/backoff if running low.
    async fn check_rate_limit(&self, resp: &reqwest::Response) {
        if let Some(remaining) = resp
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<u32>().ok())
        {
            self.rate_limit_remaining
                .store(remaining, Ordering::Relaxed);
            metrics::GITHUB_RATE_LIMIT_REMAINING.set(remaining as i64);
            if remaining < 100 {
                tracing::warn!(remaining, "GitHub API rate limit running low");
            }
            if remaining < 10 {
                tracing::error!(
                    remaining,
                    "GitHub API rate limit nearly exhausted, backing off"
                );
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        }
    }

    pub async fn list_queued_runs(&self, repo: &str) -> anyhow::Result<Vec<WorkflowRun>> {
        metrics::GITHUB_API_CALLS
            .with_label_values(&["list_queued_runs"])
            .inc();
        let url = format!("{}/actions/runs?status=queued", self.repo_url(repo));
        let resp = self
            .request(reqwest::Method::GET, &url)
            .await?
            .send()
            .await?;
        self.check_rate_limit(&resp).await;
        let data = resp
            .error_for_status()
            .with_context(|| format!("listing queued runs for {}/{}", self.config.owner, repo))?
            .json::<WorkflowRunsResponse>()
            .await?;
        Ok(data.workflow_runs)
    }

    pub async fn list_queued_jobs(&self, repo: &str, run_id: u64) -> anyhow::Result<Vec<Job>> {
        metrics::GITHUB_API_CALLS
            .with_label_values(&["list_queued_jobs"])
            .inc();
        let url = format!(
            "{}/actions/runs/{}/jobs?filter=queued",
            self.repo_url(repo),
            run_id
        );
        let resp = self
            .request(reqwest::Method::GET, &url)
            .await?
            .send()
            .await?;
        self.check_rate_limit(&resp).await;
        let data = resp
            .error_for_status()
            .with_context(|| {
                format!(
                    "listing queued jobs for {}/{} run {}",
                    self.config.owner, repo, run_id
                )
            })?
            .json::<JobsResponse>()
            .await?;
        Ok(data.jobs)
    }

    /// Generate a registration token for pre-registering runners (warm pool mode).
    pub async fn generate_registration_token(&self, repo: &str) -> anyhow::Result<String> {
        metrics::GITHUB_API_CALLS
            .with_label_values(&["generate_registration_token"])
            .inc();
        let url = format!("{}/actions/runners/registration-token", self.repo_url(repo));
        let resp = self
            .request(reqwest::Method::POST, &url)
            .await?
            .send()
            .await?;
        self.check_rate_limit(&resp).await;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "registration token for {}/{} failed (HTTP {}): {}",
                self.config.owner,
                repo,
                status,
                body
            );
        }
        #[derive(Deserialize)]
        struct RegToken {
            token: String,
        }
        let data = resp.json::<RegToken>().await?;
        Ok(data.token)
    }

    /// List all self-hosted runners for a repo.
    pub async fn list_runners(&self, repo: &str) -> anyhow::Result<Vec<Runner>> {
        metrics::GITHUB_API_CALLS
            .with_label_values(&["list_runners"])
            .inc();
        let url = format!("{}/actions/runners?per_page=100", self.repo_url(repo));
        let resp = self
            .request(reqwest::Method::GET, &url)
            .await?
            .send()
            .await?;
        self.check_rate_limit(&resp).await;
        let data = resp
            .error_for_status()
            .with_context(|| format!("listing runners for {}/{}", self.config.owner, repo))?
            .json::<RunnersResponse>()
            .await?;
        Ok(data.runners)
    }

    /// Delete a runner by ID.
    pub async fn delete_runner(&self, repo: &str, runner_id: u64) -> anyhow::Result<()> {
        metrics::GITHUB_API_CALLS
            .with_label_values(&["delete_runner"])
            .inc();
        let url = format!("{}/actions/runners/{}", self.repo_url(repo), runner_id);
        let resp = self
            .request(reqwest::Method::DELETE, &url)
            .await?
            .send()
            .await?;
        self.check_rate_limit(&resp).await;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "delete runner {} for {}/{} failed (HTTP {}): {}",
                runner_id,
                self.config.owner,
                repo,
                status,
                body
            );
        }
        Ok(())
    }

    /// Delete a specific runner by name. Finds the runner in the list and deletes it.
    /// Used after a VM shuts down to clean up its specific runner registration.
    pub async fn delete_runner_by_name(&self, repo: &str, runner_name: &str) {
        match self.list_runners(repo).await {
            Ok(runners) => {
                for runner in runners {
                    if runner.name == runner_name {
                        tracing::info!(
                            runner_id = runner.id,
                            runner_name = %runner.name,
                            repo = %repo,
                            "deleting runner by name"
                        );
                        if let Err(e) = self.delete_runner(repo, runner.id).await {
                            tracing::warn!(
                                runner_name = %runner_name,
                                error = %e,
                                "failed to delete runner by name"
                            );
                        }
                        return;
                    }
                }
                tracing::debug!(runner_name = %runner_name, repo = %repo, "runner not found for deletion (may already be removed)");
            }
            Err(e) => {
                tracing::warn!(repo = %repo, error = %e, "failed to list runners for targeted cleanup");
            }
        }
    }

    /// Delete a specific org-level runner by name.
    pub async fn delete_org_runner_by_name(&self, runner_name: &str) {
        match self.list_org_runners().await {
            Ok(runners) => {
                for runner in runners {
                    if runner.name == runner_name {
                        let org_url = match self.org_url() {
                            Some(url) => url,
                            None => return,
                        };
                        let org = self.config.organization.as_deref().unwrap_or("unknown");
                        tracing::info!(
                            runner_id = runner.id,
                            runner_name = %runner.name,
                            org = %org,
                            "deleting org runner by name"
                        );
                        let del_url = format!("{}/actions/runners/{}", org_url, runner.id);
                        metrics::GITHUB_API_CALLS
                            .with_label_values(&["delete_org_runner"])
                            .inc();
                        match self.request(reqwest::Method::DELETE, &del_url).await {
                            Ok(req) => match req.send().await {
                                Ok(resp) => {
                                    if !resp.status().is_success() {
                                        let status = resp.status();
                                        let body = resp.text().await.unwrap_or_default();
                                        tracing::warn!(
                                            runner_name = %runner_name,
                                            status = %status,
                                            body = %body,
                                            "failed to delete org runner by name (HTTP error)"
                                        );
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(runner_name = %runner_name, error = %e, "failed to send delete request");
                                }
                            },
                            Err(e) => {
                                tracing::warn!(runner_name = %runner_name, error = %e, "failed to build delete request");
                            }
                        }
                        return;
                    }
                }
                tracing::debug!(runner_name = %runner_name, "org runner not found for deletion (may already be removed)");
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to list org runners for targeted cleanup");
            }
        }
    }

    /// Remove offline runners whose names start with "fc-" (left over from completed/crashed VMs).
    pub async fn remove_offline_runners(&self, repo: &str) {
        match self.list_runners(repo).await {
            Ok(runners) => {
                for runner in runners {
                    if runner.name.starts_with("fc-") && runner.status == "offline" {
                        tracing::info!(
                            runner_id = runner.id,
                            runner_name = %runner.name,
                            repo = %repo,
                            "removing offline runner"
                        );
                        if let Err(e) = self.delete_runner(repo, runner.id).await {
                            tracing::warn!(
                                runner_id = runner.id,
                                error = %e,
                                "failed to remove offline runner"
                            );
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(repo = %repo, error = %e, "failed to list runners for cleanup");
            }
        }
    }

    /// Generate JIT config — dispatches to org or repo level based on config.
    pub async fn generate_jit_config(&self, repo: &str, job_id: u64) -> anyhow::Result<String> {
        if self.is_org_mode() {
            self.generate_org_jit_config(job_id).await
        } else {
            self.generate_repo_jit_config(repo, job_id).await
        }
    }

    /// Generate a repo-level JIT config.
    async fn generate_repo_jit_config(&self, repo: &str, job_id: u64) -> anyhow::Result<String> {
        metrics::GITHUB_API_CALLS
            .with_label_values(&["generate_jit_config"])
            .inc();
        let url = format!("{}/actions/runners/generate-jitconfig", self.repo_url(repo));
        let body = serde_json::json!({
            "name": format!("fc-{}-{}", job_id, &uuid::Uuid::new_v4().to_string()[..8]),
            "runner_group_id": self.config.runner_group_id,
            "labels": self.config.labels,
            "work_folder": "_work"
        });
        let resp = self
            .request(reqwest::Method::POST, &url)
            .await?
            .json(&body)
            .send()
            .await?;
        self.check_rate_limit(&resp).await;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "JIT config for {}/{} job {} failed (HTTP {}): {}",
                self.config.owner,
                repo,
                job_id,
                status,
                body
            );
        }
        let data = resp.json::<JitConfigResponse>().await?;
        Ok(data.encoded_jit_config)
    }

    /// Generate an org-level JIT config.
    async fn generate_org_jit_config(&self, job_id: u64) -> anyhow::Result<String> {
        metrics::GITHUB_API_CALLS
            .with_label_values(&["generate_org_jit_config"])
            .inc();
        let org_url = self.org_url().expect("org mode checked by caller");
        let url = format!("{}/actions/runners/generate-jitconfig", org_url);
        let body = serde_json::json!({
            "name": format!("fc-{}-{}", job_id, &uuid::Uuid::new_v4().to_string()[..8]),
            "runner_group_id": self.config.runner_group_id,
            "labels": self.config.labels,
            "work_folder": "_work"
        });
        let resp = self
            .request(reqwest::Method::POST, &url)
            .await?
            .json(&body)
            .send()
            .await?;
        self.check_rate_limit(&resp).await;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let org = self.config.organization.as_deref().unwrap_or("unknown");
            anyhow::bail!(
                "org JIT config for {} job {} failed (HTTP {}): {}",
                org,
                job_id,
                status,
                body
            );
        }
        let data = resp.json::<JitConfigResponse>().await?;
        Ok(data.encoded_jit_config)
    }

    /// Generate an org-level registration token.
    #[allow(dead_code)]
    pub async fn generate_org_registration_token(&self) -> anyhow::Result<String> {
        metrics::GITHUB_API_CALLS
            .with_label_values(&["generate_org_registration_token"])
            .inc();
        let org_url = self.org_url().expect("org mode checked by caller");
        let url = format!("{}/actions/runners/registration-token", org_url);
        let resp = self
            .request(reqwest::Method::POST, &url)
            .await?
            .send()
            .await?;
        self.check_rate_limit(&resp).await;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let org = self.config.organization.as_deref().unwrap_or("unknown");
            anyhow::bail!(
                "org registration token for {} failed (HTTP {}): {}",
                org,
                status,
                body
            );
        }
        #[derive(Deserialize)]
        struct RegToken {
            token: String,
        }
        let data = resp.json::<RegToken>().await?;
        Ok(data.token)
    }

    /// Remove offline runners at the org level.
    /// List all self-hosted runners for the organization.
    pub async fn list_org_runners(&self) -> anyhow::Result<Vec<Runner>> {
        let org_url = self.org_url().context("organization not configured")?;
        let url = format!("{}/actions/runners?per_page=100", org_url);
        metrics::GITHUB_API_CALLS
            .with_label_values(&["list_org_runners"])
            .inc();
        let resp = self
            .request(reqwest::Method::GET, &url)
            .await?
            .send()
            .await?;
        self.check_rate_limit(&resp).await;
        let data = resp
            .error_for_status()
            .context("listing org runners")?
            .json::<RunnersResponse>()
            .await?;
        Ok(data.runners)
    }

    pub async fn remove_org_offline_runners(&self) {
        let org_url = match self.org_url() {
            Some(url) => url,
            None => return,
        };
        let url = format!("{}/actions/runners?per_page=100", org_url);
        metrics::GITHUB_API_CALLS
            .with_label_values(&["list_org_runners"])
            .inc();
        let resp = match self.request(reqwest::Method::GET, &url).await {
            Ok(req) => match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to list org runners for cleanup");
                    return;
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "failed to build org runners request");
                return;
            }
        };
        self.check_rate_limit(&resp).await;
        let data = match resp.json::<RunnersResponse>().await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, "failed to parse org runners response");
                return;
            }
        };
        for runner in data.runners {
            if runner.name.starts_with("fc-") && runner.status == "offline" {
                let org = self.config.organization.as_deref().unwrap_or("unknown");
                tracing::info!(
                    runner_id = runner.id,
                    runner_name = %runner.name,
                    org = %org,
                    "removing offline org runner"
                );
                let del_url = format!("{}/actions/runners/{}", org_url, runner.id);
                metrics::GITHUB_API_CALLS
                    .with_label_values(&["delete_org_runner"])
                    .inc();
                match self.request(reqwest::Method::DELETE, &del_url).await {
                    Ok(req) => match req.send().await {
                        Ok(resp) => {
                            if !resp.status().is_success() {
                                let status = resp.status();
                                let body = resp.text().await.unwrap_or_default();
                                tracing::warn!(
                                    runner_id = runner.id,
                                    runner_name = %runner.name,
                                    status = %status,
                                    body = %body,
                                    "failed to delete org runner (HTTP error)"
                                );
                            } else {
                                tracing::info!(
                                    runner_id = runner.id,
                                    runner_name = %runner.name,
                                    "successfully removed offline org runner"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(runner_id = runner.id, error = %e, "failed to send delete request for org runner");
                        }
                    },
                    Err(e) => {
                        tracing::warn!(runner_id = runner.id, error = %e, "failed to build delete request for org runner");
                    }
                }
            }
        }
    }
}
