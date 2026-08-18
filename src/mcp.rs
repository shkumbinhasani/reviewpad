//! A Model Context Protocol server, spoken over stdio.
//!
//! The CLI already lets an agent read and write a review, but every call means
//! shelling out and parsing prose. An MCP client instead gets the operations as
//! typed tools, and — the part worth having — a way to ask a person to look at
//! something and then read what they said.
//!
//! The transport is newline-delimited JSON-RPC 2.0 on stdin and stdout, which
//! is small enough to speak directly rather than take on an SDK and an async
//! runtime for it. **Nothing but protocol messages may be written to stdout**;
//! anything else corrupts the stream, so diagnostics go to stderr.

use std::{
    io::{BufRead, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::{
    git::{Base, Repository},
    place::{self, Placement},
    review::{DEFAULT_AUTHOR, Review, Side, prepare_state_dir},
};

/// Revisions this server can speak, newest first. A client asking for one of
/// these is answered in its own dialect; anything else is answered in ours and
/// left to decide whether it can live with that, which is what the spec asks
/// for.
const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// How long `request_review` waits for a person by default. A review is a human
/// act and takes as long as it takes; the cap only stops a forgotten window
/// from holding the call open forever.
const DEFAULT_TIMEOUT: u64 = 1800;

/// How often a waiting call reports that it is still waiting. Well inside every
/// client's patience, and cheap.
const HEARTBEAT: Duration = Duration::from_secs(20);

/// How old a panel's session file may be before the panel is presumed gone.
/// Comfortably more than the few seconds an open one takes to touch it, so a
/// busy machine cannot make a live panel look dead.
const STALE: Duration = Duration::from_secs(15);

/// How told to the model the server is. This is the first thing a client reads,
/// so it says what the thing is for rather than listing the tools again.
const INSTRUCTIONS: &str = "\
ReviewPad is a local review panel for a Git working tree. A review here is a \
conversation, not a one-shot handover: the panel stays open, and the person \
reads your replies in it as you write them.

The loop:

1. `request_review` — opens the panel and blocks until the person submits a \
round of notes, then returns that round as Markdown. Waiting is the tool's job, \
not yours: expect minutes, do not poll, and do not ask the user to tell you when \
they are done.
2. Work through the notes. As you settle each one, `reply` in its thread — that \
is how the person follows what you are doing, live, in the window they are \
still looking at. Say what you changed, or push back if the note is wrong.
3. `request_review` again to wait for their next round. They may be answering \
your replies, or they may have nothing further, which comes back as a round \
saying so.
4. `close_review` when the exchange is finished, if the person has not closed \
the window themselves.

ONE PANEL, ONE SESSION. The window stays on screen for the whole exchange and \
every tool acts on it. Never try to open a second — `open_review` and \
`request_review` both drive the panel already up, and asking again for a window \
you already have is the wrong instinct. When you have changed the code, the \
panel notices within a couple of seconds by itself; `refresh_review` tells it at \
once and confirms the person is looking at your work. That is the call to reach \
for after finishing a change, not a fresh review.

Notes a person is still writing are drafts and are not yours to act on; a round \
is what they have chosen to send. `list_comments` shows both, marked.

Review a branch rather than uncommitted work by passing `base`, e.g. \"main\". \
A line number only means something against a base, so pass the same one when \
writing comments.";

/// Serve until stdin closes, which is how a client says it is done.
pub fn serve(default_repo: PathBuf) -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let line = line.context("could not read from stdin")?;
        if line.trim().is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<Value>(&line) {
            Ok(message) => handle(&message, &default_repo, &mut stdout),
            Err(error) => Some(failure(Value::Null, -32700, &error.to_string())),
        };

        if let Some(response) = response {
            writeln!(stdout, "{response}").context("could not write to stdout")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

/// Answer one message, or nothing at all when it was a notification.
///
/// `out` is the same stream the answer goes to, handed down so a tool that
/// waits can say it is still waiting. Nothing else is writing to it: messages
/// are handled one at a time, in this thread.
fn handle<W: Write>(message: &Value, default_repo: &Path, out: &mut W) -> Option<Value> {
    let id = message.get("id").cloned();
    let params = message.get("params").cloned().unwrap_or_else(|| json!({}));

    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return id.map(|id| failure(id, -32600, "a message needs a method"));
    };

    // A notification is a message with no id, and takes no answer — including
    // `notifications/initialized`, which is the client saying it is ready.
    let id = id?;

    match method {
        "initialize" => Some(success(id, initialize(&params))),
        "tools/list" => Some(success(id, json!({ "tools": tools() }))),
        "tools/call" => Some(success(id, call(&params, default_repo, out))),
        // Both are handshake courtesies with nothing to report.
        "ping" | "completion/complete" => Some(success(id, json!({}))),
        _ => Some(failure(id, -32601, &format!("unknown method `{method}`"))),
    }
}

fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = asked
        .filter(|asked| PROTOCOL_VERSIONS.contains(asked))
        .unwrap_or(PROTOCOL_VERSIONS[0]);

    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "reviewpad",
            "title": "ReviewPad",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": INSTRUCTIONS,
    })
}

