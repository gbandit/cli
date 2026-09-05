use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use crate::config::{auth_origin, platform_api_origin};
use crate::http::{http_client, parse_error, parse_json};
use crate::printer::Printer;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct StoredCredentials {
    auth_origin: String,
    platform_api_origin: String,
    session_token: String,
    session_expires_at: String,
    user_id: String,
    email: Option<String>,
    name: Option<String>,
    /// The one-time "View your project" browser link has been printed.
    #[serde(default)]
    browser_handoff_shown: bool,
}

#[derive(Debug, Deserialize)]
struct CliLoginStartResponse {
    login_id: String,
    login_secret: String,
    authorize_url: String,
    expires_at: String,
    poll_interval_seconds: u64,
}

#[derive(Debug, Deserialize)]
struct AccessTokenResponse {
    access_token: String,
}

#[derive(Debug, Deserialize)]
struct AgentTokensResponse {
    cli_token: AccessTokenResponse,
}

#[derive(Debug, Deserialize)]
struct CliLoginPollCompleteResponse {
    session_token: String,
    session_expires_at: String,
    user_id: String,
    email: Option<String>,
    name: Option<String>,
}

pub(crate) struct CliAuth {
    pub(crate) token: String,
    pub(crate) platform_api_origin: String,
}

pub(crate) async fn login(printer: &Printer) -> Result<()> {
    let client = http_client();
    let auth_origin = auth_origin();
    // If we're already logged in (typically as a guest), prove ownership of
    // that session so the auth server upgrades it to Google in place instead
    // of letting the browser complete the login as a guest again.
    let previous = load_credentials().ok();
    let upgrade_session_token = previous.as_ref().map(|c| c.session_token.clone());
    let response = client
        .post(format!("{auth_origin}/api/cli/login/start"))
        .json(&serde_json::json!({
            "upgrade_session_token": upgrade_session_token,
        }))
        .send()
        .await
        .with_context(|| {
            format!(
                "could not reach the auth server at {auth_origin} — check your network connection"
            )
        })?;
    let start: CliLoginStartResponse = parse_json(response).await?;
    let login_expires_at = chrono::DateTime::parse_from_rfc3339(&start.expires_at).ok();

    printer.progress("Open this URL and approve the login:");
    printer.progress(&start.authorize_url);
    if webbrowser::open(&start.authorize_url).is_ok() {
        printer.progress("Opened browser window.");
    }
    printer.progress("Waiting for approval in the browser...");

    loop {
        if let Some(expiry) = login_expires_at
            && chrono::Utc::now() >= expiry
        {
            bail!(
                "login request expired before it was approved in the browser — run `gbandit login` to start over"
            );
        }

        let response = client
            .post(format!("{auth_origin}/api/cli/login/poll"))
            .json(&serde_json::json!({
                "login_id": start.login_id,
                "login_secret": start.login_secret,
            }))
            .send()
            .await
            .with_context(|| {
                format!("could not reach the auth server at {auth_origin} — check your network connection")
            })?;

        if response.status() == StatusCode::ACCEPTED {
            tokio::time::sleep(Duration::from_secs(start.poll_interval_seconds)).await;
            continue;
        }

        let completed: CliLoginPollCompleteResponse = parse_json(response).await?;
        let credentials = StoredCredentials {
            auth_origin,
            platform_api_origin: platform_api_origin(),
            session_token: completed.session_token,
            session_expires_at: completed.session_expires_at,
            user_id: completed.user_id,
            email: completed.email,
            name: completed.name,
            browser_handoff_shown: false,
        };
        save_credentials(&credentials)?;
        printer.progress(format!(
            "Logged in as {}",
            credentials
                .email
                .clone()
                .or(credentials.name.clone())
                .unwrap_or(credentials.user_id.clone())
        ));
        // The browser decides which account the login completes as, so it can
        // differ from the one the CLI held — say so rather than leaving the
        // switch to be discovered later.
        if let Some(previous) = &previous
            && previous.user_id != credentials.user_id
        {
            let was = previous
                .email
                .as_deref()
                .or(previous.name.as_deref())
                .unwrap_or(&previous.user_id);
            printer.progress(format!("This replaces the previous login ({was})."));
        }
        printer.progress(format!(
            "Session expires at {}",
            credentials.session_expires_at
        ));
        break;
    }

    Ok(())
}

#[derive(Debug, Deserialize)]
struct AnonymousLoginResponse {
    user_id: String,
    email: Option<String>,
    name: Option<String>,
    username: Option<String>,
    expires_at: String,
}

#[derive(Debug, Deserialize)]
struct CliHandoffResponse {
    url: String,
}

