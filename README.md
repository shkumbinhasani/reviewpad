# ReviewPad

[![CI](https://github.com/shkumbinhasani/reviewpad/actions/workflows/ci.yml/badge.svg)](https://github.com/shkumbinhasani/reviewpad/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/shkumbinhasani/reviewpad)](https://github.com/shkumbinhasani/reviewpad/releases/latest)
[![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

ReviewPad is a local-first Git review tool with one Rust binary and two interfaces:

- a GPU-rendered desktop review panel built with [GPUI](https://www.gpui.rs/), the UI framework created for Zed;
- an agent-friendly CLI that can block while a human reviews, then return the completed review as Markdown on stdout.

It reads staged, unstaged, and untracked changes. Comments are anchored to old or new line numbers and stored under `.git/reviewpad/comments.json`, so they do not dirty the working tree.

## Install

```sh
brew install --cask shkumbinhasani/tap/reviewpad
```

That puts `ReviewPad.app` in `/Applications` and `reviewpad` on your `PATH`. The
build is a universal binary, ad-hoc signed and unnotarized, so the cask strips
the quarantine flag on your behalf.

You can also grab `ReviewPad-macos-universal.zip` or the bare
`reviewpad-macos-universal.tar.gz` from the
[latest release](https://github.com/shkumbinhasani/reviewpad/releases/latest);
`SHA256SUMS` is published alongside them.

### Updates

ReviewPad checks for a newer release when it opens and shows an unobtrusive
notice in the sidebar — the check is advisory and never blocks a review.

```sh
reviewpad update --check   # report what is available
reviewpad update           # download, verify and install it
```

`reviewpad update` verifies the download against the SHA-256 in the release
manifest and swaps the binary in atomically. Homebrew installs are left alone
on purpose — rewriting a file Homebrew tracks would desynchronize its manifest,
so those are pointed at `brew upgrade --cask reviewpad` instead.

## Build from source

GPUI currently supports macOS and Linux. On macOS, install Xcode and its optional Metal Toolchain, then run:

```sh
xcodebuild -downloadComponent metalToolchain
cargo build --release
cargo install --path .
```

To create a Finder-launchable macOS bundle (which opens a native repository picker):

```sh
./scripts/bundle-macos.sh          # host architecture
UNIVERSAL=1 ./scripts/bundle-macos.sh   # arm64 + x86_64, as released
open dist/ReviewPad.app
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
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

CI runs all three on every push and pull request, and separately proves the
`.app` bundle still builds.

## Releasing

The tag is the source of truth for the version, and the release job refuses to
publish if `Cargo.toml` disagrees with it.

```sh
# bump `version` in Cargo.toml first, then:
git tag v0.2.0 && git push origin v0.2.0
```

That builds a universal binary, bundles and ad-hoc signs `ReviewPad.app`,
publishes the archives with `SHA256SUMS` and the `latest.json` update manifest,
and pushes the new version to the Homebrew cask.

The cask step authenticates with `TAP_DEPLOY_KEY`, the private half of a write
deploy key on `shkumbinhasani/homebrew-tap`. The built-in `GITHUB_TOKEN` is
scoped to this repository and cannot push to another one; a deploy key is
narrower than a personal token, since it reaches exactly one repository and can
be revoked on its own.
