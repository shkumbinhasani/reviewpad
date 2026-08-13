use anyhow::{Context, Result, bail};
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

use crate::review::Side;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    pub root: PathBuf,
    pub git_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSet {
    pub files: Vec<FileDiff>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub additions: usize,
    pub deletions: usize,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Header,
    Hunk,
    Context,
    Addition,
    Deletion,
}

impl FileDiff {
    /// A file to look at rather than read a patch of.
    ///
    /// Render output is normally gitignored — `out/` in a Remotion project —
    /// so a rendered video never reaches the diff at all. It is still the thing
    /// under review, so it can be named directly and carries no lines.
    pub fn media(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            additions: 0,
            deletions: 0,
            lines: Vec::new(),
        }
    }

    /// Whether this entry came from a patch or was named directly.
    pub fn is_media(&self) -> bool {
        self.lines.is_empty()
    }

    /// Index of the diff row carrying a given side and line number.
    pub fn index_of(&self, side: Side, line: u32) -> Option<usize> {
        self.lines
            .iter()
            .position(|row| row.anchor() == Some((side, line)))
    }

    /// A few rows either side of `index`, quoted with a comment so the review
    /// brief carries the change it refers to.
    pub fn context_at(&self, index: usize) -> String {
        let start = index.saturating_sub(2);
        let end = (index + 3).min(self.lines.len());
        self.lines[start..end]
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl DiffLine {
    /// The line as it appears in the file, without the diff marker git prefixes
    /// it with. This is what lines up with a syntax-highlighted source.
    pub fn code(&self) -> &str {
        match self.kind {
            LineKind::Addition | LineKind::Deletion => &self.text[1..],
            _ => self.text.strip_prefix(' ').unwrap_or(&self.text),
        }
    }

    pub fn anchor(&self) -> Option<(Side, u32)> {
        if let Some(line) = self.new_line {
            Some((Side::New, line))
        } else {
            self.old_line.map(|line| (Side::Old, line))
        }
    }
}

impl Repository {
    pub fn discover(path: &Path) -> Result<Self> {
        let canonical = path
            .canonicalize()
            .with_context(|| format!("{} does not exist", path.display()))?;
        let root = git_text(&canonical, &["rev-parse", "--show-toplevel"])?;
        let root = PathBuf::from(root.trim());
        let git_dir = git_text(&root, &["rev-parse", "--absolute-git-dir"])?;
        Ok(Self {
            root,
            git_dir: PathBuf::from(git_dir.trim()),
        })
    }

    /// Where a review lives: a `.reviewpad` directory at the repository root,
    /// so agents can find and read it without digging through `.git`. The
    /// directory ignores itself — see `Review::save`.
    pub fn review_path(&self) -> PathBuf {
        self.root.join(".reviewpad").join("comments.json")
    }

    /// Where reviews lived before that, kept so an existing one migrates
    /// forward instead of disappearing.
    pub fn legacy_review_path(&self) -> PathBuf {
        self.git_dir.join("reviewpad").join("comments.json")
    }

    /// The Git identity configured for this repository, used to look up the
    /// local user's avatar.
    pub fn user_email(&self) -> Option<String> {
        let output = git_output(&self.root, &["config", "user.email"]).ok()?;
        let email = String::from_utf8(output.stdout).ok()?.trim().to_string();
        (output.status.success() && !email.is_empty()).then_some(email)
    }

    /// The working-tree copy of a file — the "new" side of the diff, and what a
    /// syntax highlighter needs to make sense of a hunk.
    pub fn working_source(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(path)).ok()
    }

    /// The committed copy of a file, for highlighting deleted lines.
    pub fn head_source(&self, path: &str) -> Option<String> {
        let output = git_output(&self.root, &["show", &format!("HEAD:{path}")]).ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8(output.stdout).ok())
            .flatten()
    }

    pub fn diff(&self) -> Result<DiffSet> {
        let patch = tracked_patch(&self.root)?;
        let mut diff = parse_unified_diff(&patch);
        diff.files.extend(self.untracked_files()?);
        Ok(diff)
    }

    /// Untracked files, read directly rather than diffed one subprocess at a
    /// time.
    ///
    /// `git diff --no-index` per file is fine for a handful and ruinous for a
    /// real project: a Remotion tree with 372 untracked files spent 8.3s in
    /// subprocesses before the window could open, and expanded 472MB of audio
    /// and stills into a patch nobody can read. Anything binary or oversized
    /// becomes an entry with no lines — it is listed, and looked at rather than
    /// diffed.
    fn untracked_files(&self) -> Result<Vec<FileDiff>> {
        let mut files = Vec::new();

        for path in untracked_paths(&self.root)? {
            let full = self.root.join(&path);
            let size = std::fs::metadata(&full).map(|meta| meta.len()).unwrap_or(0);
            if size > MAX_UNTRACKED_BYTES {
                files.push(FileDiff::media(path));
                continue;
            }

            let Ok(bytes) = std::fs::read(&full) else {
                continue;
            };
            // A null byte is the same test git uses to call a file binary.
            if bytes.contains(&0) {
                files.push(FileDiff::media(path));
                continue;
            }

            let text = String::from_utf8_lossy(&bytes);
            let mut lines = vec![DiffLine {
                kind: LineKind::Header,
                old_line: None,
                new_line: None,
                text: format!("new file {path}"),
            }];
            lines.extend(text.lines().enumerate().map(|(index, line)| DiffLine {
                kind: LineKind::Addition,
                old_line: None,
                new_line: Some(index as u32 + 1),
                text: format!("+{line}"),
            }));

            files.push(FileDiff {
                path,
                additions: lines.len().saturating_sub(1),
                deletions: 0,
                lines,
            });
        }

        Ok(files)
    }
}

