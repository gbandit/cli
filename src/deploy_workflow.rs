use std::cell::Cell;
use std::fs;
use std::io::IsTerminal;

use anyhow::{Result, bail};
use reqwest::multipart::{Form, Part};

use crate::config::ProjectConfig;
use crate::deploy_archive::build_project_archive;
use crate::git;
use crate::http::ApiError;
use crate::pipeline_watch::{RunFailed, watch_pipeline};
use crate::platform_client::{DeployPipeline, PlatformClient};
use crate::printer::Printer;
use crate::scaffold::title_from_slug;

/// Everything the deploy command chose at the CLI layer, built once and
/// threaded through the workflow as a unit.
pub(crate) struct DeployArgs {
    pub(crate) message: Option<String>,
    pub(crate) baseline: bool,
    pub(crate) create: bool,
    pub(crate) confirm_database_removal: bool,
    pub(crate) detach: bool,
    pub(crate) json: bool,
}

/// The expensive one-time work of a deploy, computed before the first upload
/// attempt. The confirmation retry re-uploads this as-is — the config cannot
/// change between the two attempts.
struct PreparedDeploy {
    client: PlatformClient,
    archive_bytes: Vec<u8>,
    commit_sha: Option<String>,
    deploy_message: Option<String>,
    /// The auto-commit this deploy made, taken back off if the deploy fails.
    auto_commit: Option<git::MadeCommit>,
}

/// How an attempt that did not fail ended.
enum Attempt {
    Succeeded,
    Detached,
    BaselineSkipped,
}

/// What the deploy is known to have done, which decides what happens to its
/// commit.
enum Outcome {
    Deployed,
    NotDeployed,
    /// A run was started but its end was not seen (`--detach`, or the event
    /// stream was lost).
    Unknown,
}

pub(crate) struct DeployWorkflow<'a> {
    printer: &'a Printer,
    run_started: Cell<bool>,
}

impl<'a> DeployWorkflow<'a> {
    pub(crate) fn new(printer: &'a Printer) -> Self {
        Self {
            printer,
            run_started: Cell::new(false),
        }
    }

    pub(crate) async fn deploy(&self, config: &ProjectConfig, args: &DeployArgs) -> Result<()> {
        let result = self.deploy_inner(config, args).await;
        if args.json
            && let Err(err) = &result
        {
            println!("{}", json_error_payload(err));
        }
        result
    }

    async fn deploy_inner(&self, config: &ProjectConfig, args: &DeployArgs) -> Result<()> {
        self.ensure_credentials(args).await?;
        let mut prepared = self.prepare(config, args).await?;

        let mut result = self
            .deploy_attempt(&prepared, config, args, args.confirm_database_removal)
            .await;

        // Reactive Google sign-in: guests may only deploy frontends (403
        // google_account_required). Interactively, offer the Google login —
        // the auth server upgrades the guest in place so the project stays
        // owned — mint fresh credentials, and retry the same archive.
        if !args.json
            && std::io::stdin().is_terminal()
            && let Err(err) = &result
            && let Some(api) = err.downcast_ref::<ApiError>()
            && api.has_code("google_account_required")
        {
            self.printer.progress(&api.error);
            if confirm_google_login()? {
                crate::auth_session::login(self.printer).await?;
                prepared.client = PlatformClient::from_saved_auth().await?;
                result = self
                    .deploy_attempt(&prepared, config, args, args.confirm_database_removal)
                    .await;
            }
        }

        // Reactive confirmation: the platform rejects a deploy that drops the
        // `database` field (409 database_removal_requires_confirmation). In an
        // interactive session, confirm and retry instead of making the user
        // rediscover the --confirm-database-removal flag. The retry re-uploads
        // the already-built archive — no second commit or push.
        if !args.confirm_database_removal
            && !args.json
            && std::io::stdin().is_terminal()
            && let Err(err) = &result
            && let Some(api) = err.downcast_ref::<ApiError>()
            && api.has_code("database_removal_requires_confirmation")
        {
            self.printer.progress(&api.error);
            confirm_database_removal_prompt(&config.project)?;
            result = self.deploy_attempt(&prepared, config, args, true).await;
        }

        let outcome = match &result {
            Ok(Attempt::Succeeded) => Outcome::Deployed,
            Ok(Attempt::BaselineSkipped) => Outcome::NotDeployed,
            Ok(Attempt::Detached) => Outcome::Unknown,
            Err(err) if err.downcast_ref::<RunFailed>().is_some() => Outcome::NotDeployed,
            Err(_) if self.run_started.get() => Outcome::Unknown,
            Err(_) => Outcome::NotDeployed,
        };
        if let Err(err) = self.settle_commit(&prepared, config, &outcome) {
            self.printer.progress(format!("{err:#}"));
        }

        // A CLI-created guest has no browser cookie, so the platform would
        // show them nothing — print a one-time signed-in link to their project
        // after the first successful deploy.
        if matches!(outcome, Outcome::Deployed) && !args.json {
            let redirect = format!(
                "{}/projects/{}",
                crate::config::platform_web_origin(),
                config.project
            );
            if let Some(url) = crate::auth_session::first_deploy_handoff_link(&redirect).await {
                self.printer.progress(format!(
                    "View your project in the browser (one-time sign-in link, valid 10 minutes): {url}"
                ));
            }
        }

        result.map(|_| ())
    }

