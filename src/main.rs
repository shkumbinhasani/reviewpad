mod app;

use app::Submit;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use reviewpad::{
    git::{Base, Repository},
    mcp,
    place::{self, Placement},
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
        /// Review a branch instead of uncommitted work: `--base main` shows
        /// everything this branch added since it left main. A value containing
        /// `..` is passed to git as a range.
        #[arg(long, value_name = "REV")]
        base: Option<String>,
        /// Also review these files, whether or not git shows them. Render
        /// output is usually ignored, so a video has to be named to be seen.
        #[arg(long = "include", value_name = "FILE")]
        include: Vec<String>,
    },
    /// Open the panel and print the finished Markdown review to stdout.
    Request {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Review a branch or range instead of uncommitted work.
        #[arg(long, value_name = "REV")]
        base: Option<String>,
        #[arg(long = "include", value_name = "FILE")]
        include: Vec<String>,
        /// Write each submitted round into this directory and leave the window
        /// open, instead of printing once and closing. This is how an MCP client
        /// drives a panel: it reads the round, replies into the threads it came
        /// from, and the person watches those replies arrive.
        #[arg(long, value_name = "DIR")]
        submit_to: Option<PathBuf>,
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
    /// Leave a comment on a changed file. Prints the new comment's id.
    ///
    /// Where the note lands depends on what the file is: a line in a text
    /// diff, a moment in a video, or a place on an image.
    Comment {
        /// File to comment on, relative to the repository root.
        file: String,
        /// Line number, for a text file.
        line: Option<u32>,
        /// Comment text. Read from stdin when omitted.
        #[arg(long)]
        body: Option<String>,
        /// Seconds into a video, e.g. `--time 12.5`.
        #[arg(long, value_name = "SECONDS")]
        time: Option<f64>,
        /// A place on an image or video frame as `x,y` in 0..1, e.g.
        /// `--spot 0.42,0.31`.
        #[arg(long, value_name = "X,Y")]
        spot: Option<String>,
        /// Which side of the diff the line belongs to.
        #[arg(long, value_enum, default_value_t = SideArg::New)]
        side: SideArg,
        /// The base the line number refers to. Must match what the review is
        /// being taken against, or the anchor lands on a different line.
        #[arg(long, value_name = "REV")]
        base: Option<String>,
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
    /// Run as a Model Context Protocol server on stdio.
    ///
    /// Point an MCP client at `reviewpad mcp` to give an agent the review as
    /// tools: ask a person to look at a change, then read what they said.
    Mcp {
        /// Working tree the tools act on when a call does not name one.
        #[arg(default_value = ".")]
        path: PathBuf,
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
        Some(Command::Open {
            path,
            base,
            include,
        }) => open(path, Submit::Nothing, base, include),
        Some(Command::Request {
            path,
            base,
            include,
            submit_to,
        }) => open(
            path,
            submit_to.map_or(Submit::Stdout, Submit::rounds),
            base,
            include,
        ),
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
            time,
            spot,
            side,
            base,
            author,
            repo,
        }) => comment(Placement {
            repo,
            base,
            file,
            line,
            time,
            spot,
            side: side.into(),
            author,
            body: read_body(body)?,
        }),
        Some(Command::Reply {
            id,
            body,
            author,
            repo,
        }) => reply(repo, id, author, body),
        Some(Command::List { json, path }) => list(path, json),
        Some(Command::Remove { id, repo }) => remove(repo, id),
        Some(Command::Mcp { path }) => mcp::serve(path),
        Some(Command::Update { check }) => {
            if check {
                update::check()
            } else {
                update::install()
            }
        }
        None => match open(cli.path.clone(), Submit::Nothing, None, Vec::new()) {
            Ok(()) => Ok(()),
            Err(_) if cli.path == Path::new(".") => app::pick_and_run(),
            Err(error) => Err(error),
        },
    }
}

fn open(path: PathBuf, submit: Submit, base: Option<String>, include: Vec<String>) -> Result<()> {
    let repository = Repository::discover(&path)
        .with_context(|| format!("{} is not inside a Git working tree", path.display()))?;
    let base = base.as_deref().map(Base::parse).unwrap_or_default();
    let review = Review::open(&repository)?;
    let diff = app::reviewable_diff(&repository, &base, &include, &review)?;

    app::run(repository, base, diff, include, submit)
}

/// Save a comment and report where it landed. The rules for *where* live in
/// `place`, since an MCP client asks the same question this subcommand does.
fn comment(placement: Placement) -> Result<()> {
    let file = placement.file.clone();
    let placed = place::place(placement)?;
    if let Some(warning) = placed.warning {
        eprintln!("warning: {warning}");
    }
    eprintln!("Added {} on {file} — {}", placed.id, placed.label);
    println!("{}", placed.id);
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

    // Say what the line numbers refer to before listing any.
    if let Some(base) = &review.base {
        println!("Reviewing {base}");
    }

    for comment in &review.comments {
        println!(
            "{}  {}  {}  {}",
            comment.id,
            comment.path,
            comment.anchor.label(),
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