/// Untracked files past this are listed but not expanded into the diff.
const MAX_UNTRACKED_BYTES: u64 = 512 * 1024;

fn tracked_patch(root: &Path) -> Result<String> {
    let args = [
        "diff",
        "--no-ext-diff",
        "--no-color",
        "--find-renames",
        "--unified=3",
        "HEAD",
        "--",
    ];
    let output = git_output(root, &args)?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }

    // An unborn repository has no HEAD. The empty tree lets staged files still
    // participate while the no-index pass below handles untracked files.
    let empty_tree = git_text(root, &["hash-object", "-t", "tree", "/dev/null"])?;
    let output = git_output(
        root,
        &[
            "diff",
            "--no-ext-diff",
            "--no-color",
            "--cached",
            empty_tree.trim(),
            "--",
        ],
    )?;
    if !output.status.success() {
        bail!(
            "git diff failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn untracked_paths(root: &Path) -> Result<Vec<String>> {
    let output = git_output(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    if !output.status.success() {
        bail!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect())
}

fn git_text(root: &Path, args: &[&str]) -> Result<String> {
    let output = git_output(root, args)?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn git_output(root: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))
}

pub fn parse_unified_diff(patch: &str) -> DiffSet {
    let mut files = Vec::<FileDiff>::new();
    let mut current: Option<FileDiff> = None;
    let mut old_line = None;
    let mut new_line = None;

    for raw in patch.lines() {
        if let Some(rest) = raw.strip_prefix("diff --git a/") {
            if let Some(file) = current.take() {
                files.push(file);
            }
            let path = rest
                .split_once(" b/")
                .map(|(_, path)| path)
                .unwrap_or(rest)
                .to_string();
            current = Some(FileDiff {
                path,
                additions: 0,
                deletions: 0,
                lines: Vec::new(),
            });
            old_line = None;
            new_line = None;
            continue;
        }

        let Some(file) = current.as_mut() else {
            continue;
        };

        if raw.starts_with("+++ ") {
            if let Some(path) = raw.strip_prefix("+++ b/") {
                // no-index diffs append a tab-delimited timestamp here.
                file.path = path.split('\t').next().unwrap_or(path).to_string();
            }
            file.lines.push(header(raw));
        } else if raw.starts_with("--- ")
            || raw.starts_with("index ")
            || raw.starts_with("new file ")
            || raw.starts_with("deleted file ")
            || raw.starts_with("similarity index ")
            || raw.starts_with("rename from ")
            || raw.starts_with("rename to ")
        {
            file.lines.push(header(raw));
        } else if raw.starts_with("@@") {
            let (old, new) = parse_hunk_header(raw);
            old_line = old;
            new_line = new;
            file.lines.push(DiffLine {
                kind: LineKind::Hunk,
                old_line: None,
                new_line: None,
                text: raw.to_string(),
            });
        } else if let Some(text) = raw.strip_prefix('+') {
            file.additions += 1;
            file.lines.push(DiffLine {
                kind: LineKind::Addition,
                old_line: None,
                new_line,
                text: format!("+{text}"),
            });
            new_line = new_line.map(|line| line + 1);
        } else if let Some(text) = raw.strip_prefix('-') {
            file.deletions += 1;
            file.lines.push(DiffLine {
                kind: LineKind::Deletion,
                old_line,
                new_line: None,
                text: format!("-{text}"),
            });
            old_line = old_line.map(|line| line + 1);
        } else if raw.starts_with(' ') || raw == "\\ No newline at end of file" {
            file.lines.push(DiffLine {
                kind: LineKind::Context,
                old_line,
                new_line,
                text: raw.to_string(),
            });
            if raw.starts_with(' ') {
                old_line = old_line.map(|line| line + 1);
                new_line = new_line.map(|line| line + 1);
            }
        }
    }
    if let Some(file) = current {
        files.push(file);
    }
    DiffSet { files }
}

fn header(text: &str) -> DiffLine {
    DiffLine {
        kind: LineKind::Header,
        old_line: None,
        new_line: None,
        text: text.to_string(),
    }
}

fn parse_hunk_header(line: &str) -> (Option<u32>, Option<u32>) {
    let mut parts = line.split_whitespace();
    let _at = parts.next();
    let old = parts.next().and_then(parse_range_start);
    let new = parts.next().and_then(parse_range_start);
    (old, new)
}

fn parse_range_start(range: &str) -> Option<u32> {
    range
        .trim_start_matches(['-', '+'])
        .split(',')
        .next()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, process::Command};

    #[test]
    fn parses_files_hunks_and_line_numbers() {
        let patch = "diff --git a/src/lib.rs b/src/lib.rs\nindex 111..222 100644\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -10,2 +10,3 @@\n same\n-old\n+new\n+extra\n";
        let diff = parse_unified_diff(patch);
        let file = &diff.files[0];
        assert_eq!(file.path, "src/lib.rs");
        assert_eq!((file.additions, file.deletions), (2, 1));
        let changed: Vec<_> = file
            .lines
            .iter()
            .filter(|line| matches!(line.kind, LineKind::Addition | LineKind::Deletion))
            .map(|line| (line.old_line, line.new_line))
            .collect();
        assert_eq!(
            changed,
            vec![(Some(11), None), (None, Some(11)), (None, Some(12))]
        );
    }

    #[test]
    fn discovers_repository_and_includes_untracked_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        run_git(root, &["init", "-q"]);
        run_git(root, &["config", "user.email", "reviewpad@example.com"]);
        run_git(root, &["config", "user.name", "ReviewPad Test"]);
        fs::write(root.join("tracked.txt"), "before\n").unwrap();
        run_git(root, &["add", "tracked.txt"]);
        run_git(root, &["commit", "-qm", "initial"]);

        fs::create_dir(root.join("nested")).unwrap();
        fs::write(root.join("tracked.txt"), "after\n").unwrap();
        fs::write(root.join("new file.txt"), "new\n").unwrap();

        let repository = Repository::discover(&root.join("nested")).unwrap();
        let diff = repository.diff().unwrap();
        assert_eq!(repository.root, root.canonicalize().unwrap());
        assert!(diff.files.iter().any(|file| file.path == "tracked.txt"));
        assert!(diff.files.iter().any(|file| file.path == "new file.txt"));
    }

    fn run_git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .unwrap();
        assert!(status.success(), "git {} failed", args.join(" "));
    }
}
