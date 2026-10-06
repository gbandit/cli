use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use reqwest::multipart::Form;
use serde::{Deserialize, Serialize};

use crate::auth_session::load_auth;
use crate::http::{http_client, parse_error, parse_json};

pub(crate) struct PlatformClient {
    http: reqwest::Client,
    token: String,
    origin: String,
}

impl PlatformClient {
    pub(crate) async fn from_saved_auth() -> Result<Self> {
        let auth = load_auth().await?;
        Ok(Self {
            http: http_client(),
            token: auth.token,
            origin: auth.platform_api_origin,
        })
    }

    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn origin(&self) -> &str {
        &self.origin
    }

    pub(crate) async fn query_database(
        &self,
        environment: &str,
        project: &str,
        query: &str,
    ) -> Result<QueryResponse> {
        let response = self
            .http
            .post(format!(
                "{}/projects/{}/database/query?environment={}",
                self.origin, project, environment
            ))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "query": query }))
            .send()
            .await
            .context("failed to execute query")?;
        parse_json(response)
            .await
            .with_context(|| format!("query against project '{project}' ({environment}) failed"))
    }

    /// A deploy always builds and rolls out to dev; prod never builds and is
    /// reached with `promote`, so there is no target to name.
    pub(crate) async fn start_deploy<F>(
        &self,
        project: &str,
        make_form: F,
    ) -> Result<DeployPipeline>
    where
        F: Fn() -> Result<Form>,
    {
        let url = format!("{}/projects/{}/deploys", self.origin, project);
        let context = || format!("failed to start deploy for project '{project}'");
        let response = self
            .post_with_retry(&url, |request| Ok(request.multipart(make_form()?)))
            .await
            .with_context(context)?;
        parse_json(response).await.with_context(context)
    }

    /// `Ok(None)` = the platform skipped the baseline deploy (200 instead of
    /// 202) because the project already has a succeeded deploy.
    pub(crate) async fn start_baseline_deploy<F>(
        &self,
        project: &str,
        make_form: F,
    ) -> Result<Option<DeployPipeline>>
    where
        F: Fn() -> Result<Form>,
    {
        let url = format!("{}/projects/{}/deploys/baseline", self.origin, project);
        let context = || format!("failed to start baseline deploy for project '{project}'");
        let response = self
            .post_with_retry(&url, |request| Ok(request.multipart(make_form()?)))
            .await
            .with_context(context)?;
        if response.status() == reqwest::StatusCode::OK {
            return Ok(None);
        }
        Ok(Some(parse_json(response).await.with_context(context)?))
    }

    /// POST with three attempts over transport failures and refusals the
    /// platform marks retryable. A refusal comes back as its `ApiError`, so
    /// only a success is returned as a response. `body` is applied per attempt
    /// rather than once, because a multipart body is consumed by the attempt
    /// that sends it and has to be rebuilt.
    ///
    /// Retrying is only safe because every caller carries an idempotency key:
    /// the platform attaches a repeat of the same SubmissionId to the run the
    /// first attempt started rather than starting a second one.
    async fn post_with_retry<F>(&self, url: &str, body: F) -> Result<reqwest::Response>
    where
        F: Fn(reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder>,
    {
        const ATTEMPTS: u64 = 3;
        for attempt in 1..=ATTEMPTS {
            let request = body(self.http.post(url).bearer_auth(&self.token))?;
            match request.send().await {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response) => {
                    let error = parse_error(response).await;
                    if !error.retryable || attempt == ATTEMPTS {
                        return Err(error.into());
                    }
                }
                Err(error) if attempt == ATTEMPTS => return Err(error.into()),
                Err(_) => {}
            }
            tokio::time::sleep(std::time::Duration::from_millis(250 * attempt)).await;
        }
        unreachable!("the last attempt always returns")
    }

    /// Both sources come from the same store, so one call shape serves both
    /// (platform: docs/plans/tenant_logs_via_loki.md).
    pub(crate) async fn logs(
        &self,
        environment: &str,
        project: &str,
        source: &str,
    ) -> Result<Vec<LogEntry>> {
        let response = self
            .http
            .get(format!(
                "{}/projects/{}/logs?source={}&environment={}&limit=1000",
                self.origin, project, source, environment
            ))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("failed to fetch logs")?;
        let snapshot: LogsResponse = parse_json(response).await.with_context(|| {
            format!("failed to fetch {source} logs for project '{project}' ({environment})")
        })?;
        Ok(snapshot.logs)
    }

    pub(crate) async fn set_env(
        &self,
        environment: &str,
        project: &str,
        vars: BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>> {
        let response = self
            .http
            .put(format!(
                "{}/projects/{}/env?environment={}",
                self.origin, project, environment
            ))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "vars": vars }))
            .send()
            .await
            .context("failed to set environment variables")?;
        let result: EnvVarsApiResponse = parse_json(response).await.with_context(|| {
            format!("failed to set env vars for project '{project}' ({environment})")
        })?;
        Ok(result.vars)
    }

    pub(crate) async fn list_env(
        &self,
        environment: &str,
        project: &str,
    ) -> Result<EnvVarsApiResponse> {
        let response = self
            .http
            .get(format!(
                "{}/projects/{}/env?environment={}",
                self.origin, project, environment
            ))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("failed to list environment variables")?;
        parse_json(response).await.with_context(|| {
            format!("failed to list env vars for project '{project}' ({environment})")
        })
    }

    pub(crate) async fn delete_env(
        &self,
        environment: &str,
        project: &str,
        key: &str,
    ) -> Result<()> {
        let response = self
            .http
            .delete(format!(
                "{}/projects/{}/env/{}?environment={}",
                self.origin, project, key, environment
            ))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("failed to delete environment variable")?;
        if !response.status().is_success() {
            return Err(parse_error(response).await).with_context(|| {
                format!("failed to delete env var {key} for project '{project}' ({environment})")
            });
        }
        Ok(())
    }

    /// `Ok(None)` = the project does not exist (404).
    pub(crate) async fn get_project(&self, slug: &str) -> Result<Option<ProjectSummary>> {
        let response = self
            .http
            .get(format!("{}/projects/{slug}", self.origin))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("failed to fetch project")?;
        match response.status() {
            StatusCode::NOT_FOUND => Ok(None),
            StatusCode::OK => Ok(Some(parse_json(response).await?)),
            _ => bail!(parse_error(response).await),
        }
    }

    pub(crate) async fn slug_availability(&self, slug: &str) -> Result<SlugAvailability> {
        let response = self
            .http
            .get(format!("{}/projects/{slug}/availability", self.origin))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("failed to check slug availability")?;
        let parsed: SlugAvailabilityResponse = parse_json(response).await?;
        Ok(match (parsed.status.as_str(), parsed.owned_by_you) {
            ("free", _) => SlugAvailability::Free,
            ("taken", true) => SlugAvailability::TakenByYou,
            ("taken", false) => SlugAvailability::TakenByOther,
            ("deleting", _) => SlugAvailability::Deleting,
            (other, _) => bail!("unknown availability status '{other}'"),
        })
    }

    pub(crate) async fn update_project_title(&self, slug: &str, title: &str) -> Result<()> {
        let response = self
            .http
            .patch(format!("{}/projects/{slug}/game-profile", self.origin))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "title": title }))
            .send()
            .await
            .context("failed to update project title")?;
        if !response.status().is_success() {
            bail!(parse_error(response).await);
        }
        Ok(())
    }

    pub(crate) async fn upload_cover_image(
        &self,
        slug: &str,
        file_name: String,
        bytes: Vec<u8>,
    ) -> Result<GameProfile> {
        let form = Form::new().part(
            "file",
            reqwest::multipart::Part::bytes(bytes).file_name(file_name),
        );
        let response = self
            .http
            .put(format!(
                "{}/projects/{slug}/game-profile/cover-image",
                self.origin
            ))
            .bearer_auth(&self.token)
            .multipart(form)
            .send()
            .await
            .context("failed to upload cover image")?;
        parse_json(response)
            .await
            .with_context(|| format!("failed to set the cover image of project '{slug}'"))
    }

    pub(crate) async fn publish_game(&self, slug: &str) -> Result<GameProfile> {
        let response = self
            .http
            .put(self.publication_url(slug))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("failed to publish game")?;
        parse_json(response)
            .await
            .with_context(|| format!("failed to publish project '{slug}'"))
    }

    pub(crate) async fn unpublish_game(&self, slug: &str) -> Result<GameProfile> {
        let response = self
            .http
            .delete(self.publication_url(slug))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("failed to unpublish game")?;
        parse_json(response)
            .await
            .with_context(|| format!("failed to unpublish project '{slug}'"))
    }

    fn publication_url(&self, slug: &str) -> String {
        format!("{}/projects/{slug}/game-profile/publication", self.origin)
    }

    pub(crate) async fn create_project(&self, slug: &str, title: &str) -> Result<CreatedProject> {
        let response = self
            .http
            .post(format!("{}/projects", self.origin))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "slug": slug, "title": title }))
            .send()
            .await
            .context("failed to create project")?;

        match response.status() {
            StatusCode::CREATED => parse_json(response).await,
            // parse_error already maps 401 to a `gbandit login` hint.
            _ => bail!(parse_error(response).await),
        }
    }

    pub(crate) async fn delete_project(&self, slug: &str) -> Result<ProjectDeleteOutcome> {
        let response = self
            .http
            .delete(format!("{}/projects/{slug}", self.origin))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("failed to delete project")?;

        match response.status() {
            StatusCode::ACCEPTED => Ok(ProjectDeleteOutcome::Started),
            StatusCode::NOT_FOUND => bail!("project '{slug}' not found"),
            _ => bail!(parse_error(response).await),
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct QueryColumn {
    pub(crate) name: String,
    #[serde(rename = "data_type")]
    pub(crate) _data_type: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct QueryResponse {
    pub(crate) columns: Vec<QueryColumn>,
    pub(crate) rows: Vec<Vec<serde_json::Value>>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct DeployPipeline {
    pub(crate) pipeline_run_id: i64,
}

#[derive(Debug, Deserialize)]
struct LogsResponse {
    logs: Vec<LogEntry>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct LogEntry {
    pub(crate) timestamp: String,
    pub(crate) level: Option<String>,
    pub(crate) message: String,
    #[serde(default)]
    pub(crate) source_url: Option<String>,
    #[serde(default)]
    pub(crate) user_name: Option<String>,
    #[serde(default)]
    pub(crate) user_is_anon: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EnvVarsApiResponse {
    pub(crate) vars: BTreeMap<String, String>,
    pub(crate) system_vars: BTreeMap<String, String>,
}

pub(crate) enum ProjectDeleteOutcome {
    Started,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreatedProject {
    pub(crate) slug: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ProjectSummary {
    pub(crate) slug: String,
    pub(crate) title: String,
    pub(crate) cover_image_url: Option<String>,
    /// Set while the game is listed in the Game Catalog.
    pub(crate) published_at: Option<String>,
    pub(crate) play_url: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GameProfile {
    pub(crate) title: String,
    pub(crate) cover_image_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SlugAvailabilityResponse {
    status: String,
    owned_by_you: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SlugAvailability {
    Free,
    TakenByYou,
    TakenByOther,
    Deleting,
}

/// A started promotion, as the platform reports it. `from_*` describe what
/// prod served when the promotion began; `from_release_id` is `None` on the
/// first promotion, when prod has never run anything.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct PromotionStarted {
    pub(crate) pipeline_run_id: i64,
    pub(crate) status: String,
    pub(crate) release_id: String,
    pub(crate) from_release_id: Option<String>,
    pub(crate) from_commit_sha: Option<String>,
    pub(crate) to_commit_sha: Option<String>,
    pub(crate) created_at: String,
}

impl PlatformClient {
    /// Point prod at the Release dev runs. One submission id across the
    /// retries, so a retry after a dropped response attaches to the run the
    /// first attempt started instead of starting a second one.
    pub(crate) async fn start_promotion(
        &self,
        project: &str,
        confirm_database_removal: bool,
    ) -> Result<PromotionStarted> {
        let url = format!("{}/projects/{}/promote", self.origin, project);
        let body = serde_json::json!({
            "submission_id": uuid::Uuid::new_v4().to_string(),
            "confirm_database_removal": confirm_database_removal,
        });
        let context = || format!("failed to promote project '{project}'");
        let response = self
            .post_with_retry(&url, |request| Ok(request.json(&body)))
            .await
            .with_context(context)?;
        parse_json(response).await.with_context(context)
    }
}