    /// The deploy's commit follows its outcome: pushed once the deploy has
    /// succeeded, taken back off when it is known not to have deployed, and
    /// left local when the outcome is unknown, for the next deploy to push.
    fn settle_commit(
        &self,
        prepared: &PreparedDeploy,
        config: &ProjectConfig,
        outcome: &Outcome,
    ) -> Result<()> {
        match outcome {
            Outcome::Deployed => {
                if prepared.commit_sha.is_none() {
                    return Ok(());
                }
                if config.auto_commit() {
                    push(self.printer)?;
                } else if git::has_origin()? {
                    self.printer.progress(
                        "Skipping push to linked remote (auto_commit=false). Push when you want this commit on the remote.",
                    );
                }
            }
            Outcome::NotDeployed => {
                let Some(commit) = &prepared.auto_commit else {
                    return Ok(());
                };
                if git::undo_commit(commit)? {
                    self.printer.progress(
                        "Undid the auto-commit: your changes are back in the working tree, uncommitted.",
                    );
                } else {
                    self.printer.progress(format!(
                        "Left the auto-commit {} in place: the branch has moved past it since.",
                        &commit.sha[..12]
                    ));
                }
            }
            Outcome::Unknown => {
                if config.auto_commit() && prepared.commit_sha.is_some() && git::has_origin()? {
                    self.printer.progress(
                        "Not pushed to the linked remote yet: this deploy's outcome isn't known here. The next successful deploy pushes it.",
                    );
                }
            }
        }
        Ok(())
    }

    /// First-run guest intake: with no credentials at all, offer to create a
    /// guest interactively. Non-interactive runs never create accounts — one
    /// new owner per CI run is exactly the row growth we don't want.
    async fn ensure_credentials(&self, args: &DeployArgs) -> Result<()> {
        if crate::auth_session::has_credentials() {
            return Ok(());
        }
        if args.json || !std::io::stdin().is_terminal() {
            bail!(
                "You are not logged in. Run `gbandit login` to sign in with Google, or `gbandit login --guest` to create a guest account."
            );
        }
        if !crate::printer::confirm_yes("No account found. Continue as guest? [Y/n] ")? {
            bail!(
                "aborted: run `gbandit login` to sign in with Google, or `gbandit login --guest` for a guest account"
            );
        }
        crate::auth_session::login_guest(self.printer).await
    }

    /// One-time work: project existence/title sync, archive build and
    /// auto-commit. Never repeated by the confirmation retry.
    async fn prepare(&self, config: &ProjectConfig, args: &DeployArgs) -> Result<PreparedDeploy> {
        // Ensure the platform project exists (and its title matches
        // gbandit.jsonc) before any local side effects like the
        // auto-commit — answering "n" to the create prompt must leave the
        // working tree untouched.
        let client = PlatformClient::from_saved_auth().await?;
        self.ensure_project(&client, config, args.create, args.json)
            .await?;

        let timing = std::env::var("GBANDIT_TIMING").is_ok();
        let archive_started = std::time::Instant::now();
        let archive = build_project_archive()?;
        if timing {
            eprintln!(
                "@timing phase=archive ms={}",
                archive_started.elapsed().as_millis()
            );
        }
        let archive_bytes = fs::read(archive.path())?;

        // Committed after the archive is built, so nothing that can fail
        // before the upload is left behind as a commit.
        let commit = prepare_commit(self.printer, config.auto_commit(), args.message.as_deref())?;

        Ok(PreparedDeploy {
            client,
            archive_bytes,
            commit_sha: commit.sha,
            deploy_message: commit.deploy_message,
            auto_commit: commit.made,
        })
    }

