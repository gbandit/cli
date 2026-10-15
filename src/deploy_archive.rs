use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use tar::Builder;
use tempfile::NamedTempFile;

use crate::git;

pub(crate) fn build_project_archive() -> Result<NamedTempFile> {
    build_project_archive_in(Path::new("."))
}

fn build_project_archive_in(root: &Path) -> Result<NamedTempFile> {
    if !root.is_dir() {
        bail!("project directory not found: {}", root.display());
    }

    let temp = NamedTempFile::new().context("failed to create temporary archive")?;
    let writer = temp
        .reopen()
        .context("failed to reopen temporary archive")?;
    // zstd level 3 is the default and a strict win over gzip default (6):
    // smaller output and several times faster on the projects we bundle.
    let encoder =
        zstd::stream::Encoder::new(writer, 3).context("failed to initialise zstd encoder")?;
    let mut tar = Builder::new(encoder);

    // gbandit.jsonc goes first so the server's config peek stops at the first
    // entry instead of decompressing the whole archive to find it.
    let config = root.join("gbandit.jsonc");
    if config.is_file() {
        let mut config_file = fs::File::open(&config)?;
        tar.append_file("gbandit.jsonc", &mut config_file)?;
    }

    for relative in project_files(root)? {
        if relative == Path::new("gbandit.jsonc") {
            continue;
        }
        let path = root.join(&relative);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            // Tracked, but deleted from the working tree.
            Err(err) if err.kind() == ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", path.display()));
            }
        };
        // Symlinks and submodules aren't shipped; the server rejects links anyway.
        if metadata.is_file() {
            let mut file = fs::File::open(&path)?;
            tar.append_file(&relative, &mut file)?;
        }
    }

    let encoder = tar.into_inner().context("failed to finalize tar archive")?;
    let mut file = encoder.finish().context("failed to finish zstd archive")?;
    file.flush().context("failed to flush archive")?;

    Ok(temp)
}

/// The files a deploy ships, relative to `root`. In a git repo that is git's
/// answer, the same one the Pi Agent's Code tab and size limit read. Outside
/// one, the same ignore rules read without git.
fn project_files(root: &Path) -> Result<Vec<PathBuf>> {
    if let Some(files) = git::project_files(root)? {
        return Ok(files.into_iter().map(PathBuf::from).collect());
    }
    // `hidden(false)` keeps dotfiles like `.dockerignore`: they're project config.
    let mut files = Vec::new();
    for entry in WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .build()
    {
        let entry = entry.context("failed to walk project directory")?;
        if entry.file_type().is_some_and(|kind| kind.is_file()) {
            let relative = entry.path().strip_prefix(root).with_context(|| {
                format!(
                    "failed to strip archive root for {}",
                    entry.path().display()
                )
            })?;
            files.push(relative.to_path_buf());
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use super::build_project_archive_in;

    fn archive_entries(archive: &tempfile::NamedTempFile) -> Vec<String> {
        let file = std::fs::File::open(archive.path()).unwrap();
        let decoder = zstd::stream::Decoder::new(file).unwrap();
        let mut tar = tar::Archive::new(decoder);
        tar.entries()
            .unwrap()
            .map(|entry| {
                entry
                    .unwrap()
                    .path()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn project_archive_puts_config_first_without_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gbandit.jsonc"), "{ \"project\": \"p\" }").unwrap();
        std::fs::create_dir_all(dir.path().join("frontend")).unwrap();
        std::fs::write(dir.path().join("frontend/index.html"), "<html>").unwrap();

        let archive = build_project_archive_in(dir.path()).unwrap();
        let entries = archive_entries(&archive);
        assert_eq!(entries.first().map(String::as_str), Some("gbandit.jsonc"));
        assert_eq!(
            entries.iter().filter(|e| *e == "gbandit.jsonc").count(),
            1,
            "{entries:?}"
        );
        assert!(
            entries.contains(&"frontend/index.html".to_string()),
            "{entries:?}"
        );
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn project_archive_in_a_repo_ships_what_git_lists() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("gbandit.jsonc"), "{}").unwrap();
        std::fs::write(root.join("tracked-ignored.txt"), "kept").unwrap();
        std::fs::write(root.join("deleted.txt"), "gone").unwrap();
        git(root, &["init", "-q"]);
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join(".gitignore"), "*.txt\n").unwrap();
        std::fs::remove_file(root.join("deleted.txt")).unwrap();
        std::fs::write(root.join("untracked.txt"), "ignored").unwrap();
        std::fs::write(root.join("new.js"), "added").unwrap();
        std::fs::write(root.join(".ignore"), "new.js\n").unwrap();

        let mut entries = archive_entries(&build_project_archive_in(root).unwrap());
        assert_eq!(entries.remove(0), "gbandit.jsonc");
        entries.sort();
        assert_eq!(
            entries,
            [".gitignore", ".ignore", "new.js", "tracked-ignored.txt"]
        );
    }

    #[test]
    fn project_archive_outside_a_repo_honours_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
        std::fs::create_dir_all(root.join("node_modules/x")).unwrap();
        std::fs::write(root.join("node_modules/x/index.js"), "dep").unwrap();
        std::fs::write(root.join("main.js"), "game").unwrap();

        let mut entries = archive_entries(&build_project_archive_in(root).unwrap());
        entries.sort();
        assert_eq!(entries, [".gitignore", "main.js"]);
    }
}
