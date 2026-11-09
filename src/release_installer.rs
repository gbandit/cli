use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Cursor};
use std::path::{Component as PathComponent, Path, PathBuf};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tempfile::tempdir;

use crate::config::platform_api_origin;
use crate::http::{http_client, parse_json};
use crate::printer::Printer;

/// Installs the CLI release the platform pins (ADR 0053). The archive comes
/// from GitHub, its checksum from the platform, so an archive swapped on
/// GitHub is refused instead of installed.
pub(crate) struct ReleaseInstaller {
    client: reqwest::Client,
}

/// `GET /api/cli/release`.
#[derive(Debug, Deserialize)]
struct PinnedRelease {
    version: String,
    archives: BTreeMap<String, PinnedArchive>,
}

#[derive(Debug, Deserialize)]
struct PinnedArchive {
    url: String,
    sha256: String,
}

impl ReleaseInstaller {
    pub(crate) fn new() -> Self {
        Self {
            client: http_client(),
        }
    }

    pub(crate) async fn install(&self, printer: &Printer) -> Result<()> {
        let release = self.pinned_release().await?;
        if release.version == env!("CARGO_PKG_VERSION") {
            printer.progress(format!(
                "gbandit is already up to date ({}).",
                release.version
            ));
            return Ok(());
        }

        let target = release_target()?;
        let Some(archive) = release.archives.get(&target) else {
            bail!("gbandit {} has no build for {target}", release.version);
        };
        let install_path = cli_install_path()?;

        printer.progress(format!(
            "Downloading gbandit {} for {target}...",
            release.version
        ));
        let response = self
            .client
            .get(&archive.url)
            .send()
            .await
            .with_context(|| format!("failed to download {}", archive.url))?;
        if !response.status().is_success() {
            let status = response.status();
            bail!("failed to download {}: {status}", archive.url);
        }
        let bytes = response
            .bytes()
            .await
            .context("failed to read release archive")?;
        verify_sha256(&bytes, &archive.sha256, &archive.url)?;

        let tmp = tempdir().context("failed to create temporary update directory")?;
        extract_archive(&bytes, tmp.path())?;
        let new_binary =
            find_binary(tmp.path())?.context("the release archive has no gbandit binary")?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&new_binary, fs::Permissions::from_mode(0o755))
                .context("failed to mark downloaded binary executable")?;
        }

        replace_binary(&new_binary, &install_path)?;
        printer.progress(format!(
            "Updated gbandit from {} to {}: {}",
            env!("CARGO_PKG_VERSION"),
            release.version,
            install_path.display()
        ));
        Ok(())
    }

    async fn pinned_release(&self) -> Result<PinnedRelease> {
        let url = format!("{}/cli/release", platform_api_origin());
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("failed to ask {url} for the current gbandit release"))?;
        parse_json(response)
            .await
            .context("failed to look up the current gbandit release")
    }
}

fn verify_sha256(bytes: &[u8], expected: &str, url: &str) -> Result<()> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected {
        bail!("refusing to install: {url} has sha256 {actual}, but the platform pins {expected}");
    }
    Ok(())
}

/// The archive's top-level layout is the release pipeline's choice, so the
/// binary is looked up by name rather than at a fixed path.
fn find_binary(dir: &Path) -> Result<Option<PathBuf>> {
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            if let Some(found) = find_binary(&path)? {
                return Ok(Some(found));
            }
        } else if path
            .file_name()
            .is_some_and(|name| name == binary_file_name())
        {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

fn replace_binary(new_binary: &Path, install_path: &Path) -> Result<()> {
    if let Some(parent) = install_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create install directory {}", parent.display()))?;
    }

    let staged = install_path.with_extension("new");
    let backup = install_path.with_extension("old");
    let _ = fs::remove_file(&staged);
    let _ = fs::remove_file(&backup);
    fs::copy(new_binary, &staged)
        .with_context(|| format!("failed to stage updated binary at {}", staged.display()))?;

    if install_path.exists() {
        fs::rename(install_path, &backup)
            .with_context(|| format!("failed to move current binary to {}", backup.display()))?;
    }

    if let Err(err) = fs::rename(&staged, install_path) {
        if backup.exists() {
            let _ = fs::rename(&backup, install_path);
        }
        return Err(err).with_context(|| {
            format!(
                "failed to install updated binary at {}",
                install_path.display()
            )
        });
    }

    let _ = fs::remove_file(&backup);
    Ok(())
}

