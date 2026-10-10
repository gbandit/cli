use std::fs;
use std::io::{self, Cursor};
use std::path::{Component as PathComponent, Path, PathBuf};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use serde::Deserialize;
use tempfile::tempdir;

use crate::http::http_client;
use crate::printer::Printer;

const CLI_RELEASE_REPO: &str = "gbandit/cli";

pub(crate) struct ReleaseInstaller {
    client: reqwest::Client,
}

impl ReleaseInstaller {
    pub(crate) fn github() -> Self {
        Self {
            client: http_client(),
        }
    }

    pub(crate) async fn install(
        &self,
        printer: &Printer,
        requested_tag: Option<&str>,
    ) -> Result<()> {
        let tag = match requested_tag {
            Some(tag) => tag.to_string(),
            None => self.latest_release_tag().await?,
        };

        if requested_tag.is_none() && tag == crate::BUILD_VERSION {
            printer.progress(format!(
                "gbandit is already up to date ({}).",
                crate::BUILD_VERSION
            ));
            return Ok(());
        }

        let target = release_target()?;
        let asset = format!("gbandit-{target}.{}", archive_extension());
        let url = format!("https://github.com/{CLI_RELEASE_REPO}/releases/download/{tag}/{asset}");
        let install_path = cli_install_path()?;

        printer.progress(format!("Downloading gbandit {tag} for {target}..."));
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("failed to download {url}"))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            bail!(
                "release {tag} does not provide {asset} (or the tag does not exist); \
                 see the releases at https://github.com/{CLI_RELEASE_REPO}/releases"
            );
        }
        if !response.status().is_success() {
            let status = response.status();
            bail!("failed to download {url}: {status}");
        }
        let archive = response
            .bytes()
            .await
            .context("failed to read release archive")?;

        let tmp = tempdir().context("failed to create temporary update directory")?;
        extract_archive(&archive, tmp.path())?;

        let new_binary = tmp.path().join(binary_file_name());
        if !new_binary.is_file() {
            bail!("release archive did not contain a gbandit binary");
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&new_binary, fs::Permissions::from_mode(0o755))
                .context("failed to mark downloaded binary executable")?;
        }

        replace_binary(&new_binary, &install_path)?;
        printer.progress(format!(
            "Updated gbandit from {} to {tag}: {}",
            crate::BUILD_VERSION,
            install_path.display()
        ));
        Ok(())
    }

    async fn latest_release_tag(&self) -> Result<String> {
        let url = format!("https://api.github.com/repos/{CLI_RELEASE_REPO}/releases/latest");
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .context("failed to check latest gbandit release")?;
        if !response.status().is_success() {
            let status = response.status();
            bail!("failed to check latest gbandit release: {status}");
        }
        let release: GitHubLatestRelease = response
            .json()
            .await
            .context("failed to decode latest release response")?;
        Ok(release.tag_name)
    }
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

fn archive_extension() -> &'static str {
    if cfg!(windows) { "zip" } else { "tar.gz" }
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

#[derive(Debug, Deserialize)]
struct GitHubLatestRelease {
    tag_name: String,
}
