mod app;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use reviewpad::{
    git::{FileDiff, Repository},
    media::{self, Medium},
    review::{Anchor, OrderedF64, Review, Side, Spot},
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
        /// Also review these files, whether or not git shows them. Render
        /// output is usually ignored, so a video has to be named to be seen.
        #[arg(long = "include", value_name = "FILE")]
        include: Vec<String>,
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
        Some(Command::Open { path, include }) => open(path, false, include),
        Some(Command::Request { path }) => open(path, true, Vec::new()),
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
            author,
            repo,
        }) => comment(Placement {
            repo,
            file,
            line,
            time,
            spot,
            side: side.into(),
            author,
            body,
        }),
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
        None => match open(cli.path.clone(), false, Vec::new()) {
            Ok(()) => Ok(()),
            Err(_) if cli.path == Path::new(".") => app::pick_and_run(),
            Err(error) => Err(error),
        },
    }
}

fn open(path: PathBuf, print_on_finish: bool, include: Vec<String>) -> Result<()> {
    let repository = Repository::discover(&path)
        .with_context(|| format!("{} is not inside a Git working tree", path.display()))?;
    let mut diff = repository.diff()?;

    // Named files, plus anything already carrying a comment — a render
    // commented on from the CLI has to be reachable in the panel afterwards.
    let review = Review::open(&repository)?;
    let commented = review.comments.iter().map(|comment| comment.path.clone());
    for path in include.into_iter().chain(commented) {
        if diff.files.iter().any(|file| file.path == path) {
            continue;
        }
        if !repository.root.join(&path).is_file() {
            continue;
        }
        diff.files.push(FileDiff::media(path));
    }

    app::run(repository, diff, print_on_finish)
}

/// Everything the `comment` subcommand was given, bundled so the resolution
/// below reads as one decision rather than eight parameters.
struct Placement {
    repo: PathBuf,
    file: String,
    line: Option<u32>,
    time: Option<f64>,
    spot: Option<String>,
    side: Side,
    author: String,
    body: Option<String>,
}

/// Anchor a comment to whatever the file is: a diff line, a moment in a video,
/// or a place on an image.
fn comment(placement: Placement) -> Result<()> {
    let Placement {
        repo,
        file,
        line,
        time,
        spot,
        side,
        author,
        body,
    } = placement;

    let body = read_body(body)?;
    let repository = Repository::discover(&repo)?;
    let diff = repository.diff()?;

    // A path git does not show is normal for a render — `out/` is ignored in a
    // Remotion project — so a media file that exists on disk is reviewable. A
    // text file that is not in the diff is still almost always a typo.
    let changed = diff.files.iter().find(|changed| changed.path == file);
    if changed.is_none() {
        let on_disk = repository.root.join(&file).is_file();
        if !on_disk || Medium::of(&file) == Medium::Text {
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
        }
    }

    let spot = spot.map(|spot| parse_spot(&spot)).transpose()?;
    let medium = Medium::of(&file);
    let (anchor, context) = match (line, time, spot) {
        (Some(line), None, None) => {
            if medium != Medium::Text {
                bail!(
                    "`{file}` is {}, so a line number does not point at anything — \
                     use --time or --spot",
                    medium.label()
                );
            }
            let context = changed
                .and_then(|changed| changed.index_of(side, line).map(|i| (changed, i)))
                .map(|(changed, index)| changed.context_at(index))
                .unwrap_or_default();
            (Anchor::Line { side, line }, context)
        }
        (None, Some(seconds), spot) => {
            if seconds < 0. {
                bail!("--time cannot be negative");
            }
            // The frame is what a composition is written in, so carry it when
            // the file's frame rate is readable.
            let probe = media::probe(&repository.root.join(&file));
            if let Some(probe) = probe
                && seconds > probe.duration
            {
                bail!(
                    "--time {seconds} is past the end of `{file}` ({})",
                    media::timecode(probe.duration)
                );
            }
            (
                Anchor::Time {
                    seconds: OrderedF64(seconds),
                    frame: probe.map(|probe| probe.frame_at(seconds)),
                    spot,
                },
                String::new(),
            )
        }
        (None, None, Some(spot)) => (Anchor::Spot { spot }, String::new()),
        (None, None, None) => bail!(
            "say where the note goes: a line number for text, --time for video, \
             --spot for an image"
        ),
        _ => bail!("--time, --spot and a line number are alternatives, not a combination"),
    };

    let mut review = Review::open(&repository)?;
    let label = anchor.label();
    let id = review.add_comment(&file, anchor, &author, body, context);
    review.save(&repository.review_path())?;

    eprintln!("Added {id} on {file} — {label}");
    println!("{id}");
    Ok(())
}

/// `0.42,0.31` — a place on an image, normalized so it survives any display
/// size. Percentages are accepted too, since that is how the export reads.
fn parse_spot(text: &str) -> Result<Spot> {
    let (x, y) = text
        .split_once(',')
        .context("a spot is `x,y`, for example --spot 0.42,0.31")?;

    let axis = |value: &str| -> Result<f32> {
        let value = value.trim();
        let parsed: f32 = match value.strip_suffix('%') {
            Some(percent) => percent.trim().parse::<f32>().map(|value| value / 100.),
            None => value.parse::<f32>(),
        }
        .with_context(|| format!("`{value}` is not a number"))?;
        if !(0. ..=1.).contains(&parsed) {
            bail!("`{value}` is outside the image — a spot is 0..1, or a percentage");
        }
        Ok(parsed)
    };

    Ok(Spot {
        x: axis(x)?,
        y: axis(y)?,
    })
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