/// A tool that failed is a *result*, not a protocol error — the model is meant
/// to see what went wrong and try something else, which it cannot do with a
/// JSON-RPC error.
fn call<W: Write>(params: &Value, default_repo: &Path, out: &mut W) -> Value {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    // A client that wants to be told how a long call is going sends a token to
    // report against. Without one, there is nobody to tell.
    let mut progress = Progress {
        token: params.pointer("/_meta/progressToken").cloned(),
        out,
        sent: 0,
    };

    match dispatch(name, &args, default_repo, &mut progress) {
        Ok(text) => json!({
            "content": [{ "type": "text", "text": text }],
            "isError": false,
        }),
        Err(error) => json!({
            "content": [{ "type": "text", "text": format!("{error:#}") }],
            "isError": true,
        }),
    }
}

fn dispatch<W: Write>(
    name: &str,
    args: &Value,
    default_repo: &Path,
    progress: &mut Progress<W>,
) -> Result<String> {
    match name {
        "open_review" => open_review(args, default_repo, false, progress),
        "request_review" => open_review(args, default_repo, true, progress),
        "refresh_review" => refresh_review(args, default_repo),
        "close_review" => close_review(args, default_repo),
        "list_files" => list_files(args, default_repo),
        "list_comments" => list_comments(args, default_repo),
        "export_review" => export_review(args, default_repo),
        "add_comment" => add_comment(args, default_repo),
        "reply" => reply(args, default_repo),
        "remove_comment" => remove_comment(args, default_repo),
        "clear_review" => clear_review(args, default_repo),
        _ => bail!("unknown tool `{name}`"),
    }
}

// ---------------------------------------------------------------- the tools

