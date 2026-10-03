//! Materialise a fresh project workspace from the gbandit-game template.
//!
//! Backs `gbandit scaffold` for both humans (interactive) and the Pi Agent
//! entrypoint (non-interactive, via --target). Owning the
//! clone+substitute+gbandit.jsonc+initial-commit flow in one place lets the
//! agent image stop shelling out to git/sed for the same job.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::printer::Printer;

const DEFAULT_TEMPLATE_REPO: &str = "https://github.com/gbandit/game-template";

pub(crate) struct ScaffoldOptions<'a> {
    pub(crate) slug: &'a str,
    pub(crate) target: &'a Path,
    /// When true, run `git init -b main` and an initial commit. The Pi
    /// Agent pod sets this so the workspace PVC starts with a single
    /// linear-history commit owned entirely by this project.
    pub(crate) init_git: bool,
    /// Written as `title` in gbandit.jsonc when present, opting the project
    /// into deploy-managed titles. Agent scaffolds pass None so a deploy
    /// can't clobber a title set in the web UI.
    pub(crate) title: Option<&'a str>,
}

pub(crate) fn scaffold_project(printer: &Printer, opts: ScaffoldOptions<'_>) -> Result<()> {
    if !is_valid_slug(opts.slug) {
        bail!(
            "invalid slug '{}': must be 1–63 chars, start/end with [a-z0-9], contain only [a-z0-9-]",
            opts.slug
        );
    }

    fs::create_dir_all(opts.target)
        .with_context(|| format!("failed to create target dir {}", opts.target.display()))?;

    if !is_empty_dir(opts.target)? {
        bail!(
            "target directory {} is not empty — scaffold refuses to overwrite",
            opts.target.display()
        );
    }

    let repo_url = std::env::var("GBANDIT_GAME_REPO_URL")
        .unwrap_or_else(|_| DEFAULT_TEMPLATE_REPO.to_string());

    printer.progress(format!("Cloning template from {repo_url}..."));
    let tmp = tempfile::tempdir().context("failed to create temp dir for template clone")?;
    let clone_dst = tmp.path().join("clone");
    run_git(&[
        "clone",
        "--depth=1",
        &repo_url,
        clone_dst.to_str().context("clone path is not utf-8")?,
    ])?;

    // Throw away the template's shallow history so the new workspace owns
    // its own linear git history. Carrying the template's graft boundary
    // into the workspace breaks future `git push` to a user-linked remote.
    let template_git_dir = clone_dst.join(".git");
    if template_git_dir.exists() {
        fs::remove_dir_all(&template_git_dir).with_context(|| {
            format!(
                "failed to drop template .git at {}",
                template_git_dir.display()
            )
        })?;
    }

    copy_dir_contents(&clone_dst, opts.target)?;

    let slug_underscored = opts.slug.replace('-', "_");
    substitute_placeholders(opts.target, opts.slug, &slug_underscored)?;

    if let Some(title) = opts.title {
        insert_title_into_gbandit_jsonc(opts.target, title)?;
    }

    if opts.init_git {
        printer.progress("Initialising git repo with initial commit...");
        run_git_in(opts.target, &["init", "-b", "main"])?;
        run_git_in(opts.target, &["add", "-A"])?;
        // The template is ours, not the user's work. Authoring it as gbandit
        // also means scaffolding works on a machine with no git identity and
        // never asks for the user's signing key.
        run_git_in(
            opts.target,
            &[
                "-c",
                "user.name=gbandit",
                "-c",
                "user.email=noreply@gbandit.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "Initial commit from gbandit-game template",
            ],
        )?;
    }

    Ok(())
}

