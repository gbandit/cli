use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Reqwest client preconfigured with `Gbandit-Client` so backends can
/// route per-version behaviour (e.g. archive format support) and
/// `X-Gbandit-Cli-Version` so the platform can reject outdated CLIs (426).
pub(crate) fn http_client() -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    let value = format!("gbandit-cli/{}", crate::BUILD_VERSION);
    let header =
        reqwest::header::HeaderValue::from_str(&value).expect("build version must be ASCII");
    headers.insert(
        reqwest::header::HeaderName::from_static("gbandit-client"),
        header,
    );
    headers.insert(
        reqwest::header::HeaderName::from_static("x-gbandit-cli-version"),
        reqwest::header::HeaderValue::from_static(env!("CARGO_PKG_VERSION")),
    );
    headers.insert(
        reqwest::header::USER_AGENT,
        reqwest::header::HeaderValue::from_static("gbandit-cli"),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .expect("reqwest client must build with static headers")
}

/// Platform error body: `error` is always a human-readable message; `code`
/// and `issues` are present for machine-actionable failures (e.g.
/// `invalid_config`, `database_removal_requires_confirmation`). Lenient by
/// design — the server owns the schema.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ApiError {
    pub(crate) error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) code: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) issues: Vec<ApiErrorIssue>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ApiErrorIssue {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) docs_url: Option<String>,
}

impl ApiError {
    pub(crate) fn has_code(&self, code: &str) -> bool {
        self.code.as_deref() == Some(code)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.error)?;
        for issue in &self.issues {
            f.write_str("\n  - ")?;
            if let Some(path) = issue.path.as_deref() {
                write!(f, "{path}: ")?;
            }
            f.write_str(issue.message.as_deref().unwrap_or("invalid"))?;
            if let Some(docs) = issue.docs_url.as_deref() {
                write!(f, " (see {docs})")?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for ApiError {}

pub(crate) async fn parse_json<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
) -> Result<T> {
    let status = response.status();
    if status.is_success() {
        let bytes = response
            .bytes()
            .await
            .context("failed to read response body")?;
        return serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "failed to decode response JSON (status {status}): {}",
                body_snippet(&bytes)
            )
        });
    }

    bail!(parse_error(response).await)
}

pub(crate) async fn parse_error(response: reqwest::Response) -> ApiError {
    let status = response.status();
    // auth-service answers with a plain-text sentence rather than JSON; that
    // sentence is the message worth showing.
    let plain_text = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/plain"));
    let bytes = response.bytes().await.unwrap_or_default();
    let mut parsed = match serde_json::from_slice::<ApiError>(&bytes) {
        Ok(payload) => payload,
        Err(_) => ApiError {
            error: if plain_text && !bytes.trim_ascii().is_empty() {
                body_snippet(&bytes)
            } else {
                format!("request failed with status {status}")
            },
            code: None,
            issues: Vec::new(),
        },
    };
    if is_account_suspended(status, &bytes) {
        parsed.error = ACCOUNT_SUSPENDED_MESSAGE.to_string();
    } else if status == reqwest::StatusCode::UNAUTHORIZED {
        parsed.error = format!("{} — run `gbandit login` to re-authenticate", parsed.error);
    } else if parsed.has_code("cli_outdated") {
        parsed.error = format!("{} — run `gbandit update`", parsed.error);
    }
    parsed
}

pub(crate) const ACCOUNT_SUSPENDED_MESSAGE: &str =
    "This account is suspended. Questions go to abuse@gbandit.com.";

/// auth-service answers every session lookup for a suspended user with a 403
/// whose whole body is `account_suspended`.
pub(crate) fn is_account_suspended(status: reqwest::StatusCode, body: &[u8]) -> bool {
    status == reqwest::StatusCode::FORBIDDEN && body.trim_ascii() == b"account_suspended"
}

fn body_snippet(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "<empty body>".to_string();
    }
    let mut snippet: String = trimmed.chars().take(200).collect();
    if trimmed.chars().count() > 200 {
        snippet.push('…');
    }
    snippet
}