fn tools() -> Value {
    let repo = json!({
        "type": "string",
        "description": "Path to the Git working tree. Defaults to the directory the server was started in.",
    });
    let base = json!({
        "type": "string",
        "description": "Review a branch instead of uncommitted work: \"main\" means main...HEAD, everything the branch added since it diverged. A value containing `..` is passed to git as a range.",
    });
    let include = json!({
        "type": "array",
        "items": { "type": "string" },
        "description": "Extra files to review that git does not show, such as a rendered video under an ignored `out/` directory.",
    });

    json!([
        {
            "name": "request_review",
            "title": "Request a review",
            "description": "Have a person review this change. THIS IS THE TOOL FOR ASKING FOR A REVIEW. It opens the review panel and waits for the person to send a round of notes, then returns that round as a Markdown brief to implement. Blocking is the point — the call returns the review itself, so waiting is handled for you. Expect it to take minutes; do not poll, and do not ask the user to tell you when they have finished. The panel STAYS OPEN afterwards and there is only ever one of it: `reply` in each thread as you work so they can read it live, `refresh_review` when your changes are in, then call this again to wait for their next round — it drives the same window rather than opening another. Returns whatever was saved if they close the window instead.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": repo,
                    "base": base,
                    "include": include,
                    "timeout_seconds": {
                        "type": "integer",
                        "description": "Give up waiting after this long. Defaults to 1800.",
                        "minimum": 1,
                    },
                },
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": true },
        },
        {
            "name": "open_review",
            "title": "Open the panel without waiting",
            "description": "Open the review panel and return at once, WITHOUT the review. Only ever opens one: called while a panel is up, it does nothing but tell you so — after changing code use `refresh_review`, not this. Use `request_review` instead unless you genuinely cannot block — this tool leaves you no way to know when a round has been sent, so you would have to poll `list_comments` and guess. Suitable for putting a panel up alongside other work, not for asking for a review and acting on it. A round submitted while nobody is waiting is kept, and the next `request_review` is handed it.",
            "inputSchema": {
                "type": "object",
                "properties": { "repo": repo, "base": base, "include": include },
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": true },
        },
        {
            "name": "refresh_review",
            "title": "Show the panel your latest changes",
            "description": "Tell the OPEN panel that you have changed the code, so the person sees the new diff. Call this after finishing the work a review asked for — NOT `open_review` or a fresh `request_review`, which is the same one window either way. The panel re-reads the working tree every couple of seconds on its own, so this is about saying \"I am done, look now\" and being told they can see it; it returns the files now under review. Sign it with `author` so the panel can say who changed the code.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": repo,
                    "base": base,
                    "author": { "type": "string", "description": "Who changed the code. Agents should give their own name." },
                },
            },
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "close_review",
            "title": "Close the review panel",
            "description": "Ask the open panel to close, once the exchange is finished and you have replied to everything. The person can also just close the window themselves, so this is a courtesy rather than a requirement — and never a substitute for replying. Reports any round that was submitted but never read.",
            "inputSchema": {
                "type": "object",
                "properties": { "repo": repo },
            },
            "annotations": { "openWorldHint": true },
        },
        {
            "name": "list_files",
            "title": "List files under review",
            "description": "The files a review covers, with how many lines each adds and removes. Use it to check what can be commented on before writing a comment.",
            "inputSchema": {
                "type": "object",
                "properties": { "repo": repo, "base": base },
            },
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "list_comments",
            "title": "Read the review",
            "description": "Every saved comment and reply, with its id, the file and anchor it is attached to, and its author. `submitted: false` marks a note the person is still drafting — it has not been sent to you, so do not act on it. Safe to poll while the panel is open.",
            "inputSchema": {
                "type": "object",
                "properties": { "repo": repo },
            },
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "export_review",
            "title": "Export the review as Markdown",
            "description": "The review as an implementation brief, with each comment quoted against the lines it refers to. This is what `request_review` returns.",
            "inputSchema": {
                "type": "object",
                "properties": { "repo": repo },
            },
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "add_comment",
            "title": "Write a comment",
            "description": "Attach a note to a line of a text file, a moment in a video, or a place on an image. Exactly one anchor: `line` for code, `time` for video, `spot` for an image — `time` and `spot` together mark a place on a particular frame. With no anchor the note is about the file as a whole. Returns the new comment's id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": { "type": "string", "description": "Path relative to the repository root." },
                    "body": { "type": "string", "description": "The comment." },
                    "line": { "type": "integer", "description": "Line number in a text file.", "minimum": 1 },
                    "side": {
                        "type": "string",
                        "enum": ["new", "old"],
                        "description": "Which side of the diff `line` belongs to. Defaults to new.",
                    },
                    "time": { "type": "number", "description": "Seconds into a video, e.g. 12.5.", "minimum": 0 },
                    "spot": { "type": "string", "description": "A place on an image or frame as `x,y` in 0..1, e.g. \"0.42,0.31\". Percentages are accepted." },
                    "author": { "type": "string", "description": "Who is signing it. Defaults to `reviewer`; agents should give their own name." },
                    "repo": repo,
                    "base": base,
                },
                "required": ["file", "body"],
            },
        },
        {
            "name": "reply",
            "title": "Reply in a thread",
            "description": "Answer a comment or an earlier reply, continuing its thread. This is how you report back on a review note — say what you changed, or why the note is wrong — rather than opening a new comment. A reply appears in the open panel within a second of being written, so the person reads it while you work; reply as you settle each note rather than saving them all for the end.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Id of the comment or reply to answer, such as `c1` or `c1.2`." },
                    "body": { "type": "string", "description": "The reply." },
                    "author": { "type": "string", "description": "Who is signing it. Defaults to `reviewer`." },
                    "repo": repo,
                },
                "required": ["id", "body"],
            },
        },
        {
            "name": "remove_comment",
            "title": "Delete a comment",
            "description": "Delete one comment or reply by id. Deleting a comment takes its whole thread with it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Id of the comment or reply to delete." },
                    "repo": repo,
                },
                "required": ["id"],
            },
            "annotations": { "destructiveHint": true },
        },
        {
            "name": "clear_review",
            "title": "Clear the review",
            "description": "Delete every comment for this working tree. Do this once the review has been implemented, not before.",
            "inputSchema": {
                "type": "object",
                "properties": { "repo": repo },
            },
            "annotations": { "destructiveHint": true },
        },
    ])
}

