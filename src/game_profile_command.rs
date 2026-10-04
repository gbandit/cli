use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::platform_client::{PlatformClient, ProjectSummary};
use crate::printer::Printer;

/// The project a command acts on, and how to name it again in a command we
/// suggest: a slug that came from `--project` has to be passed again, one
/// read from gbandit.jsonc does not.
pub(crate) struct ProjectTarget {
    pub(crate) slug: String,
    pub(crate) explicit: bool,
}

impl ProjectTarget {
    pub(crate) fn resolve(cli_project: Option<String>) -> Result<Self> {
        let explicit = cli_project.is_some();
        Ok(Self {
            slug: crate::config::resolve_project(cli_project)?,
            explicit,
        })
    }

    fn command(&self, command: &str) -> String {
        if self.explicit {
            format!("`gbandit {command} --project {}`", self.slug)
        } else {
            format!("`gbandit {command}`")
        }
    }
}

pub(crate) async fn show(printer: &Printer, target: &ProjectTarget, json: bool) -> Result<()> {
    let client = PlatformClient::from_saved_auth().await?;
    let project = fetch_project(&client, &target.slug).await?;
    if json {
        println!("{}", serde_json::to_string(&project)?);
        return Ok(());
    }
    printer.progress(format!("Title:    {}", project.title));
    printer.progress(format!(
        "Cover:    {}",
        project.cover_image_url.as_deref().unwrap_or("none")
    ));
    printer.progress(format!(
        "Catalog:  {}",
        match &project.published_at {
            Some(published_at) => format!("published since {published_at}"),
            None => "not published".to_string(),
        }
    ));
    printer.progress(format!("Play:     {}", project.play_url));
    if let Some(hint) = publish_hint(&project, target) {
        printer.progress(hint);
    }
    Ok(())
}

pub(crate) async fn cover(printer: &Printer, target: &ProjectTarget, image: &Path) -> Result<()> {
    let bytes =
        std::fs::read(image).with_context(|| format!("failed to read {}", image.display()))?;
    let file_name = image
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cover".to_string());
    let client = PlatformClient::from_saved_auth().await?;
    let profile = client
        .upload_cover_image(&target.slug, file_name, bytes)
        .await?;
    printer.progress(format!(
        "Cover image of \"{}\" updated: {}",
        profile.title,
        profile
            .cover_image_url
            .as_deref()
            .expect("an uploaded cover always has a URL")
    ));
    Ok(())
}

pub(crate) async fn publish(printer: &Printer, target: &ProjectTarget) -> Result<()> {
    let client = PlatformClient::from_saved_auth().await?;
    let project = fetch_project(&client, &target.slug).await?;
    if project.cover_image_url.is_none() {
        bail!(
            "a game needs a cover image before it can be published; upload one with {}",
            target.command("project cover <image>")
        );
    }
    let profile = client.publish_game(&target.slug).await?;
    printer.progress(format!(
        "\"{}\" is published to the game catalog. Players find it there and play it at {}.",
        profile.title, project.play_url
    ));
    Ok(())
}

pub(crate) async fn unpublish(printer: &Printer, target: &ProjectTarget) -> Result<()> {
    let client = PlatformClient::from_saved_auth().await?;
    let profile = client.unpublish_game(&target.slug).await?;
    printer.progress(format!(
        "\"{}\" is no longer in the game catalog. The game itself keeps running.",
        profile.title
    ));
    Ok(())
}

/// The line `gbandit promote` and `gbandit project show` end with while the
/// game is not in the catalog: what is left to do to get it there.
pub(crate) fn publish_hint(project: &ProjectSummary, target: &ProjectTarget) -> Option<String> {
    if project.published_at.is_some() {
        return None;
    }
    Some(match project.cover_image_url {
        Some(_) => format!(
            "The game is not in the game catalog. Publish it with {}.",
            target.command("project publish")
        ),
        None => format!(
            "The game is not in the game catalog. Upload a cover image with {}, then publish it with {}.",
            target.command("project cover <image>"),
            target.command("project publish")
        ),
    })
}

pub(crate) async fn fetch_project(client: &PlatformClient, slug: &str) -> Result<ProjectSummary> {
    client
        .get_project(slug)
        .await?
        .with_context(|| format!("project '{slug}' not found"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(cover: bool, published: bool) -> ProjectSummary {
        ProjectSummary {
            slug: "space-shooter".into(),
            title: "Space Shooter".into(),
            cover_image_url: cover.then(|| "https://assets.gbandit.com/c.webp".into()),
            published_at: published.then(|| "2026-10-03T12:00:00Z".into()),
            play_url: "https://space-shooter.gbandit.com".into(),
        }
    }

    fn target(explicit: bool) -> ProjectTarget {
        ProjectTarget {
            slug: "space-shooter".into(),
            explicit,
        }
    }

    #[test]
    fn a_published_game_gets_no_hint() {
        assert_eq!(publish_hint(&project(true, true), &target(false)), None);
    }

    #[test]
    fn a_game_with_a_cover_is_told_to_publish() {
        assert_eq!(
            publish_hint(&project(true, false), &target(false)).as_deref(),
            Some("The game is not in the game catalog. Publish it with `gbandit project publish`.")
        );
    }

    #[test]
    fn a_game_without_a_cover_is_told_to_upload_one_first() {
        assert_eq!(
            publish_hint(&project(false, false), &target(true)).as_deref(),
            Some(
                "The game is not in the game catalog. Upload a cover image with \
                 `gbandit project cover <image> --project space-shooter`, then publish it with \
                 `gbandit project publish --project space-shooter`."
            )
        );
    }
}
