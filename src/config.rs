use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// The fields of gbandit.jsonc the CLI itself consumes. The platform owns
/// the config schema and validates it server-side, so unknown fields (and
/// fields the CLI doesn't need, like `frontend`/`backend`/`database`) are
/// deliberately ignored here.
#[derive(Debug, Deserialize)]
pub(crate) struct ProjectConfig {
    pub(crate) project: String,
    /// Display title. When present, deploy keeps the platform title in sync
    /// with this field (and uses it when first creating the project). Absent
    /// = deploy leaves the platform title alone; agent scaffolds omit it so
    /// a deploy can't clobber a title set in the web UI.
    #[serde(default)]
    pub(crate) title: Option<String>,
    #[serde(default)]
    local_dev: LocalDevConfig,
}

impl ProjectConfig {
    /// Local developer preference only. The Pi Agent runs without this config
    /// and always auto-commits: agent-written code deserves a git history.
    pub(crate) fn auto_commit(&self) -> bool {
        if std::env::var("GBANDIT_AGENT").is_ok() {
            return true;
        }
        self.local_dev.auto_commit
    }
}

#[derive(Debug, Deserialize)]
struct LocalDevConfig {
    /// When true, `deploy` auto-commits a dirty tree and pushes it to the
    /// linked remote once the deploy has succeeded. False (default): the
    /// CLI never touches git — no commits, no push; you sync the remote
    /// yourself. The default is false so deploying a pre-existing repo never
    /// commits or pushes to someone's real remote unasked; the template opts
    /// in explicitly.
    #[serde(default = "default_auto_commit")]
    auto_commit: bool,
}

impl Default for LocalDevConfig {
    fn default() -> Self {
        Self {
            auto_commit: default_auto_commit(),
        }
    }
}

fn default_auto_commit() -> bool {
    false
}

/// Env vars are dev overrides (set by the gbandit-dev alias); defaults point at prod.
pub(crate) fn auth_origin() -> String {
    std::env::var("GBANDIT_AUTH_ORIGIN").unwrap_or_else(|_| "https://auth.gbandit.com".into())
}

pub(crate) fn docs_origin() -> String {
    std::env::var("GBANDIT_DOCS_ORIGIN").unwrap_or_else(|_| "https://docs.gbandit.com".into())
}

pub(crate) fn platform_api_origin() -> String {
    std::env::var("GBANDIT_PLATFORM_API_ORIGIN")
        .unwrap_or_else(|_| "https://platform.gbandit.com/api".into())
}

/// The platform web UI, derived from the API origin (same host minus `/api`).
pub(crate) fn platform_web_origin() -> String {
    let api = platform_api_origin();
    api.strip_suffix("/api").map(str::to_string).unwrap_or(api)
}

pub(crate) fn resolve_project(cli_project: Option<String>) -> Result<String> {
    if let Some(project) = cli_project {
        return Ok(project);
    }
    Ok(read_gbandit_jsonc()?.project)
}

/// Like `resolve_project` but returns the full config. When `--project`
/// is passed without gbandit.jsonc we synthesise defaults so deploys can
/// run from outside a checked-in workspace.
pub(crate) fn load_project_config(cli_project: Option<String>) -> Result<ProjectConfig> {
    match cli_project {
        Some(project) => match read_gbandit_jsonc() {
            Ok(mut cfg) => {
                cfg.project = project;
                Ok(cfg)
            }
            Err(_) => Ok(ProjectConfig {
                project,
                title: None,
                local_dev: LocalDevConfig::default(),
            }),
        },
        None => read_gbandit_jsonc(),
    }
}

fn read_gbandit_jsonc() -> Result<ProjectConfig> {
    let path = PathBuf::from("gbandit.jsonc");
    if !path.is_file() {
        bail!("no --project flag and no gbandit.jsonc found in the current directory");
    }
    let text = fs::read_to_string(&path).context("failed to read gbandit.jsonc")?;
    parse_config(&text)
}

fn parse_config(text: &str) -> Result<ProjectConfig> {
    let value = jsonc_parser::parse_to_serde_value(text, &Default::default())
        .map_err(|err| anyhow::anyhow!("failed to parse gbandit.jsonc: {err}"))?
        .context("gbandit.jsonc is empty")?;
    serde_json::from_value(value).context(
        "failed to read gbandit.jsonc \
         — schema: https://docs.gbandit.com/deploy#gbandit-jsonc \
         (or run: gbandit docs deploy)",
    )
}

#[cfg(test)]
mod tests {
    use super::parse_config;

    #[test]
    fn parses_jsonc_and_ignores_server_owned_fields() {
        let cfg = parse_config(
            r#"{
                // comment
                "project": "my-game",
                "title": "My Game",
                "frontend": { "dockerfile": "frontend/Dockerfile", "context": "frontend" },
                "backend": { "dockerfile": "backend/Dockerfile", "context": "backend", "volume": { "sqlite": true } },
                "local_dev": { "auto_commit": true },
            }"#,
        )
        .unwrap();
        assert_eq!(cfg.project, "my-game");
        assert_eq!(cfg.title.as_deref(), Some("My Game"));
        assert!(cfg.auto_commit());
    }

    #[test]
    fn minimal_config_defaults_auto_commit_off() {
        let cfg = parse_config(r#"{ "project": "p" }"#).unwrap();
        assert_eq!(cfg.project, "p");
        assert_eq!(cfg.title, None);
        assert!(!cfg.local_dev.auto_commit);
    }
}