/// Somewhere to report that a long call is still going.
///
/// A review takes as long as a person takes, and a client that hears nothing
/// for long enough is entitled to assume the server has died — Claude Code cuts
/// a silent stdio call off after thirty minutes. Saying so periodically is both
/// the courtesy and the fix.
struct Progress<'a, W: Write> {
    /// What the client asked to be reported against, if it asked at all.
    token: Option<Value>,
    out: &'a mut W,
    sent: u64,
}

impl<W: Write> Progress<'_, W> {
    fn tick(&mut self, message: &str) {
        let Some(token) = self.token.clone() else {
            return;
        };
        self.sent += 1;
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": {
                "progressToken": token,
                // No total: there is no telling how long a person will take.
                "progress": self.sent,
                "message": message,
            },
        });
        // A client that will not hear it is not worth failing the review over.
        let _ = writeln!(self.out, "{notification}");
        let _ = self.out.flush();
    }
}

/// What an open panel says about itself, in `.reviewpad/session.json`.
///
/// It exists so a panel is a thing a client can *find* rather than only
/// something it started: a second `request_review` drives the window already on
/// screen instead of stacking another one over the same review.
struct Session {
    pid: u32,
    /// Where that panel writes the rounds it submits.
    rounds: Option<PathBuf>,
}

impl Session {
    /// The panel open for this tree, if there is one.
    ///
    /// A panel that exits takes its session file with it, so a file that is
    /// still here is the first sign of a live one. Two things guard against a
    /// panel that died without the chance: the pid has to still exist, and the
    /// file has to be *fresh* — an open panel touches it every few seconds. A
    /// killed panel can leave a pid that looks alive (an unreaped child is still
    /// a process), so age is what settles it.
    fn read(repo: &Repository) -> Option<Self> {
        let path = repo.session_path();
        let age = std::fs::metadata(&path)
            .and_then(|file| file.modified())
            .ok()?
            .elapsed()
            .unwrap_or_default();
        if age > STALE {
            return None;
        }

        let session: Value = serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()?;
        let pid = session.get("pid").and_then(Value::as_u64)? as u32;
        if !alive(pid) {
            return None;
        }
        Some(Self {
            pid,
            rounds: session
                .get("submit_to")
                .and_then(Value::as_str)
                .map(PathBuf::from),
        })
    }
}