fn release_target() -> Result<String> {
    let os = match std::env::consts::OS {
        "linux" => "unknown-linux-musl",
        "macos" => "apple-darwin",
        "windows" => "pc-windows-msvc",
        other => bail!("unsupported OS for self-update: {other}"),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => bail!("unsupported architecture for self-update: {other}"),
    };
    Ok(format!("{arch}-{os}"))
}

fn binary_file_name() -> &'static str {
    if cfg!(windows) {
        "gbandit.exe"
    } else {
        "gbandit"
    }
}

fn extract_archive(bytes: &[u8], dest: &Path) -> Result<()> {
    if cfg!(windows) {
        let reader = Cursor::new(bytes);
        let mut zip = zip::ZipArchive::new(reader).context("failed to open release zip")?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).context("failed to read zip entry")?;
            let Some(rel) = entry.enclosed_name() else {
                continue;
            };
            let out_path = dest.join(rel);
            if entry.is_dir() {
                fs::create_dir_all(&out_path).with_context(|| {
                    format!("failed to create directory {}", out_path.display())
                })?;
                continue;
            }
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create directory {}", parent.display()))?;
            }
            let mut out = fs::File::create(&out_path)
                .with_context(|| format!("failed to write {}", out_path.display()))?;
            io::copy(&mut entry, &mut out)
                .with_context(|| format!("failed to extract {}", out_path.display()))?;
        }
        Ok(())
    } else {
        let decoder = GzDecoder::new(Cursor::new(bytes));
        let mut tar = tar::Archive::new(decoder);
        tar.unpack(dest)
            .context("failed to unpack release archive")?;
        Ok(())
    }
}

fn cli_install_path() -> Result<PathBuf> {
    let file_name = binary_file_name();
    if let Some(dir) = std::env::var_os("GBANDIT_INSTALL_DIR") {
        return Ok(PathBuf::from(dir).join(file_name));
    }

    let current = std::env::current_exe().context("failed to locate current executable")?;
    if is_cargo_target_binary(&current) {
        let home = dirs::home_dir().context("failed to find home directory")?;
        return Ok(home.join(".local/bin").join(file_name));
    }
    Ok(current)
}

fn is_cargo_target_binary(path: &Path) -> bool {
    let mut previous_was_target = false;
    for component in path.components() {
        let PathComponent::Normal(part) = component else {
            previous_was_target = false;
            continue;
        };
        if previous_was_target && (part == "debug" || part == "release") {
            return true;
        }
        previous_was_target = part == "target";
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_archive_matching_the_pin_is_accepted() {
        let sha = format!("{:x}", Sha256::digest(b"gbandit"));
        assert!(verify_sha256(b"gbandit", &sha, "https://example.test/a").is_ok());
    }

    #[test]
    fn an_archive_differing_from_the_pin_is_refused() {
        let sha = format!("{:x}", Sha256::digest(b"gbandit"));
        let err = verify_sha256(b"swapped", &sha, "https://example.test/a").unwrap_err();
        assert!(err.to_string().contains("refusing to install"));
    }

    #[test]
    fn the_binary_is_found_inside_a_top_level_directory() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("gbandit-x86_64-unknown-linux-musl");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("README.md"), "").unwrap();
        fs::write(nested.join(binary_file_name()), "").unwrap();
        assert_eq!(
            find_binary(dir.path()).unwrap(),
            Some(nested.join(binary_file_name()))
        );
    }
}
