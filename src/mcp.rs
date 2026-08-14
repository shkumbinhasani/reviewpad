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
    review::{DEFAULT_AUTHOR, Review, Side},
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

/// How told to the model the server is. This is the first thing a client reads,
/// so it says what the thing is for rather than listing the tools again.
const INSTRUCTIONS: &str = "\
ReviewPad is a local review panel for a Git working tree.

Ask for a human review with `open_review` (returns at once) or `request_review` \
(waits until the person clicks Finish, then returns their review as Markdown). \
Comments are saved as they are written, so `list_comments` and `export_review` \
can be polled while the window is open.

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
            "name": "open_review",
            "title": "Open the review panel",
            "description": "Open ReviewPad on this working tree and return immediately. Use this to ask a person to look at the change. Comments are saved as they are written, so poll `list_comments` to see what they have said so far.",
            "inputSchema": {
                "type": "object",
                "properties": { "repo": repo, "base": base, "include": include },
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": true },
        },
        {
            "name": "request_review",
            "title": "Request a review and wait",
            "description": "Open ReviewPad and wait until the person clicks Finish review, then return their review as Markdown. This blocks for as long as the review takes — often minutes — so the client must allow a long timeout; prefer `open_review` plus polling if it cannot. Returns whatever was saved if the window is closed without finishing.",
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
            "description": "Every saved comment and reply, with its id, the file and anchor it is attached to, and its author. Safe to poll while the panel is open.",
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
            "description": "Answer a comment or an earlier reply, continuing its thread. Use this to respond to review feedback rather than opening a new comment.",
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

/// Open the panel, and either wait for the person or leave them to it.
fn open_review<W: Write>(
    args: &Value,
    default_repo: &Path,
    wait: bool,
    progress: &mut Progress<W>,
) -> Result<String> {
    let repo = repository(args, default_repo)?;
    let base = args.get("base").and_then(Value::as_str);

    let mut command = Command::new(binary()?);
    command
        .arg(if wait { "request" } else { "open" })
        .arg(&repo.root);
    if let Some(base) = base {
        command.arg("--base").arg(base);
    }
    for file in strings(args, "include") {
        command.arg("--include").arg(file);
    }

    let described = base.map(|base| Base::parse(base).label());
    let described = described.as_deref().unwrap_or("the working tree");

    if !wait {
        command.stdin(Stdio::null());
        command.stdout(Stdio::null());
        command.spawn().context("could not open the review panel")?;
        return Ok(format!(
            "Opened ReviewPad on {} for {described}. \
             Poll `list_comments` to read what the reviewer writes; \
             comments are saved as they are made.",
            repo.root.display()
        ));
    }

    // The panel prints the finished review, and that has to land somewhere it
    // can be read *after* a timeout. A pipe cannot: dropping this end of it
    // leaves the app writing into a closed one when the person finally clicks
    // Finish, half an hour later. A file is always there to be written to.
    let transcript = scratch_path();
    let sink = std::fs::File::create(&transcript)
        .with_context(|| format!("could not open {}", transcript.display()))?;

    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(sink))
        .spawn()
        .context("could not open the review panel")?;

    let seconds = args
        .get("timeout_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT);

    if !waited_out(&mut child, Duration::from_secs(seconds), progress)? {
        // Still open after the cap. The window is left alone — killing it would
        // throw away a review someone is in the middle of writing.
        return Ok(format!(
            "The review panel is still open after {seconds}s. It has been left running, \
             and comments are saved as they are made — poll `list_comments`, or call \
             `request_review` again to keep waiting.\n\n{}",
            list_comments(args, default_repo)?
        ));
    }

    let review = std::fs::read_to_string(&transcript).unwrap_or_default();
    let _ = std::fs::remove_file(&transcript);

    // Finishing prints the review; closing the window prints nothing, in which
    // case whatever was saved is still worth returning.
    if review.trim().is_empty() {
        let saved = Review::open(&repo)?;
        if saved.is_empty() {
            return Ok(
                "The review panel was closed without any comments. Nothing to implement."
                    .to_string(),
            );
        }
        return Ok(format!(
            "The review panel was closed without clicking Finish review. Saved comments:\n\n{}",
            saved.markdown(&repo.root)
        ));
    }
    Ok(review)
}

/// Whether the panel closed within the cap, saying so as it waits.
fn waited_out<W: Write>(
    child: &mut Child,
    timeout: Duration,
    progress: &mut Progress<W>,
) -> Result<bool> {
    let started = Instant::now();
    let deadline = started + timeout;
    let mut announced = Instant::now();

    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Ok(true);
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
    Ok(false)
}

/// A private file for one panel's output. There is no randomness to hand, so
/// the process and the clock name it — two reviews opened in the same
/// nanosecond by the same server would be the collision, which is not a thing.
fn scratch_path() -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!("reviewpad-{}-{stamp}.md", std::process::id()))
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