/// `kill(pid, 0)` sends no signal; it only asks whether the process is still
/// there. Declared here rather than taking on a libc dependency for one call.
#[cfg(unix)]
fn alive(pid: u32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    unsafe { kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
fn alive(_pid: u32) -> bool {
    false
}

/// The panel being driven: one this call started, or one already open.
enum Panel {
    Ours(Child),
    Theirs(u32),
}

impl Panel {
    /// Whether this call is what put the panel on screen. An agent told
    /// "Opened ReviewPad" when nothing opened learns to keep asking for
    /// windows, so the two cases are never reported as one.
    fn was_opened_here(&self) -> bool {
        matches!(self, Panel::Ours(_))
    }

    fn alive(&mut self, repo: &Repository) -> bool {
        match self {
            // A child is exact: no pid to race, no file to trust. `try_wait`
            // also reaps it, so a closed panel leaves nothing behind.
            Panel::Ours(child) => matches!(child.try_wait(), Ok(None)),
            Panel::Theirs(pid) => Session::read(repo).is_some_and(|session| session.pid == *pid),
        }
    }
}

/// The panel for this tree, opening one only if none is up.
///
/// Every panel a client opens is launched the same way — `request --submit-to` —
/// so submitting always leaves a round behind and never closes the window,
/// whether or not anybody happened to be waiting at that moment.
fn panel(args: &Value, repo: &Repository) -> Result<(Panel, PathBuf)> {
    if let Some(session) = Session::read(repo) {
        let rounds = session.rounds.unwrap_or_else(|| repo.rounds_dir());
        return Ok((Panel::Theirs(session.pid), rounds));
    }

    let rounds = repo.rounds_dir();
    let mut command = Command::new(binary()?);
    command
        .arg("request")
        .arg(&repo.root)
        .arg("--submit-to")
        .arg(&rounds);
    if let Some(base) = args.get("base").and_then(Value::as_str) {
        command.arg("--base").arg(base);
    }
    for file in strings(args, "include") {
        command.arg("--include").arg(file);
    }

    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .context("could not open the review panel")?;

    // A panel takes a moment to come up and announce itself. Waiting for that
    // here is what stops a second call arriving mid-boot from opening a second
    // window over the same review. A panel that never announces itself is still
    // returned: the caller finds out from its exit, not from this.
    let ready = Instant::now() + Duration::from_secs(5);
    while Instant::now() < ready && Session::read(repo).is_none() {
        std::thread::sleep(Duration::from_millis(100));
    }

    Ok((Panel::Ours(child), rounds))
}

/// Open the panel, and either wait for a round or leave the person to it.
fn open_review<W: Write>(
    args: &Value,
    default_repo: &Path,
    wait: bool,
    progress: &mut Progress<W>,
) -> Result<String> {
    let repo = repository(args, default_repo)?;
    let described = args
        .get("base")
        .and_then(Value::as_str)
        .map(|base| Base::parse(base).label());
    let described = described.as_deref().unwrap_or("the working tree");

    let (mut panel, rounds) = panel(args, &repo)?;

    if !wait {
        let opened = if panel.was_opened_here() {
            format!("Opened ReviewPad on {}", repo.root.display())
        } else {
            format!(
                "A panel is ALREADY OPEN on {} — this did not open a second one, and \
                 nothing should. It follows your edits by itself",
                repo.root.display()
            )
        };
        return Ok(format!(
            "{opened}, for {described}. Nothing will tell you when a round is submitted \
             — poll `list_comments`, which sees comments as they are made. To be handed \
             each round as it is sent instead, call `request_review`.",
        ));
    }

    let seconds = args
        .get("timeout_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT);
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let started = Instant::now();
    let mut announced = Instant::now();

    loop {
        if let Some(round) = consume(&rounds)? {
            return Ok(format!(
                "{round}\nThe panel is still open. Reply in each thread with `reply` as you \
                 work — the person is watching those replies arrive — then call \
                 `request_review` again to wait for their next round, or `close_review` \
                 when the exchange is done."
            ));
        }

        if !panel.alive(&repo) {
            // It may have submitted and closed in the same breath.
            if let Some(round) = consume(&rounds)? {
                return Ok(round);
            }
            let saved = Review::open(&repo)?;
            if saved.is_empty() {
                return Ok(
                    "The review panel was closed without any comments. Nothing to implement."
                        .to_string(),
                );
            }
            return Ok(format!(
                "The review panel was closed without submitting a round. Saved comments:\n\n{}",
                saved.markdown(&repo.root)
            ));
        }

        if Instant::now() >= deadline {
            // Still open after the cap. The window is left alone — killing it
            // would throw away a review someone is in the middle of writing.
            return Ok(format!(
                "No round submitted in {seconds}s. The panel has been left open, and comments \
                 are saved as they are made — call `request_review` again to keep waiting, or \
                 poll `list_comments`.\n\n{}",
                list_comments(args, default_repo)?
            ));
        }

        if announced.elapsed() >= HEARTBEAT {
            announced = Instant::now();
            progress.tick(&format!(
                "Waiting for the review — {}s so far.",
                started.elapsed().as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Take every round waiting to be read, oldest first, and delete them.
///
/// Deleting is what makes the *next* submission a new round rather than this one
/// again. More than one can be waiting: the person is free to send a second
/// round before this call came back for the first.
fn consume(directory: &Path) -> Result<Option<String>> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Ok(None);
    };

    let mut rounds: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        // A round being written is a dot-file with a `.part` name until the
        // rename, so only finished ones are ever picked up.
        .filter(|path| path.extension().is_some_and(|kind| kind == "md"))
        .collect();
    rounds.sort();

    let mut text = String::new();
    for round in &rounds {
        let Ok(body) = std::fs::read_to_string(round) else {
            continue;
        };
        if !text.is_empty() {
            text.push_str("\n---\n\n");
        }
        text.push_str(&body);
        let _ = std::fs::remove_file(round);
    }

    Ok((!text.trim().is_empty()).then_some(text))
}

/// Tell the open panel that the code has changed, and wait for it to catch up.
///
/// The panel re-reads the working tree by itself every couple of seconds, so
/// this is not what makes a change visible — it is what lets an agent say *I
/// have done the work, look now* and be told the person is seeing it. Which is
/// also the answer to the instinct to reopen the window.
fn refresh_review(args: &Value, default_repo: &Path) -> Result<String> {
    let repo = repository(args, default_repo)?;
    let Some(_) = Session::read(&repo) else {
        return Ok(
            "No review panel is open for this working tree, so there is nothing to \
             refresh. Call `request_review` to open one and be handed a review."
                .to_string(),
        );
    };

    let request = repo.refresh_path();
    if let Some(parent) = request.parent() {
        prepare_state_dir(parent)?;
    }
    // Signed, so the panel can say who changed the code rather than leaving the
    // diff to move on its own.
    let who = text(args, "author").unwrap_or_else(|| DEFAULT_AUTHOR.to_string());
    std::fs::write(&request, format!("{who}\n"))
        .with_context(|| format!("could not write {}", request.display()))?;

    // The panel deletes the request as it takes it, which is the only honest
    // signal that what is on screen is now the current diff.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !request.exists() {
            return Ok(format!(
                "The panel is showing your latest changes.\n\n{}",
                list_files(args, default_repo)?
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    Ok(
        "Asked the panel to re-read the working tree; it has not answered yet. It \
        re-reads every couple of seconds regardless, so the person will see the \
        change without another call."
            .to_string(),
    )
}

/// Ask the panel to close, and report anything it had not handed over yet.
fn close_review(args: &Value, default_repo: &Path) -> Result<String> {
    let repo = repository(args, default_repo)?;
    let Some(session) = Session::read(&repo) else {
        return Ok("No review panel is open for this working tree.".to_string());
    };

    let waiting = consume(&session.rounds.unwrap_or_else(|| repo.rounds_dir()))?;
    let request = repo.close_path();
    if let Some(parent) = request.parent() {
        prepare_state_dir(parent)?;
    }
    std::fs::write(&request, "close\n")
        .with_context(|| format!("could not write {}", request.display()))?;

    // Asked, not killed: the panel saves and exits on its own, which a signal
    // would not give it the chance to do. It watches four times a second, and
    // takes its session file with it on the way out.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if Session::read(&repo).is_none() {
            return Ok(match waiting {
                Some(round) => format!(
                    "Closed the review panel. It had submitted a round nobody had read \
                     yet:\n\n{round}"
                ),
                None => "Closed the review panel.".to_string(),
            });
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    Ok("Asked the review panel to close; it has not gone yet. It may be mid-save.".to_string())
}

fn list_files(args: &Value, default_repo: &Path) -> Result<String> {
    let repo = repository(args, default_repo)?;
    let base = args
        .get("base")
        .and_then(Value::as_str)
        .map(Base::parse)
        .unwrap_or_default();
    let diff = repo.diff_from(&base)?;

    let files: Vec<Value> = diff
        .files
        .iter()
        .map(|file| {
            json!({
                "path": file.path,
                "additions": file.additions,
                "deletions": file.deletions,
                // A render carries no patch: it is looked at, not read.
                "media": file.is_media(),
            })
        })
        .collect();

    Ok(serde_json::to_string_pretty(&json!({
        "base": base.label(),
        "files": files,
    }))?)
}

fn list_comments(args: &Value, default_repo: &Path) -> Result<String> {
    let repo = repository(args, default_repo)?;
    let review = Review::open(&repo)?;
    Ok(serde_json::to_string_pretty(&review)?)
}

fn export_review(args: &Value, default_repo: &Path) -> Result<String> {
    let repo = repository(args, default_repo)?;
    let review = Review::open(&repo)?;
    if review.is_empty() {
        return Ok("No review comments.".to_string());
    }
    Ok(review.markdown(&repo.root))
}

fn add_comment(args: &Value, default_repo: &Path) -> Result<String> {
    let side = match args.get("side").and_then(Value::as_str) {
        Some("old") => Side::Old,
        Some("new") | None => Side::New,
        Some(other) => bail!("`{other}` is not a side — use \"new\" or \"old\""),
    };

    let placed = place::place(Placement {
        repo: path(args, default_repo),
        base: text(args, "base"),
        file: required(args, "file")?,
        line: args
            .get("line")
            .and_then(Value::as_u64)
            .map(|line| line as u32),
        time: args.get("time").and_then(Value::as_f64),
        spot: text(args, "spot"),
        side,
        author: text(args, "author").unwrap_or_else(|| DEFAULT_AUTHOR.to_string()),
        body: required(args, "body")?,
    })?;

    let mut report = format!("Added {} — {}", placed.id, placed.label);
    if let Some(warning) = placed.warning {
        report.push_str(&format!("\n\nWarning: {warning}"));
    }
    Ok(report)
}

fn reply(args: &Value, default_repo: &Path) -> Result<String> {
    let repo = repository(args, default_repo)?;
    let id = required(args, "id")?;
    let mut review = Review::open(&repo)?;
    let reply = review.add_reply(
        &id,
        text(args, "author").unwrap_or_else(|| DEFAULT_AUTHOR.to_string()),
        required(args, "body")?,
    )?;
    review.save(&repo.review_path())?;
    Ok(format!("Replied to {id} as {reply}"))
}

fn remove_comment(args: &Value, default_repo: &Path) -> Result<String> {
    let repo = repository(args, default_repo)?;
    let id = required(args, "id")?;
    let mut review = Review::open(&repo)?;
    review.remove(&id)?;
    review.save(&repo.review_path())?;
    Ok(format!("Removed {id}"))
}

fn clear_review(args: &Value, default_repo: &Path) -> Result<String> {
    let repo = repository(args, default_repo)?;
    Review::default().save(&repo.review_path())?;
    Ok(format!("Cleared the review for {}", repo.root.display()))
}

// -------------------------------------------------------------- plumbing

/// The ReviewPad binary to launch the panel with — this one, so a client
/// pointed at a particular build stays with it.
fn binary() -> Result<PathBuf> {
    std::env::current_exe().context("could not find the reviewpad binary")
}

fn path(args: &Value, default_repo: &Path) -> PathBuf {
    text(args, "repo").map_or_else(|| default_repo.to_path_buf(), PathBuf::from)
}

fn repository(args: &Value, default_repo: &Path) -> Result<Repository> {
    let path = path(args, default_repo);
    Repository::discover(&path)
        .with_context(|| format!("{} is not inside a Git working tree", path.display()))
}

fn text(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn required(args: &Value, key: &str) -> Result<String> {
    text(args, key).with_context(|| format!("`{key}` is required"))
}

fn strings(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn success(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn failure(id: Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nowhere for a message to go, for the calls that never send one.
    fn discard() -> Vec<u8> {
        Vec::new()
    }

    #[test]
    fn a_notification_is_not_answered() {
        let initialized = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(handle(&initialized, Path::new("."), &mut discard()).is_none());
    }

    #[test]
    fn initialize_answers_in_the_clients_dialect_when_it_can() {
        let ask = |version: &str| {
            let message = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "protocolVersion": version },
            });
            handle(&message, Path::new("."), &mut discard())
                .unwrap()
                .pointer("/result/protocolVersion")
                .and_then(Value::as_str)
                .unwrap()
                .to_string()
        };

        assert_eq!(ask("2024-11-05"), "2024-11-05");
        // An unknown revision gets ours, and the client decides.
        assert_eq!(ask("1999-01-01"), PROTOCOL_VERSIONS[0]);
    }

    #[test]
    fn every_tool_is_dispatchable_and_describes_itself() {
        let tools = tools();
        let tools = tools.as_array().unwrap();
        assert!(!tools.is_empty());

        for tool in tools {
            let name = tool["name"].as_str().unwrap();
            assert!(
                !tool["description"].as_str().unwrap().is_empty(),
                "{name} has no description"
            );
            assert_eq!(tool["inputSchema"]["type"], "object", "{name}");
            // Unknown tools are the error case; a listed one must not be.
            let mut sink = discard();
            let mut progress = Progress {
                token: None,
                out: &mut sink,
                sent: 0,
            };
            let error = dispatch(name, &json!({}), Path::new("/"), &mut progress)
                .err()
                .map(|error| error.to_string())
                .unwrap_or_default();
            assert!(
                !error.contains("unknown tool"),
                "{name} is not dispatchable"
            );
        }
    }

    #[test]
    fn waiting_is_reported_only_when_the_client_asked_to_hear_it() {
        let mut heard = discard();
        let mut progress = Progress {
            token: Some(json!("abc")),
            out: &mut heard,
            sent: 0,
        };
        progress.tick("still going");
        progress.tick("still going");

        let notifications: Vec<Value> = String::from_utf8(heard)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(notifications.len(), 2);
        assert_eq!(notifications[0]["method"], "notifications/progress");
        assert_eq!(notifications[0]["params"]["progressToken"], "abc");
        // A progress value has to climb, or a client cannot tell it apart from
        // a repeat.
        assert_eq!(notifications[0]["params"]["progress"], 1);
        assert_eq!(notifications[1]["params"]["progress"], 2);
        // Notifications carry no id: nothing is expected back.
        assert!(notifications[0].get("id").is_none());

        let mut silence = discard();
        Progress {
            token: None,
            out: &mut silence,
            sent: 0,
        }
        .tick("nobody asked");
        assert!(silence.is_empty(), "reported progress to nobody");
    }

    /// Asking for a review means `request_review`. The first cut of these
    /// descriptions sold `open_review` as the way to "ask a person to look at
    /// the change", and models took it at its word: the panel opened, nothing
    /// waited, and the user had to say when they were done by hand.
    #[test]
    fn the_tool_that_waits_is_the_one_offered_first() {
        let tools = tools();
        let tools = tools.as_array().unwrap();
        let position = |name: &str| {
            tools
                .iter()
                .position(|tool| tool["name"] == name)
                .unwrap_or_else(|| panic!("{name} is missing"))
        };
        assert!(
            position("request_review") < position("open_review"),
            "the blocking tool should be listed first"
        );

        let description = |name: &str| {
            tools[position(name)]["description"]
                .as_str()
                .unwrap()
                .to_string()
        };
        // The non-blocking one has to point back at the blocking one...
        assert!(
            description("open_review").contains("request_review"),
            "open_review does not point at request_review"
        );
        // ...and the blocking one must not send anybody away.
        assert!(
            !description("request_review").contains("open_review"),
            "request_review steers away from itself"
        );
    }

    #[test]
    fn an_unknown_method_is_a_protocol_error() {
        let message = json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/frobnicate" });
        let response = handle(&message, Path::new("."), &mut discard()).unwrap();
        assert_eq!(response["error"]["code"], -32601);
        assert_eq!(response["id"], 7);
    }

    #[test]
    fn a_failing_tool_is_a_result_the_model_can_read() {
        let params = json!({ "name": "list_comments", "arguments": { "repo": "/nowhere" } });
        let result = call(&params, Path::new("."), &mut discard());
        assert_eq!(result["isError"], true);
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("/nowhere")
        );
    }
}
