# ReviewPad

ReviewPad is a local-first Git review tool with one Rust binary and two interfaces:

- a GPU-rendered desktop review panel built with [GPUI](https://www.gpui.rs/), the UI framework created for Zed;
- an agent-friendly CLI that can block while a human reviews, then return the completed review as Markdown on stdout.

It reads staged, unstaged, and untracked changes. Comments are anchored to old or new line numbers and stored under `.git/reviewpad/comments.json`, so they do not dirty the working tree.

## Build

GPUI currently supports macOS and Linux. On macOS, install Xcode and its optional Metal Toolchain, then run:

```sh
xcodebuild -downloadComponent metalToolchain
cargo build --release
cargo install --path .
```

To create a Finder-launchable macOS bundle (which opens a native repository picker):

```sh
chmod +x scripts/bundle-macos.sh packaging/macos/reviewpad-launcher
./scripts/bundle-macos.sh
open target/release/ReviewPad.app
```

## Use it as a desktop app

From any directory inside a Git working tree:

```sh
reviewpad
```

Or point it at another working tree:

```sh
reviewpad open ../my-project
```

`reviewpad pick` opens the same native directory picker used by the `.app` bundle.

Select a changed file, click a code line, type a comment, and press `Cmd+Enter` (or click **Add comment**). **Copy Markdown** puts the complete implementation brief on the clipboard. **Finish review** saves and closes the panel.

## Use it from an AI agent

An agent should invoke:

```sh
reviewpad request /absolute/path/to/repository
```

The process opens the review panel and waits. When the user clicks **Finish review**, the process writes only the Markdown review to stdout and exits. The agent can then implement each item.

Noninteractive commands are also available:

```sh
reviewpad export .   # print the current review as Markdown
reviewpad clear .    # remove all saved comments
```

## Data and scope

- Diffs come from the current working tree relative to `HEAD`, plus untracked files.
- Review state is repository-local and survives closing the app.
- ReviewPad never stages, resets, commits, or modifies project files.
- The Markdown includes the repository path, exact file/line/side anchors, comment text, and nearby diff context.

## Development

```sh
cargo fmt --check
cargo test --all-targets
```
