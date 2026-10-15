//! `gbandit scaffold`: materialise the game template into ./<slug>/ (or the
//! current dir when the user passed "."). Purely local apart from a
//! best-effort slug-availability check — the platform project is created on
//! the first `gbandit deploy`.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::platform_client::{PlatformClient, SlugAvailability};
use crate::printer::Printer;
use crate::scaffold::{self, ScaffoldOptions};

pub(crate) async fn run(
    printer: &Printer,
    name: Option<String>,
    title: Option<String>,
    target: Option<String>,
) -> Result<()> {
    let interactive = std::io::stdin().is_terminal();
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();

    let in_place = name.as_deref() == Some(".");
    let named_slug = match name {
        Some(n) if !in_place => {
            validate_slug_input(&n).map_err(|msg| anyhow::anyhow!("invalid slug: {msg}"))?;
            Some(n)
        }
        _ => None,
    };

    if let Some(title) = &title {
        validate_title_input(title.trim())
            .map_err(|msg| anyhow::anyhow!("invalid title: {msg}"))?;
    }

    // Availability is advisory: nothing is reserved until the first deploy
    // claims the slug, and the check is skipped when we can't reach the
    // platform (offline / not logged in).
    let availability_client = PlatformClient::from_saved_auth().await.ok();
    if availability_client.is_none() {
        printer.progress(
            "Not logged in, so the slug can't be checked. The first `gbandit deploy` reserves it.",
        );
    }

    // `title` in gbandit.jsonc is opt-in for deploy-managed titles: it is
    // written only when the user provided or confirmed one. Non-interactive
    // scaffolds (the Pi Agent entrypoint) omit it so a later deploy can't
    // clobber a title set in the web UI.
    let (slug, title, existing_title) = if let Some(slug) = named_slug {
        let existing_title = check_named_slug(printer, availability_client.as_ref(), &slug).await?;
        let title = match title {
            Some(t) => Some(t.trim().to_string()),
            None if interactive => Some(prompt_with_default(
                &mut stdin,
                "Project title",
                existing_title
                    .as_deref()
                    .unwrap_or(&scaffold::title_from_slug(&slug)),
                validate_title_input,
            )?),
            None => None,
        };
        (slug, title, existing_title)
    } else {
        if !interactive {
            bail!("a slug is required when not running interactively: `gbandit scaffold <slug>`");
        }
        let title = match title {
            Some(t) => t.trim().to_string(),
            None => prompt_required(&mut stdin, "Project title", validate_title_input)?,
        };
        let slug = prompt_available_slug(
            printer,
            &mut stdin,
            availability_client.as_ref(),
            &scaffold::slugify(&title),
        )
        .await?;
        (slug, Some(title), None)
    };
    // The Pi Agent scaffolds a project that already exists, whose title the
    // web UI owns: it reaches the page, but not gbandit.jsonc.
    let page_title = title
        .clone()
        .or(existing_title)
        .unwrap_or_else(|| scaffold::title_from_slug(&slug));

    let target_path: PathBuf = match target {
        Some(t) => PathBuf::from(t),
        None if in_place => PathBuf::from("."),
        None => PathBuf::from(&slug),
    };

    printer.progress(format!(
        "Scaffolding '{slug}' into {}...",
        target_path.display()
    ));
    scaffold::scaffold_project(
        printer,
        ScaffoldOptions {
            slug: &slug,
            target: &target_path,
            init_git: true,
            title: title.as_deref(),
            page_title: &page_title,
        },
    )?;

    if target_path == PathBuf::from(".") {
        printer.progress(
            "Done. Run `gbandit deploy` to ship it. The first deploy creates the project on the platform.",
        );
    } else {
        printer.progress(format!(
            "Done. Run `cd {} && gbandit deploy` to ship it. The first deploy creates the project on the platform.",
            target_path.display()
        ));
    }
    printer.progress(
        "To work on this game with the Pi Agent later, link a git remote in the \
         project's Settings after the first deploy. Without one the agent starts \
         from a fresh template.",
    );
    Ok(())
}

