use clap::{Parser, Subcommand, ValueEnum};

/// Deploy and manage games on gbandit.
#[derive(Parser)]
#[command(name = "gbandit", version = crate::BUILD_VERSION)]
pub(crate) struct Cli {
    /// Show full build output, stage events and error diagnostics.
    #[arg(short, long, global = true, help_heading = "Global options")]
    pub(crate) verbose: bool,
    /// Prefix every line with a timestamp.
    #[arg(short, long, global = true, help_heading = "Global options")]
    pub(crate) timestamps: bool,
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Log in with Google in the browser.
    ///
    /// Without a terminal it prints the link and returns instead of waiting,
    /// and the next command picks up the login once it is approved.
    Login {
        /// Create a guest account without a browser. Move it to Google later
        /// with a plain `gbandit login`.
        #[arg(long, conflicts_with = "poll")]
        guest: bool,
        /// Check on a login that is waiting for approval. Fails while it is
        /// still waiting.
        #[arg(long)]
        poll: bool,
    },
    /// Show who you're logged in as.
    Whoami,
    /// Update the gbandit CLI to the latest release.
    Update {
        /// Install a specific release tag instead of the latest release.
        #[arg(long)]
        tag: Option<String>,
    },
    /// Run a SQL query against the game's database.
    Sql {
        /// The SQL to run.
        query: String,
        /// Environment.
        #[arg(short, long, default_value_t = Environment::Dev)]
        environment: Environment,
        /// Project slug (default: from gbandit.jsonc).
        #[arg(long)]
        project: Option<String>,
    },
    /// Build the project and roll it out to dev.
    ///
    /// Prod is never built. `gbandit promote` moves what dev runs to prod.
    Deploy {
        /// Project slug (default: from gbandit.jsonc).
        #[arg(long)]
        project: Option<String>,
        /// Label for this deploy in the history, and the commit message when
        /// auto-commit commits.
        #[arg(short, long)]
        message: Option<String>,
        /// Agent warm build: deploys the fresh scaffold to warm the build
        /// cache. Server-side it is skipped entirely if the project already
        /// has a succeeded deploy.
        #[arg(long, hide = true)]
        baseline: bool,
        /// Create the project on the platform without asking when it does
        /// not exist yet.
        #[arg(long)]
        create: bool,
        /// Confirm removing dev's database when gbandit.jsonc no longer
        /// declares it. Without it such a deploy is rejected; interactive runs
        /// are asked instead.
        #[arg(long)]
        confirm_database_removal: bool,
        /// Return after starting the deployment instead of waiting for completion.
        /// An auto-commit is then kept but not pushed; the next deploy that
        /// succeeds pushes it.
        #[arg(long)]
        detach: bool,
        /// Emit stable machine-readable JSON on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Roll out what dev runs to prod, without rebuilding.
    ///
    /// Prod serves exactly the build you tested in dev.
    Promote {
        /// Project slug (default: from gbandit.jsonc).
        #[arg(long)]
        project: Option<String>,
        /// Confirm removing prod's database when what dev runs no longer
        /// declares it. Without it such a promotion is rejected; interactive
        /// runs are asked instead.
        #[arg(long)]
        confirm_database_removal: bool,
        /// Return after starting the promotion instead of waiting for completion.
        #[arg(long)]
        detach: bool,
        /// Emit stable machine-readable JSON on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Print recent frontend or backend logs.
    Logs {
        /// Which logs to print.
        component: LogTarget,
        /// Environment.
        #[arg(short, long, default_value_t = Environment::Dev)]
        environment: Environment,
        /// Project slug (default: from gbandit.jsonc).
        #[arg(long)]
        project: Option<String>,
    },
    /// Manage environment variables.
    Env {
        #[command(subcommand)]
        action: EnvAction,
    },
    /// Manage the game's profile, catalog listing and deletion.
    Project {
        #[command(subcommand)]
        action: ProjectAction,
    },
    /// Create a new game from the template.
    ///
    /// The game goes in ./<name>/, or the current directory when NAME is ".".
    /// The project is created on the platform by your first `gbandit deploy`.
    Scaffold {
        /// Project slug, also used as the directory name. Use "." to scaffold
        /// into the current directory. Asked for if omitted.
        name: Option<String>,
        /// Display title, stored as `title` in gbandit.jsonc. Every deploy
        /// sets the platform title from it. Asked for if omitted.
        #[arg(long)]
        title: Option<String>,
        /// Scaffold into this directory instead of ./<name>/. Used by the
        /// Pi Agent entrypoint.
        #[arg(long, hide = true)]
        target: Option<String>,
    },
    /// Print the platform docs.
    ///
    /// Fetched as markdown from docs.gbandit.com, so it is always current.
    Docs {
        /// Page to print (e.g. "deploy", "auth", "heartbeat"). Omit to
        /// print the llms.txt index of available pages.
        page: Option<String>,
        /// Print the entire documentation in one output (llms-full.txt).
        #[arg(long)]
        full: bool,
    },
    /// Log out and revoke this session.
    Logout,
}

#[derive(Subcommand)]
pub(crate) enum ProjectAction {
    /// Show the game's title, cover, catalog listing and address.
    Show {
        /// Project slug (default: from gbandit.jsonc).
        #[arg(long)]
        project: Option<String>,
        /// Emit stable machine-readable JSON on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Upload the game's cover image (PNG, JPEG or WebP). It is cropped to
    /// 16:9 and shown in the game catalog.
    Cover {
        /// Path to the image file.
        image: std::path::PathBuf,
        /// Project slug (default: from gbandit.jsonc).
        #[arg(long)]
        project: Option<String>,
    },
    /// List the game in the game catalog. Needs a cover image and a game
    /// promoted to prod.
    Publish {
        /// Project slug (default: from gbandit.jsonc).
        #[arg(long)]
        project: Option<String>,
    },
    /// Take the game out of the game catalog. The game itself keeps running.
    Unpublish {
        /// Project slug (default: from gbandit.jsonc).
        #[arg(long)]
        project: Option<String>,
    },
    /// Permanently delete a project.
    Delete {
        /// Project slug to delete.
        slug: String,
        /// Skip the type-the-slug interactive confirm. Use only in scripts.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum EnvAction {
    /// Set one or more variables.
    Set {
        /// KEY=VALUE pairs to set.
        #[arg(required = true)]
        pairs: Vec<String>,
        /// Environment.
        #[arg(short, long, default_value_t = Environment::Dev)]
        environment: Environment,
        /// Project slug (default: from gbandit.jsonc).
        #[arg(long)]
        project: Option<String>,
    },
    /// List variables.
    List {
        /// Environment.
        #[arg(short, long, default_value_t = Environment::Dev)]
        environment: Environment,
        /// Project slug (default: from gbandit.jsonc).
        #[arg(long)]
        project: Option<String>,
    },
    /// Delete a variable.
    Delete {
        /// Name of the variable.
        key: String,
        /// Environment.
        #[arg(short, long, default_value_t = Environment::Dev)]
        environment: Environment,
        /// Project slug (default: from gbandit.jsonc).
        #[arg(long)]
        project: Option<String>,
    },
}

/// Mirrors `tenant_routing::Environment`; duplicated to avoid the dep.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub(crate) enum Environment {
    Dev,
    Prod,
}

impl Environment {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Environment::Dev => "dev",
            Environment::Prod => "prod",
        }
    }
}

impl std::fmt::Display for Environment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum LogTarget {
    Backend,
    Frontend,
}
