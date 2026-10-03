//! The only place in the CLI that shells out to `git`.

use std::process::Command;

use anyhow::{Context, Result, bail};

/// True when the cwd is inside a git work tree.
pub fn in_repo() -> Result<bool> {
    let output = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .context("failed to run `git` — is git installed?")?;
    Ok(output.status.success())
}

/// True when the working tree has no uncommitted changes and no untracked files.
pub fn is_clean() -> Result<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .context("failed to run `git status` — is git installed and is this a git repository?")?;
    if !output.status.success() {
        bail!(
            "`git status` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output
        .stdout
        .iter()
        .all(|b| matches!(*b, b' ' | b'\t' | b'\n' | b'\r')))
}

/// Who a commit is by when git has no identity configured.
pub struct Identity {
    pub name: String,
    pub email: String,
}

/// True when git can name an author for a commit (`user.name`/`user.email`,
/// or whatever git manages to derive from the system).
pub fn has_identity() -> Result<bool> {
    let output = Command::new("git")
        .args(["var", "GIT_AUTHOR_IDENT"])
        .output()
        .context("failed to run `git var GIT_AUTHOR_IDENT`")?;
    Ok(output.status.success())
}

/// A commit made by `commit_all`, with what it takes to undo it.
pub struct MadeCommit {
    pub sha: String,
    parent: Option<String>,
}

/// Stages everything and commits it, as `identity` when given. Caller must
/// have ensured the tree is dirty.
pub fn commit_all(message: &str, identity: Option<&Identity>) -> Result<MadeCommit> {
    let parent = head_sha()?;
    run(&["add", "-A"])?;

    let mut commit = Command::new("git");
    commit.args(["commit", "-q", "-m", message]);
    if let Some(identity) = identity {
        commit
            .env("GIT_AUTHOR_NAME", &identity.name)
            .env("GIT_AUTHOR_EMAIL", &identity.email)
            .env("GIT_COMMITTER_NAME", &identity.name)
            .env("GIT_COMMITTER_EMAIL", &identity.email);
    }
    let output = commit.output().context("failed to run `git commit`")?;
    if !output.status.success() {
        bail!(
            "`git commit` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let sha = head_sha()?.context("HEAD is unborn right after `git commit`")?;
    Ok(MadeCommit { sha, parent })
}

/// Takes `commit` back off the branch and leaves its changes in the working
/// tree, unstaged. Does nothing and returns false when HEAD has moved past it
/// since, so a commit someone made in the meantime is never rewritten.
pub fn undo_commit(commit: &MadeCommit) -> Result<bool> {
    // update-ref with the expected old value is a compare-and-swap: it fails
    // instead of moving a branch someone else just moved.
    let moved = match &commit.parent {
        Some(parent) => Command::new("git")
            .args(["update-ref", "HEAD", parent, &commit.sha])
            .output(),
        None => Command::new("git")
            .args(["update-ref", "-d", "HEAD", &commit.sha])
            .output(),
    }
    .context("failed to run `git update-ref`")?;
    if !moved.status.success() {
        return Ok(false);
    }
    match &commit.parent {
        Some(_) => run(&["reset", "-q"])?,
        None => run(&["read-tree", "--empty"])?,
    }
    Ok(true)
}

fn run(args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("failed to run `git {}`", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// `None` when the repo has no commits yet.
pub fn head_sha() -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .context("failed to run `git rev-parse HEAD`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("unknown revision") || stderr.contains("ambiguous argument") {
            return Ok(None);
        }
        bail!("`git rev-parse HEAD` failed: {}", stderr.trim());
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() {
        Ok(None)
    } else {
        Ok(Some(sha))
    }
}

/// Outcome of the push that follows a successful `gbandit deploy` (ADR 0005).
/// No origin → `NoRemote`; otherwise push and categorise so the CLI can show
/// a useful next-step message.
pub enum PushOutcome {
    Ok,
    /// No `origin` configured — Project not linked.
    NoRemote,
    /// Remote moved forward; user should pull/rebase and retry.
    NonFastForward {
        detail: String,
    },
    /// Offline / DNS / firewall.
    Network {
        detail: String,
    },
    /// Deploy key removed on host, or wrong personal credentials on laptop.
    Auth {
        detail: String,
    },
}

pub fn has_origin() -> Result<bool> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .context("failed to run `git remote get-url origin`")?;
    Ok(output.status.success() && !output.stdout.is_empty())
}

/// Push HEAD to `origin/main`, categorising the outcome.
pub fn push_main() -> Result<PushOutcome> {
    if !has_origin()? {
        return Ok(PushOutcome::NoRemote);
    }
    let output = Command::new("git")
        .args(["push", "origin", "HEAD:main"])
        .output()
        .context("failed to run `git push`")?;
    if output.status.success() {
        return Ok(PushOutcome::Ok);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let stderr_lower = stderr.to_lowercase();
    if stderr_lower.contains("non-fast-forward")
        || stderr_lower.contains("rejected")
        || stderr_lower.contains("fetch first")
        || stderr_lower.contains("updates were rejected")
    {
        return Ok(PushOutcome::NonFastForward {
            detail: stderr.trim().to_string(),
        });
    }
    if stderr_lower.contains("could not resolve host")
        || stderr_lower.contains("network is unreachable")
        || stderr_lower.contains("connection refused")
        || stderr_lower.contains("connection timed out")
        || stderr_lower.contains("temporary failure in name resolution")
    {
        return Ok(PushOutcome::Network {
            detail: stderr.trim().to_string(),
        });
    }
    if stderr_lower.contains("permission denied")
        || stderr_lower.contains("could not read from remote repository")
        || stderr_lower.contains("authentication failed")
        || stderr_lower.contains("publickey")
        || stderr_lower.contains("403")
    {
        return Ok(PushOutcome::Auth {
            detail: stderr.trim().to_string(),
        });
    }
    bail!("`git push` failed: {}", stderr.trim())
}