/// Explicit, browserless guest creation (`gbandit login --guest`). The
/// server auto-generates the Username; the session cookie value doubles as
/// the CLI session token.
pub(crate) async fn login_guest(printer: &Printer) -> Result<()> {
    if let Ok(existing) = load_credentials() {
        let who = existing
            .email
            .as_deref()
            .or(existing.name.as_deref())
            .unwrap_or(&existing.user_id);
        bail!(
            "Already logged in as {who}. Run `gbandit logout` first if you really want a fresh guest account."
        );
    }

    let client = http_client();
    let auth_origin = auth_origin();
    let response = client
        .post(format!("{auth_origin}/api/anonymous"))
        .send()
        .await
        .with_context(|| {
            format!(
                "could not reach the auth server at {auth_origin} — check your network connection"
            )
        })?;
    let session_token = session_cookie_value(&response).context(
        "auth server response did not include a session cookie — is the server up to date?",
    )?;
    let me: AnonymousLoginResponse = parse_json(response).await?;

    let credentials = StoredCredentials {
        auth_origin,
        platform_api_origin: platform_api_origin(),
        session_token,
        session_expires_at: me.expires_at,
        user_id: me.user_id,
        email: me.email,
        name: me.name,
        browser_handoff_shown: false,
    };
    save_credentials(&credentials)?;
    match me.username.as_deref() {
        Some(username) => printer.progress(format!("Created guest account @{username}.")),
        None => printer.progress("Created guest account."),
    }
    printer.progress(
        "Upgrade to Google any time with `gbandit login` — your username and projects are kept.",
    );
    Ok(())
}

fn session_cookie_value(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|cookie| {
            // Prod frames the session as `__Host-gbandit_session`; dev, over
            // plain http, cannot use the prefix.
            let cookie = cookie.strip_prefix("__Host-").unwrap_or(cookie);
            cookie
                .strip_prefix("gbandit_session=")?
                .split(';')
                .next()
                .filter(|token| !token.is_empty())
                .map(str::to_string)
        })
}

/// True when a deploy can authenticate without creating anything: a workload
/// identity, an env-provided session or a stored credentials file.
pub(crate) fn has_credentials() -> bool {
    workload_token_file().is_some() || load_credentials().is_ok()
}

/// One-time "View your project" link after the first successful deploy: mints
/// a single-use browser handoff code so the CLI's user (typically a guest with
/// no browser cookie) opens the platform already signed in. Best-effort — any
/// failure just skips the link.
pub(crate) async fn first_deploy_handoff_link(redirect: &str) -> Option<String> {
    // Agent pods and env-provided sessions (e2e, CI) have no human at a browser.
    if workload_token_file().is_some() || std::env::var("GBANDIT_SESSION_TOKEN").is_ok() {
        return None;
    }
    let mut credentials = load_credentials().ok()?;
    if credentials.browser_handoff_shown {
        return None;
    }

    let client = http_client();
    let response = client
        .post(format!("{}/api/cli/handoff", credentials.auth_origin))
        .json(&serde_json::json!({
            "session_token": credentials.session_token,
            "redirect": redirect,
        }))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let handoff: CliHandoffResponse = response.json().await.ok()?;

    credentials.browser_handoff_shown = true;
    save_credentials(&credentials).ok();
    Some(handoff.url)
}

pub(crate) async fn whoami(printer: &Printer) -> Result<()> {
    let credentials = load_credentials()?;
    let display_name = credentials
        .email
        .as_deref()
        .or(credentials.name.as_deref())
        .unwrap_or(&credentials.user_id);
    printer.progress(format!("Logged in as {display_name}"));
    if let Some(name) = &credentials.name {
        printer.progress(format!("  Name:    {name}"));
    }
    if let Some(email) = &credentials.email {
        printer.progress(format!("  Email:   {email}"));
    }
    printer.progress(format!("  User ID: {}", credentials.user_id));
    printer.progress(format!(
        "  Session expires at {}",
        credentials.session_expires_at
    ));

    match cli_access_token(&credentials).await {
        Ok(_) => printer.progress("  Session is valid."),
        Err(_) => printer
            .progress("  Session is expired or invalid. Run `gbandit login` to re-authenticate."),
    }

    Ok(())
}

