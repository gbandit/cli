use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};

use crate::auth_session;
use crate::cli::{Command, EnvAction, LogTarget, ProjectAction};
use crate::config::{load_project_config, resolve_project};
use crate::deploy_workflow::{DeployArgs, DeployWorkflow};
use crate::game_profile_command::{self, ProjectTarget};
use crate::platform_client::{PlatformClient, ProjectDeleteOutcome};
use crate::printer::Printer;
use crate::promote_workflow::{PromoteArgs, promote};
use crate::query_table::QueryTable;
use crate::release_installer::ReleaseInstaller;
use crate::scaffold_command;

pub(crate) async fn run(command: Command, printer: &Printer) -> Result<()> {
    match command {
        Command::Login { guest, poll } => {
            if guest {
                auth_session::login_guest(printer).await
            } else if poll {
                auth_session::login_poll(printer).await
            } else {
                auth_session::login(printer).await
            }
        }
        Command::Whoami => auth_session::whoami(printer).await,
        Command::Update { tag } => {
            ReleaseInstaller::github()
                .install(printer, tag.as_deref())
                .await
        }
        Command::Sql {
            environment,
            project,
            query,
        } => {
            let project = resolve_project(project)?;
            let client = PlatformClient::from_saved_auth().await?;
            let result = client
                .query_database(environment.as_str(), &project, &query)
                .await?;
            QueryTable::new(&result).print();
            Ok(())
        }
        Command::Deploy {
            project,
            message,
            baseline,
            create,
            confirm_database_removal,
            detach,
            json,
        } => {
            let config = load_project_config(project)?;
            let args = DeployArgs {
                message,
                baseline,
                create,
                confirm_database_removal,
                detach,
                json,
            };
            DeployWorkflow::new(printer).deploy(&config, &args).await
        }
        Command::Promote {
            project,
            confirm_database_removal,
            detach,
            json,
        } => {
            let target = ProjectTarget::resolve(project)?;
            let args = PromoteArgs {
                confirm_database_removal,
                detach,
                json,
            };
            promote(printer, &target, &args).await
        }
        Command::Logs {
            environment,
            component,
            project,
        } => {
            let project = resolve_project(project)?;
            logs(printer, environment.as_str(), component, &project).await
        }
        Command::Env { action } => match action {
            EnvAction::Set {
                pairs,
                environment,
                project,
            } => {
                let project = resolve_project(project)?;
                env_set(printer, environment.as_str(), &project, &pairs).await
            }
            EnvAction::List {
                environment,
                project,
            } => {
                let project = resolve_project(project)?;
                env_list(printer, environment.as_str(), &project).await
            }
            EnvAction::Delete {
                key,
                environment,
                project,
            } => {
                let project = resolve_project(project)?;
                env_delete(printer, environment.as_str(), &project, &key).await
            }
        },
        Command::Project { action } => match action {
            ProjectAction::Show { project, json } => {
                game_profile_command::show(printer, &ProjectTarget::resolve(project)?, json).await
            }
            ProjectAction::Cover { image, project } => {
                game_profile_command::cover(printer, &ProjectTarget::resolve(project)?, &image)
                    .await
            }
            ProjectAction::Publish { project } => {
                game_profile_command::publish(printer, &ProjectTarget::resolve(project)?).await
            }
            ProjectAction::Unpublish { project } => {
                game_profile_command::unpublish(printer, &ProjectTarget::resolve(project)?).await
            }
            ProjectAction::Delete { slug, yes } => project_delete(printer, &slug, yes).await,
        },
        Command::Scaffold {
            name,
            title,
            target,
        } => scaffold_command::run(printer, name, title, target).await,
        Command::Docs { page, full } => docs(page.as_deref(), full).await,
        Command::Logout => auth_session::logout(printer).await,
    }
}

/// Fetch-and-print, never bundled: the deploy contract is a server-side fact
/// and a stale binary must not document an old one.
async fn docs(page: Option<&str>, full: bool) -> Result<()> {
    let origin = crate::config::docs_origin();
    let url = if full {
        format!("{origin}/llms-full.txt")
    } else {
        match page {
            Some(page) => {
                let page = page.trim_start_matches('/').trim_end_matches(".md");
                let page = page.split(['#', '?']).next().unwrap_or(page);
                format!("{origin}/{page}.md")
            }
            None => format!("{origin}/llms.txt"),
        }
    };
    let response = crate::http::http_client()
        .get(&url)
        .send()
        .await
        .with_context(|| {
            format!("failed to fetch {url} — check your network; the docs live at {origin}")
        })?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("no docs page at {url} — run `gbandit docs` to list available pages");
    }
    let text = response
        .error_for_status()
        .with_context(|| format!("failed to fetch {url}"))?
        .text()
        .await
        .with_context(|| format!("failed to read response body from {url}"))?;
    print!("{text}");
    Ok(())
}

