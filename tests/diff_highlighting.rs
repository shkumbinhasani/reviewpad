//! Covers the seam between a unified diff and a syntax-highlighted file: hunk
//! line numbers have to land on the right source lines, on both sides of the
//! change, or the diff renders in the wrong colors.

use reviewpad::{
    git::{DiffLine, LineKind, Repository},
    syntax::{DiffHighlight, Grammar, SyntaxIndex},
};
use std::{fs, path::Path, process::Command};

const BEFORE: &str = "fn total(values: &[usize]) -> usize {\n    values.iter().sum()\n}\n";
const AFTER: &str = "\
fn total(values: &[usize]) -> usize {
    values.iter().copied().sum()
}

/// Added in the working tree.
pub fn double(value: usize) -> usize {
    value * 2
}
";

/// The text each span covers, so assertions read in terms of source rather than
/// byte offsets.
fn spans<'a>(highlight: &DiffHighlight, line: &'a DiffLine) -> Vec<&'a str> {
    highlight
        .spans(line)
        .unwrap_or_default()
        .iter()
        .map(|(range, _)| &line.code()[range.clone()])
        .collect()
}

#[test]
fn both_sides_of_a_hunk_resolve_to_source_spans() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    run_git(root, &["init", "-q"]);
    run_git(root, &["config", "user.email", "reviewpad@example.com"]);
    run_git(root, &["config", "user.name", "ReviewPad Test"]);
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/total.rs"), BEFORE).unwrap();
    run_git(root, &["add", "-A"]);
    run_git(root, &["commit", "-qm", "initial"]);
    fs::write(root.join("src/total.rs"), AFTER).unwrap();

    let repository = Repository::discover(root).unwrap();
    let diff = repository.diff().unwrap();
    let file = diff
        .files
        .iter()
        .find(|file| file.path == "src/total.rs")
        .expect("the edited file is in the diff");

    let highlight = DiffHighlight::load(&repository, file, &mut SyntaxIndex::new());
    assert_eq!(highlight.grammar, Some(Grammar::Rust));

    // An added line reads from the working tree.
    let added = file
        .lines
        .iter()
        .find(|line| line.kind == LineKind::Addition && line.text.contains("pub fn double"))
        .expect("the new function is an addition");
    assert!(spans(&highlight, added).contains(&"fn"));

    // A deleted line no longer exists on disk, so it has to come from HEAD.
    let deleted = file
        .lines
        .iter()
        .find(|line| line.kind == LineKind::Deletion)
        .expect("the rewritten line is a deletion");
    assert!(spans(&highlight, deleted).contains(&"iter"));

    // Context lines are shared by both sides and still highlight.
    let context = file
        .lines
        .iter()
        .find(|line| line.kind == LineKind::Context && line.text.contains("fn total"))
        .expect("the signature is unchanged context");
    assert!(spans(&highlight, context).contains(&"fn"));
}

#[test]
fn unsupported_languages_fall_back_to_flat_colors() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    run_git(root, &["init", "-q"]);
    run_git(root, &["config", "user.email", "reviewpad@example.com"]);
    run_git(root, &["config", "user.name", "ReviewPad Test"]);
    fs::write(root.join("Makefile"), "build:\n\tcargo build\n").unwrap();

    let repository = Repository::discover(root).unwrap();
    let diff = repository.diff().unwrap();
    let file = &diff.files[0];

    let highlight = DiffHighlight::load(&repository, file, &mut SyntaxIndex::new());
    assert_eq!(highlight.grammar, None);
    assert!(
        file.lines
            .iter()
            .all(|line| highlight.spans(line).is_none())
    );
}

fn run_git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .unwrap();
    assert!(status.success(), "git {} failed", args.join(" "));
}
