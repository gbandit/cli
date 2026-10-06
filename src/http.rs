use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
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

/// Where the platform's problem types live; each one is an anchor on that page.
const PROBLEM_TYPE_BASE: &str = "https://docs.gbandit.com/errors#";

const PROBLEM_CONTENT_TYPE: &str = "application/problem+json";

/// The correlation id every platform service echoes on its responses.
const REQUEST_ID_HEADER: &str = "x-request-id";

/// Problem types the CLI acts on rather than only prints.
pub(crate) mod problem_types {
    pub(crate) const ACCESS_DENIED: &str = "access-denied";
    pub(crate) const AUTHORIZATION_PENDING: &str = "authorization-pending";
    pub(crate) const CLI_OUTDATED: &str = "cli-outdated";
    pub(crate) const DATABASE_REMOVAL_REQUIRES_CONFIRMATION: &str =
        "database-removal-requires-confirmation";
    pub(crate) const EXPIRED_TOKEN: &str = "expired-token";
    pub(crate) const GOOGLE_ACCOUNT_REQUIRED: &str = "google-account-required";
    pub(crate) const INVALID_GRANT: &str = "invalid-grant";
}

/// A failed platform call: the RFC 9457 Problem Details body every gbandit API
/// answers a failure with (https://docs.gbandit.com/errors). Lenient by
/// design — the server owns the schema, and members the CLI has no use for
/// are kept so `--json` passes them through.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ApiError {
    #[serde(rename = "type", default = "about_blank")]
    pub(crate) type_uri: String,
    #[serde(default)]
    pub(crate) title: String,
    pub(crate) status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) request_id: Option<String>,
    /// Whether repeating the same request can succeed.
    #[serde(default)]
    pub(crate) retryable: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) issues: Vec<ApiErrorIssue>,
    /// Raw technical output: a string such as git's stderr, or whatever
    /// structure the service attached. Kept as sent so `--json` passes it on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) diagnostics: Option<serde_json::Value>,
    #[serde(flatten)]
    pub(crate) extensions: serde_json::Map<String, serde_json::Value>,
}

