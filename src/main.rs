mod auth_session;
mod cli;
mod commands;
mod config;
mod deploy_archive;
mod deploy_workflow;
mod game_profile_command;
mod git;
mod http;
mod pipeline_watch;
mod platform_client;
mod printer;
mod promote_workflow;
mod query_table;
mod release_installer;
mod scaffold;
mod scaffold_command;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command};
use crate::printer::Printer;

pub(crate) const BUILD_VERSION: &str = env!("GBANDIT_BUILD_VERSION");

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let printer = Printer {
        verbose: cli.verbose,
        timestamps: cli.timestamps,
        json: matches!(
            &cli.command,
            Command::Deploy { json: true, .. } | Command::Promote { json: true, .. }
        ),
    };
    commands::run(cli.command, &printer).await
}