fn is_valid_slug(slug: &str) -> bool {
    if slug.is_empty() || slug.len() > 63 {
        return false;
    }
    let bytes = slug.as_bytes();
    let edge_ok = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if !edge_ok(bytes[0]) || !edge_ok(bytes[bytes.len() - 1]) {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// "space-shooter" → "Space Shooter". The default platform title when
/// gbandit.jsonc doesn't manage one.
pub(crate) fn title_from_slug(slug: &str) -> String {
    slug.split('-')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn slugify(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut prev_dash = true;
    for ch in title.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_lowercase() || lower.is_ascii_digit() {
            out.push(lower);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.len() > 63 {
        out.truncate(63);
        while out.ends_with('-') {
            out.pop();
        }
    }
    out
}

/// `lost+found` doesn't count. Scaffolding into the root of a freshly
/// formatted volume is the Pi Agent's normal first boot, and every ext4
/// filesystem is born with that directory — the agent's own emptiness check in
/// `apps/pi-agent/entrypoint.sh` skips it for the same reason.
fn is_empty_dir(path: &Path) -> Result<bool> {
    let mut iter =
        fs::read_dir(path).with_context(|| format!("failed to read dir {}", path.display()))?;
    Ok(!iter.any(|entry| {
        entry
            .map(|entry| entry.file_name() != "lost+found")
            .unwrap_or(true)
    }))
}

fn copy_dir_contents(src: &Path, dst: &Path) -> Result<()> {
    for entry in fs::read_dir(src)
        .with_context(|| format!("failed to read template dir {}", src.display()))?
    {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            fs::create_dir_all(&to)?;
            copy_dir_contents(&from, &to)?;
        } else if file_type.is_symlink() {
            let target = fs::read_link(&from)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &to)?;
            #[cfg(not(unix))]
            fs::copy(&from, &to)?;
            let _ = target;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn substitute_placeholders(root: &Path, dashed: &str, underscored: &str) -> Result<()> {
    let targets = collect_text_files(root)?;
    for path in targets {
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if !looks_like_text(&bytes) {
            continue;
        }
        let content = match std::str::from_utf8(&bytes) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if !content.contains("replace-with-project") && !content.contains("replace_with_project") {
            continue;
        }
        let replaced = content
            .replace("replace-with-project", dashed)
            .replace("replace_with_project", underscored);
        fs::write(&path, replaced)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(())
}

fn collect_text_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk(root, &mut out)?;
    Ok(out)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if entry.file_name() == ".git" {
                continue;
            }
            walk(&path, out)?;
        } else if file_type.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

fn looks_like_text(bytes: &[u8]) -> bool {
    // Cheap binary sniff: NUL byte in the first 8 KiB → binary.
    let head = &bytes[..bytes.len().min(8 * 1024)];
    !head.contains(&0)
}

/// The template's gbandit.jsonc is the source of truth for the scaffolded
/// config: placeholder substitution has already stamped the project slug into
/// it, and its comments (the opt-in `backend`/`volume` blocks) must
/// survive, so it is never regenerated — the chosen title is spliced in
/// after the `"project"` line instead.
fn insert_title_into_gbandit_jsonc(target: &Path, title: &str) -> Result<()> {
    let path = target.join("gbandit.jsonc");
    let content = fs::read_to_string(&path)
        .with_context(|| format!("template is missing {}", path.display()))?;

    let mut out = String::with_capacity(content.len() + title.len() + 32);
    let mut inserted = false;
    for line in content.split_inclusive('\n') {
        out.push_str(line);
        if !inserted && line.trim_start().starts_with("\"project\"") {
            if !line.ends_with('\n') {
                out.push('\n');
            }
            let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
            let title_json = serde_json::Value::String(title.to_string());
            out.push_str(&format!("{indent}\"title\": {title_json},\n"));
            inserted = true;
        }
    }
    if !inserted {
        bail!("template gbandit.jsonc has no \"project\" line to insert \"title\" after");
    }
    fs::write(&path, out).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::insert_title_into_gbandit_jsonc;

    /// Mirrors the shipped template's gbandit.jsonc shape: slug already
    /// substituted, backend/database opt-in blocks commented out.
    const TEMPLATE_JSONC: &str = r#"{
    "project": "space-shooter",
    "frontend": {
        "dockerfile": "frontend/Dockerfile", "context": "frontend"
    },
    // "backend": {
    //     "dockerfile": "backend/Dockerfile", "context": "backend"
    // },
    "local_dev": {
        "auto_commit": true
    }
}
"#;

    fn parse(text: &str) -> serde_json::Value {
        jsonc_parser::parse_to_serde_value(text, &Default::default())
            .unwrap()
            .unwrap()
    }

    #[test]
    fn title_is_inserted_and_comments_survive() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gbandit.jsonc"), TEMPLATE_JSONC).unwrap();
        insert_title_into_gbandit_jsonc(dir.path(), "Space \"Shooter\"").unwrap();
        let written = std::fs::read_to_string(dir.path().join("gbandit.jsonc")).unwrap();
        let config = parse(&written);
        assert_eq!(config["project"], "space-shooter");
        assert_eq!(config["title"], "Space \"Shooter\"");
        assert_eq!(config["frontend"]["context"], "frontend");
        assert!(config.get("backend").is_none());
        assert!(written.contains("// \"backend\""));
    }
}

fn run_git(args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .status()
        .with_context(|| format!("failed to spawn `git {}`", args.join(" ")))?;
    if !status.success() {
        bail!("`git {}` failed with status {}", args.join(" "), status);
    }
    Ok(())
}

fn run_git_in(cwd: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .status()
        .with_context(|| format!("failed to spawn `git {}`", args.join(" ")))?;
    if !status.success() {
        bail!("`git {}` failed with status {}", args.join(" "), status);
    }
    Ok(())
}