async fn logs(
    printer: &Printer,
    environment: &str,
    component: LogTarget,
    project: &str,
) -> Result<()> {
    let client = PlatformClient::from_saved_auth().await?;
    let source = match component {
        LogTarget::Backend => "backend",
        LogTarget::Frontend => "frontend",
    };
    let logs = client.logs(environment, project, source).await?;
    if logs.is_empty() {
        printer.progress(&format!("No {source} logs recorded."));
        // The platform cannot see into a browser: an empty frontend stream is
        // more often a frontend that never posts than a game with nothing to say.
        if component == LogTarget::Frontend {
            printer.progress(
                "Frontend logs only arrive if your frontend sends them. The gbandit template \
                 does this out of the box; an existing app needs to post its browser errors \
                 to the platform. See `gbandit docs frontend-logs`.",
            );
        }
        return Ok(());
    }

    // Response is newest-first; print oldest-first.
    for entry in logs.iter().rev() {
        let time = entry.timestamp.get(11..19).unwrap_or(&entry.timestamp);
        let level = entry
            .level
            .as_deref()
            .map(str::to_uppercase)
            .unwrap_or_default();
        match component {
            // Browser logs carry who hit them and where, which is the whole
            // reason to look at them.
            LogTarget::Frontend => {
                let user = entry
                    .user_name
                    .as_deref()
                    .unwrap_or_else(|| match entry.user_is_anon {
                        Some(true) => "anon",
                        _ => "-",
                    });
                let path = entry.source_url.as_deref().map(url_path).unwrap_or("/");
                println!(
                    "{time} {level:<5}  [{user} @ {path}] {msg}",
                    msg = entry.message,
                );
            }
            LogTarget::Backend => println!("{time} {msg}", msg = entry.message),
        }
    }
    Ok(())
}

/// Strip scheme + host from a URL.
fn url_path(url: &str) -> &str {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    match after_scheme.find('/') {
        Some(idx) => &after_scheme[idx..],
        None => "/",
    }
}

async fn env_set(
    printer: &Printer,
    environment: &str,
    project: &str,
    pairs: &[String],
) -> Result<()> {
    let mut vars = BTreeMap::new();
    for pair in pairs {
        let (key, value) = pair
            .split_once('=')
            .with_context(|| format!("invalid KEY=VALUE pair: {pair}"))?;
        vars.insert(key.to_string(), value.to_string());
    }

    let client = PlatformClient::from_saved_auth().await?;
    let vars = client.set_env(environment, project, vars).await?;
    for (key, value) in &vars {
        printer.progress(format!("{key}={value}"));
    }
    Ok(())
}

async fn env_list(printer: &Printer, environment: &str, project: &str) -> Result<()> {
    let client = PlatformClient::from_saved_auth().await?;
    let vars = client.list_env(environment, project).await?;
    if vars.vars.is_empty() && vars.system_vars.is_empty() {
        printer.progress("No environment variables set.");
    } else {
        for (key, value) in &vars.vars {
            printer.progress(format!("{key}={value}"));
        }
        for (key, value) in &vars.system_vars {
            printer.progress(format!("{key}={value} [system]"));
        }
    }
    Ok(())
}

async fn env_delete(printer: &Printer, environment: &str, project: &str, key: &str) -> Result<()> {
    let client = PlatformClient::from_saved_auth().await?;
    client.delete_env(environment, project, key).await?;
    printer.progress(format!("Deleted {key}"));
    Ok(())
}

async fn project_delete(printer: &Printer, slug: &str, skip_prompt: bool) -> Result<()> {
    if !skip_prompt {
        // Friction at the presentation layer (ADR 0004 §7).
        printer.progress(format!(
            "About to permanently delete project '{slug}', including its"
        ));
        printer.progress("deployments, databases, uploaded files, and Git remote connection.");
        printer.progress("This cannot be undone.");
        crate::printer::confirm_typed(
            "Type the slug to confirm: ",
            slug,
            "aborted: typed value did not match slug",
        )?;
    }

    let client = PlatformClient::from_saved_auth().await?;
    match client.delete_project(slug).await? {
        ProjectDeleteOutcome::Started => {
            // ADR 0007: deletion is async. The project shows up as `deleting`
            // in the UI / list endpoint until the background reconciler frees
            // the slug.
            printer.progress(format!(
                "Deletion started for project '{slug}'. The project name remains \
                 unavailable until deletion is complete."
            ));
            Ok(())
        }
    }
}