    async fn deploy_attempt(
        &self,
        prepared: &PreparedDeploy,
        config: &ProjectConfig,
        args: &DeployArgs,
        confirm_database_removal: bool,
    ) -> Result<Attempt> {
        let upload = self
            .upload(prepared, config, args, confirm_database_removal)
            .await?;

        let Some(upload) = upload else {
            if args.json {
                println!("{}", serde_json::json!({ "status": "baseline_skipped" }));
            } else {
                self.printer.progress(
                    "Baseline deploy skipped — the project already has a succeeded deploy.",
                );
            }
            return Ok(Attempt::BaselineSkipped);
        };
        self.run_started.set(true);

        if args.json {
            println!("{}", serde_json::to_string(&upload)?);
        } else if args.detach {
            self.printer.progress(format!(
                "Started deployment for project {} (#{}).",
                config.project, upload.pipeline_run_id
            ));
        }

        if args.detach {
            return Ok(Attempt::Detached);
        }

        self.printer
            .progress(format!("Deploying project {}...", config.project));
        watch_pipeline(
            self.printer,
            prepared.client.http(),
            prepared.client.origin(),
            prepared.client.token(),
            upload.pipeline_run_id,
            "Deploy",
        )
        .await?;
        Ok(Attempt::Succeeded)
    }

    async fn upload(
        &self,
        prepared: &PreparedDeploy,
        config: &ProjectConfig,
        args: &DeployArgs,
        confirm_database_removal: bool,
    ) -> Result<Option<DeployPipeline>> {
        // Fresh per attempt: the platform dedupes on submission_id, so the
        // confirmed retry must not reuse the rejected attempt's id.
        let submission_id = uuid::Uuid::new_v4().to_string();
        let timing = std::env::var("GBANDIT_TIMING").is_ok();
        let upload_started = std::time::Instant::now();
        let make_form = || {
            let mut form = Form::new().text("submission_id", submission_id.clone());
            if let Some(sha) = prepared.commit_sha.as_deref() {
                form = form.text("commit_sha", sha.to_string());
            }
            if let Some(msg) = prepared.deploy_message.as_deref() {
                form = form.text("deploy_message", msg.to_string());
            }
            if confirm_database_removal {
                form = form.text("confirm_database_removal", "true");
            }
            Ok(form.part(
                "bundle",
                Part::bytes(prepared.archive_bytes.clone())
                    .file_name("project.tar.zst".to_string())
                    .mime_str("application/zstd")?,
            ))
        };
        let upload = if args.baseline {
            prepared
                .client
                .start_baseline_deploy(&config.project, make_form)
                .await?
        } else {
            Some(
                prepared
                    .client
                    .start_deploy(&config.project, make_form)
                    .await?,
            )
        };
        if timing {
            eprintln!(
                "@timing phase=upload ms={}",
                upload_started.elapsed().as_millis()
            );
        }
        Ok(upload)
    }

    /// Create-on-deploy (with confirmation) plus title sync: gbandit.jsonc is
    /// the source of truth for the title whenever it carries one.
    async fn ensure_project(
        &self,
        client: &PlatformClient,
        config: &ProjectConfig,
        create: bool,
        json: bool,
    ) -> Result<()> {
        let slug = &config.project;
        match client.get_project(slug).await? {
            Some(existing) => {
                if let Some(title) = config.title.as_deref().map(str::trim) {
                    if title != existing.title {
                        client.update_project_title(slug, title).await?;
                        self.printer
                            .progress(format!("Updated project title to \"{title}\"."));
                    }
                }
            }
            None => {
                if !create && !confirm_create(slug, json)? {
                    bail!("aborted: project '{slug}' was not created");
                }
                let title = config
                    .title
                    .as_deref()
                    .map(str::trim)
                    .map(str::to_string)
                    .unwrap_or_else(|| title_from_slug(slug));
                self.printer.progress(format!(
                    "Creating project '{slug}' (title: \"{title}\") on the platform..."
                ));
                let created = client.create_project(slug, &title).await?;
                self.printer
                    .progress(format!("Project '{}' created.", created.slug));
            }
        }
        Ok(())
    }
}

fn confirm_google_login() -> Result<bool> {
    crate::printer::confirm_yes(
        "Sign in with Google now? Your guest account and its projects are kept. [Y/n] ",
    )
}

/// Typo guard: a misspelled `project` in gbandit.jsonc must not silently
/// become a fresh project. `--create` is the non-interactive opt-in.
fn confirm_create(slug: &str, json: bool) -> Result<bool> {
    if json || !std::io::stdin().is_terminal() {
        bail!(
            "project '{slug}' does not exist on the platform (or you are not a member of it). \
             If gbandit.jsonc's \"project\" was edited, change it back; pass --create to \
             create '{slug}' on deploy"
        );
    }
    crate::printer::confirm_yes(&format!(
        "Project '{slug}' does not exist on the platform (or you are not a member of it). \
         If gbandit.jsonc's \"project\" was edited, answer no and change it back. Create it? [Y/n] "
    ))
}

