# ReviewPad as an MCP server

`reviewpad mcp` speaks the [Model Context Protocol](https://modelcontextprotocol.io)
over stdio. It is a plain subcommand of the CLI you already have — no daemon, no
port, no separate install. The client starts it, talks to it on stdin and
stdout, and stops it by closing the pipe.

What an agent gets from it: a way to ask *you* to look at a change, and then a
conversation about it. The panel stays open the whole time, and the agent's
answers appear in it as it writes them.

```
agent finishes a change
  └─ request_review  → your review panel opens
                     → you draft notes, press Send
                     → the agent gets that round as a Markdown brief
     reply           → its answers appear in the panel you are still reading
  └─ request_review  → waits for your next round
                     → you answer, or press Send with nothing left to say
     close_review    → the panel closes, unless you closed it first
```

## Install it in your client

Every client below runs the same command. If `reviewpad` is on your `PATH`,
that is all you need:

```sh
reviewpad mcp
```

Some clients launch servers from a GUI app with a minimal `PATH` and will not
find it. Use the absolute path there — `which reviewpad` prints it, typically
`/opt/homebrew/bin/reviewpad`.

<details open>
<summary><b>Claude Code</b></summary>

```sh
claude mcp add reviewpad -- reviewpad mcp
```

`-s user` makes it available in every project; `-s project` writes a `.mcp.json`
your team shares. Check it with `claude mcp list`, which health-checks each
server.

The project file, if you prefer to write it by hand:

```json
{
  "mcpServers": {
    "reviewpad": { "command": "reviewpad", "args": ["mcp"] }
  }
}
```

Claude Code aborts a stdio tool call that has been silent for 30 minutes. The
server sends progress notifications while it waits, which resets that clock, so
`request_review` survives a long review. To raise the ceiling anyway, add a
per-server `timeout` in milliseconds to the `.mcp.json` entry.
</details>

<details>
<summary><b>Codex</b></summary>

```sh
codex mcp add reviewpad -- reviewpad mcp
```

Or in `~/.codex/config.toml`:

```toml
[mcp_servers.reviewpad]
command = "reviewpad"
args = ["mcp"]
tool_timeout_sec = 1800
```

**Set `tool_timeout_sec`.** Codex gives a tool 60 seconds by default, which is
long enough for every tool here except `request_review` — a review takes as long
as reading the code takes. Without it, use `open_review` and poll.
</details>

<details>
<summary><b>opencode</b></summary>

In `opencode.json` (project) or `~/.config/opencode/opencode.json` (global):

```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "reviewpad": {
      "type": "local",
      "command": ["reviewpad", "mcp"],
      "enabled": true
    }
  }
}
```

Note the shape: `mcp` rather than `mcpServers`, and `command` is one array with
the arguments in it.
</details>

<details>
<summary><b>Cursor</b></summary>

`.cursor/mcp.json` in the project, or `~/.cursor/mcp.json` for every project:

```json
{
  "mcpServers": {
    "reviewpad": { "command": "reviewpad", "args": ["mcp"] }
  }
}
```
</details>

<details>
<summary><b>VS Code — GitHub Copilot</b></summary>

`.vscode/mcp.json`, or the user-level file from **MCP: Open User Configuration**:

```json
{
  "servers": {
    "reviewpad": { "type": "stdio", "command": "reviewpad", "args": ["mcp"] }
  }
}
```

The top-level key is `servers`, not `mcpServers`.
</details>

<details>
<summary><b>Zed</b></summary>

In `settings.json`:

```json
{
  "context_servers": {
    "reviewpad": { "command": "reviewpad", "args": ["mcp"], "env": {} }
  }
}
```
</details>

<details>
<summary><b>Gemini CLI</b></summary>

```sh
gemini mcp add reviewpad reviewpad mcp        # this project
gemini mcp add -s user reviewpad reviewpad mcp  # every project
```

Or in `.gemini/settings.json` per project, or `~/.gemini/settings.json` for all:

```json
{
  "mcpServers": {
    "reviewpad": { "command": "reviewpad", "args": ["mcp"], "timeout": 1800000 }
  }
}
```

`timeout` is in milliseconds.
</details>

<details>
<summary><b>Claude Desktop</b></summary>

`~/Library/Application Support/Claude/claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "reviewpad": { "command": "/opt/homebrew/bin/reviewpad", "args": ["mcp"] }
  }
}
```

Use the absolute path here. The desktop app does not inherit your shell's
`PATH`, so a bare `reviewpad` will not be found. Restart the app afterwards.
</details>

<details>
<summary><b>Anything else</b></summary>

The server is an ordinary stdio MCP server, so any client can run it:

- command: `reviewpad`
- args: `["mcp"]`

`reviewpad mcp <path>` takes the working tree that tools act on when a call does
not name one; it defaults to the directory the client starts the server in.
</details>

## The tools

| Tool | What it does |
| --- | --- |
| `open_review` | Open the panel and return at once |
| `request_review` | Open the panel and wait for a round of notes, then return the Markdown |
| `close_review` | Ask the open panel to close, once the exchange is done |
| `list_files` | The files under review, with their line counts |
| `list_comments` | Every comment and reply, as JSON, with ids |
| `export_review` | The review as an implementation brief |
| `add_comment` | Anchor a note to a line, a moment in a video, or a place on an image |
| `reply` | Answer a comment, continuing its thread |
| `remove_comment` | Delete one comment or reply |
| `clear_review` | Delete every comment |

Every tool takes an optional `repo`, and the ones that read a diff take an
optional `base` — `"main"` reviews `main...HEAD` rather than uncommitted work.

## Rounds

A note you write in the panel is a **draft** until you press Send. Drafts are
yours: `list_comments` shows them marked `submitted: false`, and the tool
descriptions tell the agent not to act on them. Pressing Send turns every draft
into a **round** and hands it over — and leaves the window open.

That is the part worth knowing. The agent replies in each thread as it works, and
those replies appear in your panel within a second of being written, so you watch
it work rather than waiting for a summary. Answer them by writing more notes and
pressing Send again; the agent's next `request_review` is handed that round.
Nothing left to say is also an answer: Send with no drafts tells the agent you
read its replies and are content, rather than leaving it waiting.

Only one panel is ever open per working tree. A panel announces itself in
`.reviewpad/session.json`, so a second `request_review` drives the window
already on screen instead of stacking another over the same review.

Either side can end it: close the window, or let the agent call `close_review`,
which asks the panel to save and exit rather than killing it.

A round submitted while nobody is waiting is not lost — it sits in
`.reviewpad/rounds` until the next `request_review` reads it. So reviewing before
the agent gets around to asking works fine.

## Waiting for a person

`request_review` blocks until you send a round. That is the point of it, and it
is also the one thing a protocol built for fast tool calls finds surprising. Two
ways through:

- **Let it block** — the default, and what an agent should reach for. The
  server emits a progress notification every 20 seconds, which is what keeps
  idle-timeout clients from giving up. Raise the client's tool timeout to match
  how long you actually take. Claude Code moves a call this long to a background
  task and picks it up when it returns, so the session is not held hostage.
- **Don't block.** `open_review` returns as soon as the window is up, and then
  nothing announces that you have sent anything — the agent has to poll
  `list_comments` and judge for itself. Comments are written to `.reviewpad` as
  they are made, so polling does see them arrive.

If the wait does time out, the window is left open — killing it would throw away
a review you were in the middle of writing — and the call returns whatever has
been saved so far. Calling `request_review` again keeps waiting on the same
panel.

## Checking it works

The server is just a program reading lines, so you can talk to it yourself:

```sh
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"probe","version":"1"}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  | reviewpad mcp .
```

Two JSON lines back means it works. If a client says it cannot connect:

- **`reviewpad: command not found`** — the client's `PATH` is not your shell's.
  Use the absolute path from `which reviewpad`.
- **Connects, but every tool fails** — the `repo` is not a Git working tree.
  Pass one explicitly, or start the server with `reviewpad mcp /path/to/repo`.
- **Nothing at all** — check the client's MCP log. The server writes protocol
  messages to stdout and everything else to stderr, so a crash shows up there.
