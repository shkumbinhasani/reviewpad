mod app;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use reviewpad::{
    git::Repository,
    review::{Review, Side},
    update,
};
use std::{
    io::Read,
    path::{Path, PathBuf},
};

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

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SideArg {
    /// The line as it appears after the change.
    New,
    /// The line as it appeared before the change.
    Old,
}

impl From<SideArg> for Side {
    fn from(side: SideArg) -> Self {
        match side {
            SideArg::New => Side::New,
            SideArg::Old => Side::Old,
        }
    }
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
    /// Leave a comment on a changed line. Prints the new comment's id.
    Comment {
        /// File to comment on, relative to the repository root.
        file: String,
        /// Line number within that file.
        line: u32,
        /// Comment text. Read from stdin when omitted.
        #[arg(long)]
        body: Option<String>,
        /// Which side of the diff the line belongs to.
        #[arg(long, value_enum, default_value_t = SideArg::New)]
        side: SideArg,
        /// Name to sign the comment with.
        #[arg(long, default_value = reviewpad::review::DEFAULT_AUTHOR)]
        author: String,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Reply to a comment, continuing its thread. Prints the new reply's id.
    Reply {
        /// Id of the comment or reply to answer, such as `c1` or `c1.2`.
        id: String,
        /// Reply text. Read from stdin when omitted.
        #[arg(long)]
        body: Option<String>,
        /// Name to sign the reply with.
        #[arg(long, default_value = reviewpad::review::DEFAULT_AUTHOR)]
        author: String,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// List saved comments and their ids.
    List {
        /// Print the raw review file instead of a summary.
        #[arg(long)]
        json: bool,
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Delete a single comment or reply by id.
    Remove {
        /// Id of the comment or reply to delete.
        id: String,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
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
            let review = Review::open(&repository)?;
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
        Some(Command::Comment {
            file,
            line,
            body,
            side,
            author,
            repo,
        }) => comment(repo, file, line, side.into(), author, body),
        Some(Command::Reply {
            id,
            body,
            author,
            repo,
        }) => reply(repo, id, author, body),
        Some(Command::List { json, path }) => list(path, json),
        Some(Command::Remove { id, repo }) => remove(repo, id),
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

/// Anchor a comment to a changed line, quoting the surrounding diff so the
/// exported brief carries the change it refers to.
fn comment(
    repo: PathBuf,
    file: String,
    line: u32,
    side: Side,
    author: String,
    body: Option<String>,
) -> Result<()> {
    let body = read_body(body)?;
    let repository = Repository::discover(&repo)?;
    let diff = repository.diff()?;

    // A path that is not in the diff is almost always a typo, and accepting it
    // would anchor the note to nothing.
    let Some(changed) = diff.files.iter().find(|changed| changed.path == file) else {
        let changed = diff
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>();
        bail!(
            "`{file}` has no changes to review. Changed files: {}",
            if changed.is_empty() {
                "none".to_string()
            } else {
                changed.join(", ")
            }
        );
    };

    let context = changed
        .index_of(side, line)
        .map(|index| changed.context_at(index))
        .unwrap_or_default();

    let mut review = Review::open(&repository)?;
    let id = review.add_comment(&file, side, line, &author, body, context);
    review.save(&repository.review_path())?;

    eprintln!("Added {id} on {file}:{line} ({})", side.label());
    println!("{id}");
    Ok(())
}

fn reply(repo: PathBuf, id: String, author: String, body: Option<String>) -> Result<()> {
    let body = read_body(body)?;
    let repository = Repository::discover(&repo)?;
    let mut review = Review::open(&repository)?;

    let reply = review.add_reply(&id, &author, body)?;
    review.save(&repository.review_path())?;

    eprintln!("Replied to {id} as {reply}");
    println!("{reply}");
    Ok(())
}

fn remove(repo: PathBuf, id: String) -> Result<()> {
    let repository = Repository::discover(&repo)?;
    let mut review = Review::open(&repository)?;
    review.remove(&id)?;
    review.save(&repository.review_path())?;
    eprintln!("Removed {id}");
    Ok(())
}

fn list(path: PathBuf, json: bool) -> Result<()> {
    let repository = Repository::discover(&path)?;
    let review = Review::open(&repository)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&review)?);
        return Ok(());
    }

    if review.is_empty() {
        println!("No review comments.");
        return Ok(());
    }

    for comment in &review.comments {
        println!(
            "{}  {}:{} ({})  {}",
            comment.id,
            comment.path,
            comment.line,
            comment.side.label(),
            comment.author
        );
        for line in comment.body.trim().lines() {
            println!("      {line}");
        }
        for reply in &comment.replies {
            println!("  {}  {}", reply.id, reply.author);
            for line in reply.body.trim().lines() {
                println!("      {line}");
            }
        }
    }
    Ok(())
}

/// Take the note from `--body`, or from stdin when it is omitted, so an agent
/// can pipe a long comment in without quoting it into a shell argument.
fn read_body(body: Option<String>) -> Result<String> {
    let text = match body {
        Some(text) => text,
        None => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .context("failed to read the comment from stdin")?;
            text
        }
    };

    let text = text.trim().to_string();
    if text.is_empty() {
        bail!("the comment is empty — pass --body or pipe the text in on stdin");
    }
    Ok(text)
}