/// Type-the-slug friction, mirroring `project delete`: removing a database
/// is destructive and must not happen off a reflexive "y".
pub(crate) fn confirm_database_removal_prompt(project: &str) -> Result<()> {
    crate::printer::confirm_typed(
        "Type the project name to confirm removing the database: ",
        project,
        "aborted: typed value did not match the project name",
    )
}

/// `--json` failure line for stdout: the platform's structured error payload
/// ({error, code?, issues?}) plus status, matching the success-line shape.
pub(crate) fn json_error_payload(err: &anyhow::Error) -> String {
    let mut payload = match err.downcast_ref::<ApiError>() {
        Some(api) => {
            serde_json::to_value(api).unwrap_or_else(|_| serde_json::json!({ "error": api.error }))
        }
        None => serde_json::json!({ "error": format!("{err:#}") }),
    };
    payload["status"] = serde_json::Value::String("error".to_string());
    payload.to_string()
}

/// Pushes a deployed commit to the linked remote (ADR 0005). The deploy has
/// already happened, so a rejected push is something to fix, not a failure.
fn push(printer: &Printer) -> Result<()> {
    if !git::has_origin()? {
        return Ok(());
    }
    printer.progress("Pushing to linked remote...");
    match git::push_main()? {
        git::PushOutcome::Ok => printer.progress("Push succeeded."),
        git::PushOutcome::NoRemote => {}
        git::PushOutcome::NonFastForward { detail } => printer.progress(format!(
            "Deployed, but the push was rejected (non-fast-forward) — the linked remote has commits this repo doesn't. \
             If you're in Pi, ask it to use the pull_remote skill. From a laptop, run `git pull --rebase` and push.\n\n{detail}"
        )),
        git::PushOutcome::Network { detail } => printer.progress(format!(
            "Deployed, but the push failed — network unreachable. The next successful deploy pushes this commit too.\n\n{detail}"
        )),
        git::PushOutcome::Auth { detail } => printer.progress(format!(
            "Deployed, but the push failed — authentication rejected. \
             Looks like the Deploy Key isn't installed any more (or your laptop's git credentials are wrong). \
             Reconnect from the Settings page or fix your local credentials; the next successful deploy pushes this commit too.\n\n{detail}"
        )),
    }
    Ok(())
}

/// The deploy's commit, as `prepare_commit` left it.
struct DeployCommit {
    /// The deploy's label in the history; nothing decides anything from it,
    /// so it is only sent when HEAD is what gets uploaded.
    sha: Option<String>,
    deploy_message: Option<String>,
    made: Option<git::MadeCommit>,
}

/// - auto_commit=true, dirty: commit, and send that commit.
/// - auto_commit=true, clean: send HEAD (no empty commit).
/// - auto_commit=false, dirty: no SHA, the upload is not any commit.
/// - auto_commit=false, clean: send HEAD; the deploy never pushes.
fn prepare_commit(
    printer: &Printer,
    auto_commit: bool,
    message: Option<&str>,
) -> Result<DeployCommit> {
    let deploy_message = message.map(str::to_string);
    let commit_message = message
        .map(str::to_string)
        .unwrap_or_else(|| "gbandit deploy".to_string());
    let uncommitted = |deploy_message| DeployCommit {
        sha: None,
        deploy_message,
        made: None,
    };

    if !git::in_repo()? {
        if !auto_commit {
            printer.progress("Skipping auto-commit: not a git repository.");
            return Ok(uncommitted(deploy_message));
        }
        bail!(
            "auto-commit requires a git repository and this directory is not one — \
             run `git init`, or set local_dev.auto_commit to false in gbandit.jsonc \
             to deploy without committing"
        );
    }

    let clean = match git::is_clean() {
        Ok(value) => value,
        Err(err) => {
            if !auto_commit {
                printer.progress(format!("Skipping auto-commit: {err}"));
                return Ok(uncommitted(deploy_message));
            }
            return Err(err);
        }
    };

    if !auto_commit && !clean {
        printer.progress(
            "Deploying uncommitted local changes. Linked remote will not be pushed; the Gbandit Agent will not see these changes unless you commit/push/sync them.",
        );
        return Ok(uncommitted(deploy_message));
    }

    if auto_commit && !clean {
        printer.progress("Auto-committing working tree...");
        let identity = if git::has_identity()? {
            None
        } else {
            Some(crate::auth_session::git_identity()?)
        };
        let made = git::commit_all(&commit_message, identity.as_ref())?;
        return Ok(DeployCommit {
            sha: Some(made.sha.clone()),
            deploy_message,
            made: Some(made),
        });
    }

    Ok(DeployCommit {
        sha: git::head_sha()?,
        deploy_message,
        made: None,
    })
}