/// The slug was given explicitly, so a definite "taken by someone else" is a
/// hard error instead of wasted scaffolding. An unreachable check proceeds.
/// The title of the project the slug already names, when it is one of yours.
async fn check_named_slug(
    printer: &Printer,
    client: Option<&PlatformClient>,
    slug: &str,
) -> Result<Option<String>> {
    let Some(client) = client else {
        return Ok(None);
    };
    match client.slug_availability(slug).await {
        Ok(SlugAvailability::Free) => {
            printer.progress(format!(
                "'{slug}' is available. The first deploy reserves it."
            ));
        }
        Ok(SlugAvailability::TakenByYou) => {
            printer.progress(format!(
                "You're already a member of '{slug}'; deploys go to that project."
            ));
            return Ok(client.get_project(slug).await?.map(|project| project.title));
        }
        Ok(SlugAvailability::TakenByOther) => {
            bail!("the slug '{slug}' is already taken by another user; pick another one");
        }
        Ok(SlugAvailability::Deleting) => {
            bail!("project '{slug}' is being deleted; try again in a moment or pick another slug");
        }
        Err(err) => {
            printer.progress(format!(
                "Couldn't check the slug ({err:#}). The first `gbandit deploy` reserves it."
            ));
        }
    }
    Ok(None)
}

/// Interactive slug prompt that re-prompts while the platform reports the
/// name as unavailable. Checks we can't run (offline) fall through — the
/// first deploy is the real claim.
async fn prompt_available_slug<R: BufRead>(
    printer: &Printer,
    stdin: &mut R,
    client: Option<&PlatformClient>,
    suggested: &str,
) -> Result<String> {
    let mut default = suggested.to_string();
    loop {
        let slug = prompt_with_default(
            stdin,
            "Project slug (your game will be at your-slug.gbandit.com)",
            &default,
            validate_slug_input,
        )?;
        let Some(client) = client else {
            return Ok(slug);
        };
        match client.slug_availability(&slug).await {
            Ok(SlugAvailability::Free) => return Ok(slug),
            Ok(SlugAvailability::TakenByYou) => {
                printer.progress(format!(
                    "You're already a member of '{slug}'; deploys go to that project."
                ));
                return Ok(slug);
            }
            Ok(SlugAvailability::TakenByOther) => {
                eprintln!("  '{slug}' is already taken by another user; pick another one");
            }
            Ok(SlugAvailability::Deleting) => {
                eprintln!(
                    "  '{slug}' is being deleted; try again in a moment or pick another slug"
                );
            }
            Err(err) => {
                printer.progress(format!(
                    "Couldn't check the slug ({err:#}). The first `gbandit deploy` reserves it."
                ));
                return Ok(slug);
            }
        }
        default = slug;
    }
}

fn prompt_required<R, F>(stdin: &mut R, label: &str, validate: F) -> Result<String>
where
    R: BufRead,
    F: Fn(&str) -> Result<(), String>,
{
    loop {
        print!("{label}: ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        stdin
            .read_line(&mut line)
            .context("failed to read from stdin")?;
        let trimmed = line.trim().to_string();
        match validate(&trimmed) {
            Ok(()) => return Ok(trimmed),
            Err(msg) => eprintln!("  {msg}"),
        }
    }
}

fn prompt_with_default<R, F>(
    stdin: &mut R,
    label: &str,
    default: &str,
    validate: F,
) -> Result<String>
where
    R: BufRead,
    F: Fn(&str) -> Result<(), String>,
{
    loop {
        if default.is_empty() {
            print!("{label}: ");
        } else {
            print!("{label} [{default}]: ");
        }
        std::io::stdout().flush().ok();
        let mut line = String::new();
        stdin
            .read_line(&mut line)
            .context("failed to read from stdin")?;
        let trimmed = line.trim();
        let value = if trimmed.is_empty() {
            default.to_string()
        } else {
            trimmed.to_string()
        };
        match validate(&value) {
            Ok(()) => return Ok(value),
            Err(msg) => eprintln!("  {msg}"),
        }
    }
}

fn validate_title_input(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("title cannot be empty".to_string());
    }
    if value.chars().count() > 80 {
        return Err("title must be 80 characters or fewer".to_string());
    }
    if value.chars().any(char::is_control) {
        return Err("title may not contain control characters".to_string());
    }
    Ok(())
}

fn validate_slug_input(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > scaffold::MAX_SLUG_LEN {
        return Err(format!(
            "slug must be 1–{} characters",
            scaffold::MAX_SLUG_LEN
        ));
    }
    let bytes = value.as_bytes();
    let edge_ok = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if !edge_ok(bytes[0]) || !edge_ok(bytes[bytes.len() - 1]) {
        return Err("slug must start and end with [a-z0-9]".to_string());
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
    {
        return Err("slug may only contain [a-z0-9-]".to_string());
    }
    for prefix in ["dev-", "stage-", "prod-"] {
        if value.starts_with(prefix) {
            return Err(format!(
                "slug may not start with reserved prefix '{prefix}'"
            ));
        }
    }
    Ok(())
}
