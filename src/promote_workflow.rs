use std::io::IsTerminal;

use anyhow::Result;

use crate::deploy_workflow::{confirm_database_removal_prompt, json_error_payload};
use crate::game_profile_command::{ProjectTarget, fetch_project, publish_hint};
use crate::http::{ApiError, problem_types};
use crate::pipeline_watch::watch_pipeline;
use crate::platform_client::{PlatformClient, PromotionStarted};
use crate::printer::Printer;

pub(crate) struct PromoteArgs {
    pub(crate) confirm_database_removal: bool,
    pub(crate) detach: bool,
    pub(crate) json: bool,
}

/// `gbandit promote`: point prod at the Release dev is running. It asks
/// nothing on its own, a snapshot is taken first and a promotion is
/// reversible by promoting again. The one prompt it can raise is the same one
/// a deploy has, because removing a database is not reversible.
pub(crate) async fn promote(
    printer: &Printer,
    target: &ProjectTarget,
    args: &PromoteArgs,
) -> Result<()> {
    let result = promote_inner(printer, target, args).await;
    if args.json
        && let Err(err) = &result
    {
        println!("{}", json_error_payload(err));
    }
    result
}

async fn promote_inner(
    printer: &Printer,
    target: &ProjectTarget,
    args: &PromoteArgs,
) -> Result<()> {
    let project = target.slug.as_str();
    let client = PlatformClient::from_saved_auth().await?;
    let mut result = client
        .start_promotion(project, args.confirm_database_removal)
        .await;

    if !args.confirm_database_removal
        && !args.json
        && std::io::stdin().is_terminal()
        && let Err(err) = &result
        && let Some(api) = err.downcast_ref::<ApiError>()
        && api.is(problem_types::DATABASE_REMOVAL_REQUIRES_CONFIRMATION)
    {
        printer.progress(api.message());
        confirm_database_removal_prompt(project)?;
        result = client.start_promotion(project, true).await;
    }
    let started = result?;

    if args.json {
        println!("{}", serde_json::to_string(&started)?);
    } else {
        printer.progress(describe_move(&started));
    }

    if args.detach {
        if !args.json {
            printer.progress(format!(
                "Started promotion of project {project} to prod (#{}).",
                started.pipeline_run_id
            ));
        }
        return Ok(());
    }

    if !args.json {
        printer.progress(format!("Promoting project {project} to prod..."));
    }
    watch_pipeline(
        printer,
        client.http(),
        client.origin(),
        client.token(),
        started.pipeline_run_id,
        "Promotion",
    )
    .await?;

    if let Some(hint) = publish_hint(&fetch_project(&client, project).await?, target) {
        printer.progress(hint);
    }
    Ok(())
}

/// "prod runs abc1234, promoting to def5678": what prod serves now and what
/// it moves to, which is the whole content of a promotion.
fn describe_move(started: &PromotionStarted) -> String {
    let from = match (&started.from_release_id, &started.from_commit_sha) {
        (None, _) => "nothing yet".to_string(),
        (Some(_), Some(sha)) => short_sha(sha),
        (Some(_), None) => "a build without a commit".to_string(),
    };
    let to = started
        .to_commit_sha
        .as_deref()
        .map(short_sha)
        .unwrap_or_else(|| "a build without a commit".to_string());
    format!("prod runs {from}, promoting to {to}")
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(
        from_release: Option<&str>,
        from: Option<&str>,
        to: Option<&str>,
    ) -> PromotionStarted {
        PromotionStarted {
            pipeline_run_id: 1,
            status: "pending".into(),
            release_id: "r2".into(),
            from_release_id: from_release.map(str::to_string),
            from_commit_sha: from.map(str::to_string),
            to_commit_sha: to.map(str::to_string),
            created_at: "now".into(),
        }
    }

    #[test]
    fn the_move_names_both_commits() {
        assert_eq!(
            describe_move(&started(
                Some("r1"),
                Some("0123456789abcdef"),
                Some("fedcba9876543210")
            )),
            "prod runs 0123456, promoting to fedcba9"
        );
    }

    #[test]
    fn a_first_promotion_says_prod_runs_nothing_yet() {
        assert_eq!(
            describe_move(&started(None, None, Some("fedcba9876543210"))),
            "prod runs nothing yet, promoting to fedcba9"
        );
    }

    #[test]
    fn a_dirty_tree_build_has_no_commit_to_name() {
        assert_eq!(
            describe_move(&started(Some("r1"), None, None)),
            "prod runs a build without a commit, promoting to a build without a commit"
        );
    }
}
