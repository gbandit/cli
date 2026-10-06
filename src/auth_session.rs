use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use crate::config::{auth_origin, platform_api_origin};
use crate::http::{ApiError, http_client, parse_error, parse_json, problem_types};
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
    /// A guest's deploys end with a link to claim them; an account's do not.
    is_guest: bool,
}

/// What `/api/cli/login/start` asks the browser to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LoginKind {
    Login,
    Claim,
}

/// RFC 8628 §3.2's names; there is no user code, the link says it all.
#[derive(Debug, Deserialize)]
struct DeviceAuthorizationResponse {
    device_code: String,
    verification_uri: String,
    expires_in: i64,
    interval: u64,
}

/// A device login the browser has not approved yet, kept on disk so that a
/// later command can collect it: `gbandit login` without a terminal hands the
/// link to a person and exits, and a guest's claim may be approved days after the deploy
/// that printed it. Every command that needs a session collects it first.
#[derive(Debug, Serialize, Deserialize)]
struct PendingLogin {
    auth_origin: String,
    kind: LoginKind,
    device_code: String,
    verification_uri: String,
    expires_at: String,
    interval: u64,
}

impl PendingLogin {
    fn is_expired(&self) -> bool {
        chrono::DateTime::parse_from_rfc3339(&self.expires_at)
            .map(|expiry| expiry.with_timezone(&chrono::Utc) <= chrono::Utc::now())
            .unwrap_or(true)
    }
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

/// What one look at a login waiting for the browser found.
enum Checked {
    /// Approved and saved; says who the CLI is now.
    SignedIn(String),
    Pending,
    Denied,
    /// Expired, or the server no longer knows the device code.
    Expired,
}

const LOGIN_DENIED: &str = "The login was denied in the browser.";
const LOGIN_EXPIRED: &str =
    "The login expired before it was approved. Run `gbandit login` to start over.";

pub(crate) struct CliAuth {
    pub(crate) token: String,
    pub(crate) platform_api_origin: String,
}

/// `gbandit login`. In a terminal it waits for the browser, and stopping the
/// wait abandons the login: the device code dies with the process. Without
/// one, nobody can sit in that wait (an agent hands the link to its user), so
/// the request is kept on disk and the next command collects the session once
/// it is approved.
pub(crate) async fn login(printer: &Printer) -> Result<()> {
    // A guest CLI sends its own session along, so approving folds the guest
    // and its projects into the account instead of leaving them behind. The
    // server ignores a token that is not a guest's.
    let guest_session_token = load_credentials().ok().map(|c| c.session_token);
    clear_pending()?;
    let pending = start_device_login(LoginKind::Login, guest_session_token, None).await?;

    printer.progress("Open this URL and approve the login:");
    printer.progress(&pending.verification_uri);
    if webbrowser::open(&pending.verification_uri).is_ok() {
        printer.progress("Opened browser window.");
    }
    if !std::io::stdin().is_terminal() {
        save_pending(&pending)?;
        printer.progress(
            "Once it is approved, the next gbandit command picks up the login. `gbandit login --poll` checks.",
        );
        return Ok(());
    }
    printer.progress("Waiting for approval in the browser...");

    loop {
        match check(&pending).await? {
            Checked::SignedIn(message) => {
                printer.progress(message);
                return Ok(());
            }
            Checked::Pending => tokio::time::sleep(Duration::from_secs(pending.interval)).await,
            Checked::Denied => bail!(LOGIN_DENIED),
            Checked::Expired => bail!(LOGIN_EXPIRED),
        }
    }
}

/// `gbandit login --poll`: one look at a login waiting for approval. Fails
/// while it is still waiting, so a script can tell the two apart.
pub(crate) async fn login_poll(printer: &Printer) -> Result<()> {
    let Some(pending) = load_pending()? else {
        bail!("No login is waiting for approval. Run `gbandit login` to start one.");
    };
    match check(&pending).await? {
        Checked::SignedIn(message) => {
            printer.progress(message);
            Ok(())
        }
        Checked::Pending => bail!(
            "Still waiting for approval at {}.",
            pending.verification_uri
        ),
        Checked::Denied => bail!(LOGIN_DENIED),
        Checked::Expired => bail!(LOGIN_EXPIRED),
    }
}

/// The claim link a guest's deploy ends with: approving it in the browser
/// moves the guest's projects into a Google account, and the next command
/// here picks up a session for that account. One claim serves every deploy
/// until it is approved or expires. `None` for an account, for the runs that
/// have no person at a browser, and when the auth server cannot be reached,
/// since the deploy itself has already succeeded.
pub(crate) async fn claim_link(redirect: &str) -> Option<String> {
    if workload_token_file().is_some() || std::env::var("GBANDIT_SESSION_TOKEN").is_ok() {
        return None;
    }
    let credentials = load_credentials().ok()?;
    if !credentials.is_guest {
        return None;
    }
    if let Ok(Some(pending)) = load_pending()
        && !pending.is_expired()
    {
        // A login in progress takes the guest's projects along too.
        return (pending.kind == LoginKind::Claim).then_some(pending.verification_uri);
    }
    let pending = start_device_login(
        LoginKind::Claim,
        Some(credentials.session_token),
        Some(redirect),
    )
    .await
    .ok()?;
    save_pending(&pending).ok()?;
    Some(pending.verification_uri)
}

/// Collects a login approved since the last command, so a claim made in the
/// browser takes effect here without anyone running `gbandit login`. Anything
/// short of an answer leaves the request for next time. Stderr, since the
/// command this runs ahead of may own stdout. True when it signed the CLI in.
async fn collect_pending_login() -> bool {
    if let Ok(Some(pending)) = load_pending()
        && let Ok(Checked::SignedIn(message)) = check(&pending).await
    {
        eprintln!("{message}");
        return true;
    }
    false
}

async fn start_device_login(
    kind: LoginKind,
    guest_session_token: Option<String>,
    redirect: Option<&str>,
) -> Result<PendingLogin> {
    let auth_origin = auth_origin();
    let response = http_client()
        .post(format!("{auth_origin}/api/cli/login/start"))
        .json(&serde_json::json!({
            "kind": kind,
            "guest_session_token": guest_session_token,
            "redirect": redirect,
        }))
        .send()
        .await
        .with_context(|| {
            format!(
                "could not reach the auth server at {auth_origin} — check your network connection"
            )
        })?;
    let start: DeviceAuthorizationResponse = parse_json(response).await?;
    Ok(PendingLogin {
        auth_origin,
        kind,
        device_code: start.device_code,
        verification_uri: start.verification_uri,
        expires_at: (chrono::Utc::now() + chrono::Duration::seconds(start.expires_in)).to_rfc3339(),
        interval: start.interval,
    })
}

/// One poll of the token endpoint, which answers a login that is not
/// approved yet with a problem named after RFC 8628 §3.5's errors. An approved
/// login replaces the stored credentials, and every final answer forgets the
/// request.
async fn check(pending: &PendingLogin) -> Result<Checked> {
    let response = http_client()
        .post(format!("{}/api/cli/login/poll", pending.auth_origin))
        .json(&serde_json::json!({ "device_code": pending.device_code }))
        .send()
        .await
        .with_context(|| {
            format!(
                "could not reach the auth server at {} — check your network connection",
                pending.auth_origin
            )
        })?;
    let checked = if response.status().is_success() {
        Checked::SignedIn(save_approved_login(pending, parse_json(response).await?)?)
    } else {
        let error = parse_error(response).await;
        if error.is(problem_types::AUTHORIZATION_PENDING) {
            return Ok(Checked::Pending);
        } else if error.is(problem_types::ACCESS_DENIED) {
            Checked::Denied
        } else if error.is(problem_types::EXPIRED_TOKEN) || error.is(problem_types::INVALID_GRANT) {
            Checked::Expired
        } else {
            return Err(error).context("the auth server refused the login poll");
        }
    };
    clear_pending()?;
    Ok(checked)
}

fn save_approved_login(
    pending: &PendingLogin,
    completed: CliLoginPollCompleteResponse,
) -> Result<String> {
    let previous = load_credentials().ok();
    let credentials = StoredCredentials {
        auth_origin: pending.auth_origin.clone(),
        platform_api_origin: platform_api_origin(),
        session_token: completed.session_token,
        session_expires_at: completed.session_expires_at,
        user_id: completed.user_id,
        email: completed.email,
        name: completed.name,
        // Approving always goes through Google.
        is_guest: false,
    };
    save_credentials(&credentials)?;

    let who = display_name(&credentials);
    let mut message = match pending.kind {
        LoginKind::Claim => format!("Your game was claimed — signed in as {who}."),
        LoginKind::Login => format!("Logged in as {who}."),
    };
    // The browser decides which account the login completes as. A guest the
    // CLI held was folded into it, but another account is simply replaced,
    // and that should not be discovered later.
    if let Some(previous) = previous
        && !previous.is_guest
        && previous.user_id != credentials.user_id
    {
        message.push_str(&format!(
            "\nThis replaces the previous login ({}).",
            display_name(&previous)
        ));
    }
    Ok(message)
}

fn display_name(credentials: &StoredCredentials) -> &str {
    credentials
        .email
        .as_deref()
        .or(credentials.name.as_deref())
        .unwrap_or(&credentials.user_id)
}

#[derive(Debug, Deserialize)]
struct AnonymousLoginResponse {
    user_id: String,
    email: Option<String>,
    name: Option<String>,
    username: Option<String>,
    expires_at: String,
}

/// Explicit, browserless guest creation (`gbandit login --guest`). The
/// server auto-generates the Username; the session cookie value doubles as
/// the CLI session token.
pub(crate) async fn login_guest(printer: &Printer) -> Result<()> {
    if let Ok(existing) = load_credentials() {
        let who = display_name(&existing);
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
        is_guest: true,
    };
    save_credentials(&credentials)?;
    match me.username.as_deref() {
        Some(username) => printer.progress(format!("Created guest account @{username}.")),
        None => printer.progress("Created guest account."),
    }
    printer.progress(
        "Link Google any time with `gbandit login` — your projects are kept, and so is the username if you want it.",
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

/// The git identity for a deploy's auto-commit when git has none configured:
/// the gbandit account's name and a noreply address, so a repo pushed to a
/// public remote never carries the account's real email.
pub(crate) fn git_identity() -> Result<crate::git::Identity> {
    let credentials = load_credentials()?;
    Ok(crate::git::Identity {
        name: credentials.name.unwrap_or_else(|| "gbandit".to_string()),
        email: format!("{}@users.noreply.gbandit.com", credentials.user_id),
    })
}

pub(crate) async fn whoami(printer: &Printer) -> Result<()> {
    collect_pending_login().await;
    let credentials = load_credentials()?;
    printer.progress(format!(
        "Logged in as {}{}",
        display_name(&credentials),
        if credentials.is_guest { " (guest)" } else { "" }
    ));
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
        Ok(Ok(_)) => printer.progress("  Session is valid."),
        _ => printer
            .progress("  Session is expired or invalid. Run `gbandit login` to re-authenticate."),
    }
    if let Some(pending) = load_pending()? {
        let what = match pending.kind {
            LoginKind::Claim => "Claim your game",
            LoginKind::Login => "Approve the login",
        };
        printer.progress(format!("  {what} at {}.", pending.verification_uri));
    }

    Ok(())
}

pub(crate) async fn logout(printer: &Printer) -> Result<()> {
    clear_pending()?;
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

/// The auth service's refusal when the stored session no longer
/// authenticates.
async fn cli_access_token(
    credentials: &StoredCredentials,
) -> Result<std::result::Result<String, ApiError>> {
    let client = http_client();
    let response = client
        .post(format!("{}/api/cli/token", credentials.auth_origin))
        .json(&serde_json::json!({
            "session_token": credentials.session_token,
        }))
        .send()
        .await
        .context("failed to mint platform access token")?;
    if response.status() == StatusCode::UNAUTHORIZED {
        return Ok(Err(parse_error(response).await));
    }
    let token: AccessTokenResponse = parse_json(response).await?;
    Ok(Ok(token.access_token))
}

fn session_rejected_message(credentials: &StoredCredentials) -> String {
    let expired = chrono::DateTime::parse_from_rfc3339(&credentials.session_expires_at)
        .map(|expiry| expiry.with_timezone(&chrono::Utc) <= chrono::Utc::now())
        .unwrap_or(false);
    if expired {
        format!("Session expired at {}", credentials.session_expires_at)
    } else {
        "Session is no longer valid".to_string()
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
    // A login waits only minutes and may be all the CLI has, so it is
    // collected up front. Approving a claim folds this guest away, which is
    // what ends its session, so a claim is only asked about once the session
    // stops working.
    let pending = load_pending().ok().flatten().map(|pending| pending.kind);
    if pending == Some(LoginKind::Login) {
        collect_pending_login().await;
    }
    let mut credentials = load_credentials()?;
    let mut token = cli_access_token(&credentials).await?;
    if token.is_err() && pending == Some(LoginKind::Claim) && collect_pending_login().await {
        credentials = load_credentials()?;
        token = cli_access_token(&credentials).await?;
    }
    // The server's detail cannot say whether the session expired, so the
    // problem keeps its type and request id but says that instead; its
    // Display adds the `gbandit login` hint every 401 gets.
    let token = match token {
        Ok(token) => token,
        Err(mut error) => {
            error.detail = Some(session_rejected_message(&credentials));
            return Err(error.into());
        }
    };
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
    let tokens: AgentTokensResponse = parse_json(response)
        .await
        .context("auth-service refused this pod's workload identity")?;
    Ok(tokens.cli_token.access_token)
}

fn save_credentials(credentials: &StoredCredentials) -> Result<()> {
    write_private_json(&credentials_path()?, credentials)
}

fn save_pending(pending: &PendingLogin) -> Result<()> {
    write_private_json(&pending_path()?, pending)
}

/// The device code is as good as a session once the browser approves, so it
/// is kept like one.
fn load_pending() -> Result<Option<PendingLogin>> {
    let path = pending_path()?;
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    // An unreadable file is a request nobody can collect; drop it.
    Ok(serde_json::from_slice(&bytes)
        .inspect_err(|_| {
            fs::remove_file(&path).ok();
        })
        .ok())
}

fn clear_pending() -> Result<()> {
    let path = pending_path()?;
    match fs::remove_file(&path) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
            Err(err).with_context(|| format!("failed to remove {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// Written to a temp file in the same directory and renamed into place: the
/// file carries mode 0600 before the first byte lands, and a crash mid-write
/// never leaves a half-written file. Windows has no mode bits; there
/// `%APPDATA%` is already private to the user via the profile ACL.
fn write_private_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .context("credentials path must have a parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create credentials dir {}", parent.display()))?;
    let json = serde_json::to_vec_pretty(value)?;
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
            is_guest: false,
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
    config_file(&format!("credentials-{}.json", credentials_identity()))
}

fn pending_path() -> Result<PathBuf> {
    config_file(&format!("pending-login-{}.json", credentials_identity()))
}

fn config_file(filename: &str) -> Result<PathBuf> {
    let config_dir = dirs::config_dir().context("failed to determine config directory")?;
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

    use super::{StoredCredentials, read_credentials_file, write_private_json};

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
            is_guest: false,
        }
    }

    #[test]
    fn credentials_file_is_owner_only_and_a_loose_file_gets_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("gbandit")
            .join("credentials-example.test.json");

        write_private_json(&path, &credentials("first")).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(read_credentials_file(&path).unwrap().session_token, "first");

        // Overwriting replaces the content and leaves no temp file behind.
        write_private_json(&path, &credentials("second")).unwrap();
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