pub(crate) async fn logout(printer: &Printer) -> Result<()> {
    let path = credentials_path()?;
    let Ok(credentials) = load_credentials() else {
        // Missing file → already logged out; unreadable file → just clear it.
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove credentials file {}", path.display()))?;
            printer.progress("Logged out.");
        } else {
            printer.progress("Already logged out.");
        }
        return Ok(());
    };

    // Best-effort server-side revoke: local credentials are removed even when
    // it fails, so logout can't get wedged on a dead session or offline network.
    let client = http_client();
    let revoke_error = match client
        .post(format!("{}/api/cli/logout", credentials.auth_origin))
        .json(&serde_json::json!({
            "session_token": credentials.session_token,
        }))
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => None,
        Ok(response) => Some(parse_error(response).await.to_string()),
        Err(err) => Some(err.to_string()),
    };

    if path.exists() {
        fs::remove_file(&path)
            .with_context(|| format!("failed to remove credentials file {}", path.display()))?;
    }
    match revoke_error {
        None => printer.progress("Logged out."),
        Some(err) => printer.progress(format!(
            "Logged out locally, but the session could not be revoked on the server: {err}"
        )),
    }
    Ok(())
}

async fn cli_access_token(credentials: &StoredCredentials) -> Result<String> {
    let client = http_client();
    let response = client
        .post(format!("{}/api/cli/token", credentials.auth_origin))
        .json(&serde_json::json!({
            "session_token": credentials.session_token,
            "audience": "platform-api",
        }))
        .send()
        .await
        .context("failed to mint platform access token")?;
    // A 401 here means the stored session no longer authenticates. The raw
    // body is unhelpful ("401 Unauthorized: request failed"), so say whether
    // it expired and point at `gbandit login`.
    if response.status() == StatusCode::UNAUTHORIZED {
        bail!(session_rejected_message(credentials));
    }
    let token: AccessTokenResponse = parse_json(response).await?;
    Ok(token.access_token)
}

fn session_rejected_message(credentials: &StoredCredentials) -> String {
    let expired = chrono::DateTime::parse_from_rfc3339(&credentials.session_expires_at)
        .map(|expiry| expiry.with_timezone(&chrono::Utc) <= chrono::Utc::now())
        .unwrap_or(false);
    if expired {
        format!(
            "Session expired at {}. Run `gbandit login` to re-authenticate.",
            credentials.session_expires_at
        )
    } else {
        "Session is no longer valid. Run `gbandit login` to re-authenticate.".to_string()
    }
}

/// Inside a Pi Agent pod the CLI has a workload identity and exchanges it for
/// a fresh access token on every run; otherwise it loads disk credentials and
/// mints one from the stored session.
pub(crate) async fn load_auth() -> Result<CliAuth> {
    if let Some(token_file) = workload_token_file() {
        return Ok(CliAuth {
            token: workload_access_token(&token_file).await?,
            platform_api_origin: platform_api_origin(),
        });
    }
    let credentials = load_credentials()?;
    let token = cli_access_token(&credentials).await?;
    Ok(CliAuth {
        token,
        platform_api_origin: credentials.platform_api_origin,
    })
}

