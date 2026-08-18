//! The MCP server, driven the way a client drives it: JSON-RPC messages in on
//! stdin, one JSON message per line back on stdout.
//!
//! The tools that open a window are left alone here — those need a person.

use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

use serde_json::{Value, json};

/// A running server, and the two pipes that talk to it.
struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    id: u64,
}

impl Server {
    fn start(repo: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_reviewpad"))
            .arg("mcp")
            .arg(repo)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("could not start the server");

        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
            id: 0,
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let message = json!({
            "jsonrpc": "2.0",
            "id": self.id,
            "method": method,
            "params": params,
        });
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();

        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let response: Value = serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("`{}` answered with `{line}`: {error}", method));

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], self.id, "answered the wrong request");
        response
    }

    fn notify(&mut self, method: &str) {
        let message = json!({ "jsonrpc": "2.0", "method": method });
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Call a tool and return its text, asserting it did not fail.
    fn call(&mut self, name: &str, arguments: Value) -> String {
        let response = self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        );
        let result = &response["result"];
        assert_eq!(result["isError"], false, "{name} failed: {result}");
        result["content"][0]["text"].as_str().unwrap().to_string()
    }

    fn call_expecting_failure(&mut self, name: &str, arguments: Value) -> String {
        let response = self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        );
        let result = &response["result"];
        assert_eq!(
            result["isError"], true,
            "{name} was meant to fail: {result}"
        );
        result["content"][0]["text"].as_str().unwrap().to_string()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn a_client_can_review_a_working_tree_over_stdio() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "reviewpad@example.com"]);
    git(root, &["config", "user.name", "ReviewPad Test"]);
    std::fs::write(root.join("src.rs"), "fn main() {}\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "initial"]);
    std::fs::write(root.join("src.rs"), "fn main() {\n    work();\n}\n").unwrap();

    let mut server = Server::start(root);

    // The handshake.
    let initialized = server.request(
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "1" },
        }),
    );
    assert_eq!(initialized["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(initialized["result"]["serverInfo"]["name"], "reviewpad");
    assert!(initialized["result"]["capabilities"]["tools"].is_object());
    // A notification is not answered, so the next reply must belong to the next
    // request — `request` asserts the id, which is what catches it.
    server.notify("notifications/initialized");

    let listed = server.request("tools/list", json!({}));
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    for expected in ["open_review", "request_review", "add_comment", "reply"] {
        assert!(
            names.contains(&expected),
            "{expected} is missing from {names:?}"
        );
    }

    // What is under review.
    let files: Value = serde_json::from_str(&server.call("list_files", json!({}))).unwrap();
    assert_eq!(files["base"], "working tree");
    assert_eq!(files["files"][0]["path"], "src.rs");

    // Writing a comment, and answering it.
    let added = server.call(
        "add_comment",
        json!({ "file": "src.rs", "line": 2, "body": "work() is undefined", "author": "claude" }),
    );
    assert!(added.contains("c1"), "{added}");
    assert!(
        server
            .call("reply", json!({ "id": "c1", "body": "fixing" }))
            .contains("c1.1")
    );

    let review: Value = serde_json::from_str(&server.call("list_comments", json!({}))).unwrap();
    assert_eq!(review["comments"][0]["author"], "claude");
    assert_eq!(review["comments"][0]["anchor"]["line"], 2);
    assert_eq!(review["comments"][0]["replies"][0]["id"], "c1.1");

    let markdown = server.call("export_review", json!({}));
    assert!(markdown.contains("work() is undefined"), "{markdown}");
    assert!(markdown.contains("`src.rs`"), "{markdown}");

    // A file with no changes cannot be commented on, and the model is told
    // which files can be — as a result it can act on, not a protocol error.
    let refused = server.call_expecting_failure(
        "add_comment",
        json!({ "file": "absent.rs", "line": 1, "body": "…" }),
    );
    assert!(refused.contains("src.rs"), "{refused}");

    server.call("remove_comment", json!({ "id": "c1" }));
    server.call("clear_review", json!({}));
    assert_eq!(
        server.call("export_review", json!({})),
        "No review comments."
    );
}

#[test]
fn a_branch_review_is_reachable_through_the_tools() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "reviewpad@example.com"]);
    git(root, &["config", "user.name", "ReviewPad Test"]);
    std::fs::write(root.join("kept.rs"), "fn main() {}\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "initial"]);

    git(root, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(root.join("added.rs"), "fn work() {}\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "branch work"]);

    let mut server = Server::start(root);
    let files: Value =
        serde_json::from_str(&server.call("list_files", json!({ "base": "main" }))).unwrap();
    assert_eq!(files["base"], "main...HEAD");
    assert_eq!(files["files"][0]["path"], "added.rs");

    server.call(
        "add_comment",
        json!({ "file": "added.rs", "line": 1, "body": "name it", "base": "main" }),
    );
    let review: Value = serde_json::from_str(&server.call("list_comments", json!({}))).unwrap();
    // The base is recorded, so a reader knows what line 1 refers to.
    assert_eq!(review["base"], "main...HEAD");
}

/// The round trip, with a stand-in for the panel.
///
/// A live process that has announced itself in the session file is all the
/// server needs to drive a review, so the whole handover — a round read and
/// consumed, the window left open, a close asked for rather than forced — can be
/// exercised without a window on screen. The stand-in behaves the way the panel
/// does: it waits for the close request, then takes its session file with it.
#[test]
fn a_round_reaches_the_agent_and_leaves_the_panel_open() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "reviewpad@example.com"]);
    git(root, &["config", "user.name", "ReviewPad Test"]);
    std::fs::write(root.join("src.rs"), "fn main() {}\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "initial"]);
    std::fs::write(root.join("src.rs"), "fn main() {\n    work();\n}\n").unwrap();

    let state = root.join(".reviewpad");
    let rounds = state.join("rounds");
    let session = state.join("session.json");
    let close = state.join("close");
    std::fs::create_dir_all(&rounds).unwrap();

    let mut panel = Command::new("sh")
        .arg("-c")
        .arg("while [ ! -f \"$1\" ]; do sleep 0.05; done; rm -f \"$2\"")
        .arg("panel")
        .arg(&close)
        .arg(&session)
        .spawn()
        .expect("could not start the stand-in panel");
    std::fs::write(
        &session,
        json!({ "pid": panel.id(), "submit_to": rounds.display().to_string() }).to_string(),
    )
    .unwrap();

    let mut server = Server::start(root);

    // A round the person submitted before anybody asked for it. Waiting is not
    // required for it to be kept, and the next request is handed it.
    std::fs::write(
        rounds.join("00000000000000000001.md"),
        "# Code review\n\nRename `work` to something honest.\n",
    )
    .unwrap();

    let round = server.call("request_review", json!({}));
    assert!(round.contains("Rename `work`"), "{round}");
    // The panel is still up, and the answer says how to use it.
    assert!(round.contains("still open"), "{round}");
    assert!(round.contains("reply"), "{round}");
    assert!(session.exists(), "the panel was closed by being read");

    // Consumed: the same notes are not handed out again as a second round.
    assert_eq!(waiting_rounds(&rounds), 0);

    // The person can answer a reply with another round, which the next request
    // picks up on its own.
    std::fs::write(
        rounds.join("00000000000000000002.md"),
        "# Code review\n\nStill reads oddly.\n",
    )
    .unwrap();
    let second = server.call("request_review", json!({}));
    assert!(second.contains("Still reads oddly"), "{second}");
    assert!(!second.contains("Rename `work`"), "{second}");

    // Closing asks rather than kills, and reports a round nobody had read.
    std::fs::write(
        rounds.join("00000000000000000003.md"),
        "# Code review\n\nOne last thing.\n",
    )
    .unwrap();
    let closed = server.call("close_review", json!({}));
    assert!(closed.contains("Closed the review panel"), "{closed}");
    assert!(closed.contains("One last thing"), "{closed}");
    assert!(close.exists(), "the close was never requested");

    let _ = panel.wait();

    // With no panel open there is nothing to close, and saying so is not a
    // failure the model has to handle.
    let again = server.call("close_review", json!({}));
    assert!(again.contains("No review panel is open"), "{again}");
}

/// Rounds submitted but not yet read.
fn waiting_rounds(directory: &Path) -> usize {
    std::fs::read_dir(directory)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "md")
        })
        .count()
}

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .unwrap();
    assert!(status.success(), "git {} failed", args.join(" "));
}