fn about_blank() -> String {
    "about:blank".to_string()
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

/// Lines of `diagnostics` an error shows unless `--verbose` asks for all.
const INLINE_DIAGNOSTIC_LINES: usize = 3;
/// Characters of each such line shown unless `--verbose` asks for all.
const INLINE_DIAGNOSTIC_LINE_CHARS: usize = 200;

/// `--verbose` is global to the process, and an error is rendered by its
/// `Display`, which has no printer to ask.
static FULL_DIAGNOSTICS: AtomicBool = AtomicBool::new(false);

pub(crate) fn show_full_diagnostics(enabled: bool) {
    FULL_DIAGNOSTICS.store(enabled, Ordering::Relaxed);
}

impl ApiError {
    pub(crate) fn is(&self, problem_type: &str) -> bool {
        self.type_uri
            .strip_prefix(PROBLEM_TYPE_BASE)
            .is_some_and(|slug| slug == problem_type)
    }

    /// The sentence for the user: `detail` when there is one, else the title.
    pub(crate) fn message(&self) -> &str {
        self.detail.as_deref().unwrap_or(&self.title)
    }

    fn diagnostics_text(&self) -> Option<String> {
        let text = match self.diagnostics.as_ref()? {
            serde_json::Value::String(text) => text.trim().to_string(),
            other => serde_json::to_string_pretty(other).expect("a JSON value serializes to JSON"),
        };
        (!text.is_empty()).then_some(text)
    }

    /// A problem for a response that did not carry one: an ingress error page,
    /// a proxy's plain text. Its body is kept as diagnostics. Only a gateway's
    /// 502/503/504 is worth repeating: the service behind it is restarting or
    /// briefly unreachable, while anything else answered for a reason that a
    /// second attempt does not change.
    fn without_body(status: reqwest::StatusCode, bytes: &[u8]) -> Self {
        let retryable = matches!(status.as_u16(), 502..=504);
        let detail = if retryable {
            format!(
                "The platform could not be reached (HTTP {}). Try again in a moment.",
                status.as_u16()
            )
        } else {
            format!(
                "The platform answered HTTP {} without saying what went wrong.",
                status.as_u16()
            )
        };
        let body = String::from_utf8_lossy(bytes).trim().to_string();
        Self {
            type_uri: about_blank(),
            title: status.canonical_reason().unwrap_or("Error").to_string(),
            status: status.as_u16(),
            detail: Some(detail),
            request_id: None,
            retryable,
            issues: Vec::new(),
            diagnostics: (!body.is_empty()).then_some(serde_json::Value::String(body)),
            extensions: serde_json::Map::new(),
        }
    }

    /// The failure a response describes. Only a body labelled
    /// `application/problem+json` is read as a problem; anything else is the
    /// answer of something in front of the platform. A body without its
    /// `request_id` takes it from the `x-request-id` header, which every
    /// platform service echoes.
    fn from_response(
        status: reqwest::StatusCode,
        content_type: Option<&str>,
        request_id: Option<&str>,
        bytes: &[u8],
    ) -> Self {
        let is_problem = content_type
            .and_then(|value| value.split(';').next())
            .is_some_and(|media_type| {
                media_type.trim().eq_ignore_ascii_case(PROBLEM_CONTENT_TYPE)
            });
        let parsed = if is_problem {
            serde_json::from_slice::<ApiError>(bytes).ok()
        } else {
            None
        };
        let mut error = parsed.unwrap_or_else(|| ApiError::without_body(status, bytes));
        if error.request_id.is_none() {
            error.request_id = request_id.map(str::to_string);
        }
        error
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())?;
        if self.status == reqwest::StatusCode::UNAUTHORIZED.as_u16() {
            f.write_str(" — run `gbandit login` to re-authenticate")?;
        } else if self.is(problem_types::CLI_OUTDATED) {
            f.write_str(" — run `gbandit update`")?;
        }
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
        if let Some(diagnostics) = self.diagnostics_text() {
            write_diagnostics(f, &diagnostics, FULL_DIAGNOSTICS.load(Ordering::Relaxed))?;
        }
        if let Some(request_id) = self.request_id.as_deref() {
            write!(f, "\n  request id: {request_id}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ApiError {}

fn write_diagnostics(
    f: &mut std::fmt::Formatter<'_>,
    diagnostics: &str,
    full: bool,
) -> std::fmt::Result {
    if full {
        for line in diagnostics.lines() {
            write!(f, "\n  {line}")?;
        }
        return Ok(());
    }
    let mut lines = diagnostics.lines();
    for line in lines.by_ref().take(INLINE_DIAGNOSTIC_LINES) {
        let shown: String = line.chars().take(INLINE_DIAGNOSTIC_LINE_CHARS).collect();
        let cut = shown.len() < line.len();
        write!(f, "\n  {shown}{}", if cut { "…" } else { "" })?;
    }
    let hidden = lines.count();
    if hidden > 0 {
        write!(f, "\n  … {hidden} more lines (--verbose shows them)")?;
    }
    Ok(())
}

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

    Err(parse_error(response).await.into())
}

pub(crate) async fn parse_error(response: reqwest::Response) -> ApiError {
    let status = response.status();
    let header = |name: reqwest::header::HeaderName| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    };
    let content_type = header(reqwest::header::CONTENT_TYPE);
    let request_id = header(reqwest::header::HeaderName::from_static(REQUEST_ID_HEADER));
    let bytes = response.bytes().await.unwrap_or_default();
    ApiError::from_response(
        status,
        content_type.as_deref(),
        request_id.as_deref(),
        &bytes,
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_problem_prints_its_detail_issues_and_request_id() {
        let error: ApiError = serde_json::from_str(
            r#"{
                "type": "https://docs.gbandit.com/errors#invalid-config",
                "title": "Invalid gbandit.jsonc",
                "status": 422,
                "detail": "gbandit.jsonc has 1 problem",
                "request_id": "req-1",
                "retryable": false,
                "issues": [{"path": "backend.port", "message": "must be a number", "docs_url": "https://docs.gbandit.com/deploy#components"}]
            }"#,
        )
        .unwrap();
        assert!(error.is("invalid-config"));
        assert!(!error.is("config-missing"));
        assert_eq!(
            error.to_string(),
            "gbandit.jsonc has 1 problem\n  - backend.port: must be a number (see https://docs.gbandit.com/deploy#components)\n  request id: req-1"
        );
    }

    #[test]
    fn a_body_that_is_not_a_problem_becomes_diagnostics() {
        let error = ApiError::from_response(
            reqwest::StatusCode::BAD_GATEWAY,
            Some("text/html"),
            Some("req-2"),
            b"<html>502</html>",
        );
        assert_eq!(error.title, "Bad Gateway");
        assert!(error.retryable);
        assert_eq!(
            error.to_string(),
            "The platform could not be reached (HTTP 502). Try again in a moment.\n  <html>502</html>\n  request id: req-2"
        );
    }

    #[test]
    fn only_a_gateway_failure_without_a_problem_is_retryable() {
        let not_found = ApiError::from_response(reqwest::StatusCode::NOT_FOUND, None, None, b"");
        assert!(!not_found.retryable);
        assert_eq!(not_found.title, "Not Found");
        assert!(not_found.diagnostics.is_none());
    }

    #[test]
    fn json_that_is_not_labelled_a_problem_is_not_read_as_one() {
        let error = ApiError::from_response(
            reqwest::StatusCode::BAD_REQUEST,
            Some("application/json"),
            None,
            br#"{"type": "about:blank", "title": "Bad Request", "status": 400, "detail": "x"}"#,
        );
        assert_ne!(error.detail.as_deref(), Some("x"));
        assert!(error.diagnostics.is_some());
    }

    #[test]
    fn a_problem_takes_retryable_from_its_body_and_request_id_from_the_header() {
        let error = ApiError::from_response(
            reqwest::StatusCode::CONFLICT,
            Some("application/problem+json; charset=utf-8"),
            Some("req-3"),
            br#"{"type": "about:blank", "title": "Conflict", "status": 409, "detail": "busy", "retryable": true}"#,
        );
        assert!(error.retryable);
        assert_eq!(error.request_id.as_deref(), Some("req-3"));
        assert_eq!(error.message(), "busy");
    }

    #[test]
    fn structured_diagnostics_are_kept_and_long_ones_are_cut_inline() {
        let error: ApiError = serde_json::from_str(
            r#"{"title": "Git failed", "status": 500, "diagnostics": {"exit": 128}}"#,
        )
        .unwrap();
        assert_eq!(error.to_string(), "Git failed\n  {\n    \"exit\": 128\n  }");

        let error: ApiError = serde_json::from_str(
            r#"{"title": "Git failed", "status": 500, "diagnostics": "a\nb\nc\nd\ne"}"#,
        )
        .unwrap();
        assert_eq!(
            error.to_string(),
            "Git failed\n  a\n  b\n  c\n  … 2 more lines (--verbose shows them)"
        );
    }

    #[test]
    fn unknown_members_survive_for_json_output() {
        let error: ApiError = serde_json::from_str(
            r#"{"type": "about:blank", "title": "Payment Required", "status": 402, "usage": {"reset_at": "x"}}"#,
        )
        .unwrap();
        let value = serde_json::to_value(&error).unwrap();
        assert_eq!(value["usage"]["reset_at"], "x");
        assert_eq!(error.message(), "Payment Required");
    }
}