/// The projected Kubernetes ServiceAccount token platform-api mounts into
/// every Pi Agent pod. Set means "this is an agent"; a human's shell never
/// has it.
fn workload_token_file() -> Option<PathBuf> {
    std::env::var_os("GBANDIT_WORKLOAD_TOKEN_FILE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// auth-service asks the API server to vouch for the token, checks the agent's
/// user and project, and answers with tokens that live minutes. kubelet keeps
/// the file itself fresh, so there is nothing to cache here.
async fn workload_access_token(token_file: &Path) -> Result<String> {
    let workload_token = fs::read_to_string(token_file).with_context(|| {
        format!(
            "failed to read workload identity token {}",
            token_file.display()
        )
    })?;
    let response = http_client()
        .post(format!("{}/api/agent/session-tokens", auth_origin()))
        .bearer_auth(workload_token.trim())
        .send()
        .await
        .context("failed to exchange workload identity for an access token")?;
    // auth-service answers plain text, not the platform's JSON error shape.
    if !response.status().is_success() {
        let status = response.status();
        let detail = response.text().await.unwrap_or_default();
        bail!("auth-service refused this pod's workload identity ({status}): {detail}");
    }
    let tokens: AgentTokensResponse = parse_json(response).await?;
    Ok(tokens.cli_token.access_token)
}

fn save_credentials(credentials: &StoredCredentials) -> Result<()> {
    write_credentials_file(&credentials_path()?, credentials)
}

/// The session token is written to a temp file in the same directory and
/// renamed into place: the file carries mode 0600 before the first byte lands,
/// and a crash mid-write never leaves a half-written file. Windows has no mode
/// bits; there `%APPDATA%` is already private to the user via the profile ACL.
fn write_credentials_file(path: &Path, credentials: &StoredCredentials) -> Result<()> {
    let parent = path
        .parent()
        .context("credentials path must have a parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create credentials dir {}", parent.display()))?;
    let json = serde_json::to_vec_pretty(credentials)?;
    let mut file = tempfile::Builder::new()
        .prefix(".credentials-")
        .tempfile_in(parent)
        .with_context(|| format!("failed to create temp file in {}", parent.display()))?;
    file.write_all(&json)?;
    file.persist(path)
        .with_context(|| format!("failed to write credentials file {}", path.display()))?;
    Ok(())
}

fn load_credentials() -> Result<StoredCredentials> {
    // Test/CI bypass: synthesise credentials from env vars when both
    // `GBANDIT_SESSION_TOKEN` and `GBANDIT_USER_ID` are set. The auth-service
    // validates the session token against its DB on every `/api/cli/token`
    // call, so a bogus token here just produces a 401 — there's no
    // local-only auth check to spoof. Used by the e2e journey suite and
    // any future non-interactive deploy paths (CI, Pi Agent).
    if let Ok(session_token) = std::env::var("GBANDIT_SESSION_TOKEN")
        && let Ok(user_id) = std::env::var("GBANDIT_USER_ID")
    {
        return Ok(StoredCredentials {
            auth_origin: auth_origin(),
            platform_api_origin: platform_api_origin(),
            session_token,
            // Actual expiry is enforced server-side against the DB row.
            session_expires_at: "2099-01-01T00:00:00Z".to_string(),
            user_id,
            email: None,
            name: None,
            browser_handoff_shown: true,
        });
    }

    read_credentials_file(&credentials_path()?)
}

fn read_credentials_file(path: &Path) -> Result<StoredCredentials> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            bail!("You are not logged in. Run `gbandit login` to get started.")
        }
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to read credentials file {}", path.display()));
        }
    };
    restrict_to_owner(path)?;
    serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "credentials file {} is unreadable — run `gbandit login` to re-authenticate",
            path.display()
        )
    })
}

/// Files written by older CLI versions got their mode from the umask, typically
/// 0644. Tighten to 0600 on sight and say so, since the token was exposed.
#[cfg(unix)]
fn restrict_to_owner(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    if perms.mode() & 0o077 == 0 {
        return Ok(());
    }
    perms.set_mode(0o600);
    fs::set_permissions(path, perms)
        .with_context(|| format!("failed to restrict permissions on {}", path.display()))?;
    eprintln!(
        "warning: {} was readable by other users on this machine; permissions tightened to 0600.",
        path.display()
    );
    Ok(())
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path) -> Result<()> {
    Ok(())
}

fn credentials_path() -> Result<PathBuf> {
    let config_dir = dirs::config_dir().context("failed to determine config directory")?;
    let filename = format!("credentials-{}.json", credentials_identity());
    Ok(config_dir.join("gbandit").join(filename))
}

/// Scopes the stored login to one deployment, derived the same way the frontends
/// derive their base domain: the last two labels of the auth host. So all
/// subdomains of an instance (auth./platform.) share one login, while distinct
/// deployments (wt1.gbandit, main.gbandit, gbandit.com) never clobber each other.
fn credentials_identity() -> String {
    let origin = auth_origin();
    let host = reqwest::Url::parse(&origin)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| origin.clone());
    if host == "localhost" || host == "127.0.0.1" {
        return "localhost".to_string();
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() >= 2 {
        labels[labels.len() - 2..].join(".")
    } else {
        host
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::{StoredCredentials, read_credentials_file, write_credentials_file};

    fn mode(path: &std::path::Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn credentials(token: &str) -> StoredCredentials {
        StoredCredentials {
            auth_origin: "https://auth.example.test".into(),
            platform_api_origin: "https://platform.example.test".into(),
            session_token: token.into(),
            session_expires_at: "2099-01-01T00:00:00Z".into(),
            user_id: "user-1".into(),
            email: Some("u@example.test".into()),
            name: None,
            browser_handoff_shown: false,
        }
    }

    #[test]
    fn credentials_file_is_owner_only_and_a_loose_file_gets_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("gbandit")
            .join("credentials-example.test.json");

        write_credentials_file(&path, &credentials("first")).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(read_credentials_file(&path).unwrap().session_token, "first");

        // Overwriting replaces the content and leaves no temp file behind.
        write_credentials_file(&path, &credentials("second")).unwrap();
        assert_eq!(
            read_credentials_file(&path).unwrap().session_token,
            "second"
        );
        let entries: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![path.file_name().unwrap()]);

        // A file left by an older CLI with umask permissions is tightened on read.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            read_credentials_file(&path).unwrap().session_token,
            "second"
        );
        assert_eq!(mode(&path), 0o600);
    }
}
