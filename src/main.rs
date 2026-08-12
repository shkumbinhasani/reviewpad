mod app;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use reviewpad::{git::Repository, review::Review, update};
use std::path::{Path, PathBuf};

#[derive(Debug, Parser)]
#[command(
    name = "reviewpad",
    version,
    about = "Review local Git diffs in a GPUI desktop app"
)]
struct Cli {
    /// Git working tree to review when no subcommand is provided.
    #[arg(value_name = "PATH", default_value = ".")]
    path: PathBuf,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Open the review panel as a regular desktop app.
    Open {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Open the panel and print the finished Markdown review to stdout.
    Request {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Print saved review comments as Markdown without opening a window.
    Export {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Delete all saved comments for a working tree.
    Clear {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Open a native directory picker, then review the selected repository.
    Pick,
    /// Download and install the newest release.
    Update {
        /// Report whether an update exists without installing it.
        #[arg(long)]
        check: bool,
    },
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Open { path }) => open(path, false),
        Some(Command::Request { path }) => open(path, true),
        Some(Command::Export { path }) => {
            let repository = Repository::discover(&path)?;
            let review = Review::load(&repository.review_path())?;
            print!("{}", review.markdown(&repository.root));
            Ok(())
        }
        Some(Command::Clear { path }) => {
            let repository = Repository::discover(&path)?;
            Review::default().save(&repository.review_path())?;
            println!("Cleared comments for {}", repository.root.display());
            Ok(())
        }
        Some(Command::Pick) => app::pick_and_run(),
        Some(Command::Update { check }) => {
            if check {
                update::check()
            } else {
                update::install()
            }
        }
        None => match open(cli.path.clone(), false) {
            Ok(()) => Ok(()),
            Err(_) if cli.path == Path::new(".") => app::pick_and_run(),
            Err(error) => Err(error),
        },
    }
}

fn open(path: PathBuf, print_on_finish: bool) -> Result<()> {
    let repository = Repository::discover(&path)
        .with_context(|| format!("{} is not inside a Git working tree", path.display()))?;
    let diff = repository.diff()?;
    app::run(repository, diff, print_on_finish)
}
