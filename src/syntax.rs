//! Tree-sitter syntax highlighting for diff bodies.
//!
//! Diffs are fragments, and a parser needs a whole document, so ReviewPad
//! highlights the *files* a hunk came from — the working-tree copy for the new
//! side, the `HEAD` blob for the old one — then indexes the resulting spans by
//! line number. That is the same trick Zed plays: highlight the buffer, paint
//! the diff from it.

use std::{collections::HashMap, ops::Range};

use tree_sitter::Language;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

use crate::git::{DiffLine, FileDiff, LineKind, Repository};

/// Capture names we recognize, in the order their colors are listed in
/// [`SCOPE_COLORS`]. Tree-sitter matches the longest recognized prefix, so
/// `function.method` falls back to `function` when it is absent here.
const SCOPE_NAMES: &[&str] = &[
    "attribute",
    "comment",
    "constant",
    "constant.builtin",
    "constructor",
    "embedded",
    "function",
    "function.builtin",
    "function.method",
    "keyword",
    "label",
    "number",
    "operator",
    "property",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "punctuation.special",
    "string",
    "string.escape",
    "string.special",
    "tag",
    "type",
    "type.builtin",
    "variable",
    "variable.builtin",
    "variable.parameter",
];

/// One Dark, the palette Zed ships as its default dark theme. Indices line up
/// with [`SCOPE_NAMES`]; the second field marks italics.
pub const SCOPE_COLORS: &[(u32, bool)] = &[
    (0xbf956a, false), // attribute
    (0x5d636f, true),  // comment
    (0xbf956a, false), // constant
    (0xbf956a, false), // constant.builtin
    (0xdfc184, false), // constructor
    (0xc8ccd4, false), // embedded
    (0x74ade8, false), // function
    (0x74ade8, false), // function.builtin
    (0x74ade8, false), // function.method
    (0xb477cf, false), // keyword
    (0xd07277, false), // label
    (0xbf956a, false), // number
    (0x9aa3b2, false), // operator
    (0xd07277, false), // property
    (0x8b93a1, false), // punctuation
    (0x8b93a1, false), // punctuation.bracket
    (0x8b93a1, false), // punctuation.delimiter
    (0xb477cf, false), // punctuation.special
    (0xa1c181, false), // string
    (0xbf956a, false), // string.escape
    (0xa1c181, false), // string.special
    (0xd07277, false), // tag
    (0xdfc184, false), // type
    (0xdfc184, false), // type.builtin
    (0xc8ccd4, false), // variable
    (0xd07277, false), // variable.builtin
    (0xc8ccd4, false), // variable.parameter
];

/// Files past this size are left unhighlighted — parsing them would stall the
/// frame for no real benefit in a review.
const MAX_SOURCE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Grammar {
    Rust,
    JavaScript,
    TypeScript,
    Tsx,
    Python,
    Go,
    Json,
    Css,
    Html,
    Bash,
}

impl Grammar {
    /// The label shown in the diff header.
    pub fn label(self) -> &'static str {
        match self {
            Grammar::Rust => "rust",
            Grammar::JavaScript => "javascript",
            Grammar::TypeScript => "typescript",
            Grammar::Tsx => "tsx",
            Grammar::Python => "python",
            Grammar::Go => "go",
            Grammar::Json => "json",
            Grammar::Css => "css",
            Grammar::Html => "html",
            Grammar::Bash => "shell",
        }
    }

    pub fn for_path(path: &str) -> Option<Self> {
        let name = path.rsplit('/').next().unwrap_or(path);
        let extension = name.rsplit_once('.').map(|(_, extension)| extension)?;
        Some(match extension {
            "rs" => Grammar::Rust,
            "js" | "mjs" | "cjs" | "jsx" => Grammar::JavaScript,
            "ts" | "mts" | "cts" => Grammar::TypeScript,
            "tsx" => Grammar::Tsx,
            "py" | "pyi" => Grammar::Python,
            "go" => Grammar::Go,
            "json" | "jsonc" => Grammar::Json,
            "css" | "scss" => Grammar::Css,
            "html" | "htm" => Grammar::Html,
            "sh" | "bash" | "zsh" => Grammar::Bash,
            _ => return None,
        })
    }

    fn language(self) -> Language {
        match self {
            Grammar::Rust => tree_sitter_rust::LANGUAGE.into(),
            Grammar::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Grammar::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Grammar::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Grammar::Python => tree_sitter_python::LANGUAGE.into(),
            Grammar::Go => tree_sitter_go::LANGUAGE.into(),
            Grammar::Json => tree_sitter_json::LANGUAGE.into(),
            Grammar::Css => tree_sitter_css::LANGUAGE.into(),
            Grammar::Html => tree_sitter_html::LANGUAGE.into(),
            Grammar::Bash => tree_sitter_bash::LANGUAGE.into(),
        }
    }

    /// The TypeScript and JSX grammars ship queries that extend JavaScript's
    /// rather than replace it, so those are concatenated.
    fn highlights_query(self) -> String {
        match self {
            Grammar::Rust => tree_sitter_rust::HIGHLIGHTS_QUERY.to_string(),
            Grammar::JavaScript => format!(
                "{}{}",
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY
            ),
            Grammar::TypeScript => format!(
                "{}{}",
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_typescript::HIGHLIGHTS_QUERY
            ),
            Grammar::Tsx => format!(
                "{}{}{}",
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                tree_sitter_typescript::HIGHLIGHTS_QUERY
            ),
            Grammar::Python => tree_sitter_python::HIGHLIGHTS_QUERY.to_string(),
            Grammar::Go => tree_sitter_go::HIGHLIGHTS_QUERY.to_string(),
            Grammar::Json => tree_sitter_json::HIGHLIGHTS_QUERY.to_string(),
            Grammar::Css => tree_sitter_css::HIGHLIGHTS_QUERY.to_string(),
            Grammar::Html => tree_sitter_html::HIGHLIGHTS_QUERY.to_string(),
            Grammar::Bash => tree_sitter_bash::HIGHLIGHT_QUERY.to_string(),
        }
    }

    fn injections_query(self) -> &'static str {
        match self {
            Grammar::Rust => tree_sitter_rust::INJECTIONS_QUERY,
            Grammar::JavaScript | Grammar::TypeScript | Grammar::Tsx => {
                tree_sitter_javascript::INJECTIONS_QUERY
            }
            Grammar::Html => tree_sitter_html::INJECTIONS_QUERY,
            _ => "",
        }
    }
}

/// A single styled run inside one line: a byte range and an index into
/// [`SCOPE_COLORS`].
pub type Span = (Range<usize>, usize);

/// Highlight spans for a whole file, addressable by 1-based line number.
#[derive(Debug, Default)]
pub struct HighlightedSource {
    lines: Vec<Line>,
}

#[derive(Debug, Default)]
struct Line {
    /// Byte length of the source line, used to reject spans when the diff text
    /// and the file have drifted apart (a stale working tree, say).
    len: usize,
    spans: Vec<Span>,
}

impl HighlightedSource {
    /// Spans for a 1-based line, or `None` when the line is unknown or its
    /// length no longer matches what the diff carries.
    pub fn line(&self, number: u32, expected_len: usize) -> Option<&[Span]> {
        let line = self.lines.get(number.checked_sub(1)? as usize)?;
        (line.len == expected_len).then_some(line.spans.as_slice())
    }
}

/// Owns the parser and the per-grammar query configurations, which are costly
/// enough to build that they are cached for the life of the window.
pub struct SyntaxIndex {
    highlighter: Highlighter,
    configs: HashMap<Grammar, Option<HighlightConfiguration>>,
}

impl Default for SyntaxIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl SyntaxIndex {
    pub fn new() -> Self {
        Self {
            highlighter: Highlighter::new(),
            configs: HashMap::new(),
        }
    }

    pub fn highlight(&mut self, grammar: Grammar, source: &str) -> Option<HighlightedSource> {
        if source.len() > MAX_SOURCE_BYTES {
            return None;
        }

        let config = self
            .configs
            .entry(grammar)
            .or_insert_with(|| build_config(grammar))
            .as_ref()?;

        let events = self
            .highlighter
            .highlight(config, source.as_bytes(), None, |_| None)
            .ok()?;

        let mut lines = line_ranges(source);
        let mut stack = Vec::new();
        for event in events {
            match event.ok()? {
                HighlightEvent::HighlightStart(highlight) => stack.push(highlight.0),
                HighlightEvent::HighlightEnd => {
                    stack.pop();
                }
                HighlightEvent::Source { start, end } => {
                    if let Some(&scope) = stack.last() {
                        distribute(&mut lines, start..end, scope);
                    }
                }
            }
        }

        Some(HighlightedSource {
            lines: lines.into_iter().map(|(_, line)| line).collect(),
        })
    }
}

fn build_config(grammar: Grammar) -> Option<HighlightConfiguration> {
    let mut config = HighlightConfiguration::new(
        grammar.language(),
        grammar.label(),
        &grammar.highlights_query(),
        grammar.injections_query(),
        "",
    )
    .ok()?;
    config.configure(SCOPE_NAMES);
    Some(config)
}

/// One entry per source line: its global byte offset plus the line being built.
fn line_ranges(source: &str) -> Vec<(usize, Line)> {
    let mut lines = Vec::new();
    let mut offset = 0;
    for text in source.split_inclusive('\n') {
        let len = text.trim_end_matches(['\n', '\r']).len();
        lines.push((
            offset,
            Line {
                len,
                spans: Vec::new(),
            },
        ));
        offset += text.len();
    }
    lines
}

/// Clip a global highlight range onto the lines it covers, storing each piece
/// as a line-local range.
fn distribute(lines: &mut [(usize, Line)], range: Range<usize>, scope: usize) {
    // Binary search for the first line that can overlap, so a large file does
    // not turn this into a quadratic scan.
    let first = lines
        .partition_point(|(offset, line)| offset + line.len < range.start)
        .saturating_sub(1);

    for (offset, line) in &mut lines[first..] {
        if *offset > range.end {
            break;
        }
        let start = range.start.saturating_sub(*offset);
        let end = range.end.saturating_sub(*offset).min(line.len);
        if start < end {
            line.spans.push((start..end, scope));
        }
    }
}

/// Syntax spans for one file's diff. Both sides are kept, so deleted lines
/// highlight against the committed copy rather than the working tree, which no
/// longer contains them.
#[derive(Default)]
pub struct DiffHighlight {
    pub grammar: Option<Grammar>,
    new: Option<HighlightedSource>,
    old: Option<HighlightedSource>,
}

impl DiffHighlight {
    /// Parse whichever sides of `file` its hunks actually reference. A file in
    /// an unsupported language, or one that has since been deleted, simply
    /// yields no spans and renders in flat diff colors.
    pub fn load(repository: &Repository, file: &FileDiff, index: &mut SyntaxIndex) -> Self {
        let Some(grammar) = Grammar::for_path(&file.path) else {
            return Self::default();
        };

        let working = repository.working_source(&file.path);
        let new = working.and_then(|source| index.highlight(grammar, &source));

        let has_deletions = file
            .lines
            .iter()
            .any(|line| line.kind == LineKind::Deletion);
        let committed = has_deletions
            .then(|| repository.head_source(&file.path))
            .flatten();
        let old = committed.and_then(|source| index.highlight(grammar, &source));

        Self {
            grammar: Some(grammar),
            new,
            old,
        }
    }

    /// Spans for a diff line, taken from whichever side that line belongs to.
    pub fn spans(&self, line: &DiffLine) -> Option<&[Span]> {
        let len = line.code().len();
        match line.kind {
            LineKind::Deletion => self.old.as_ref()?.line(line.old_line?, len),
            LineKind::Addition | LineKind::Context => self.new.as_ref()?.line(line.new_line?, len),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_grammars_from_extensions() {
        assert_eq!(Grammar::for_path("src/app.rs"), Some(Grammar::Rust));
        assert_eq!(Grammar::for_path("web/Page.tsx"), Some(Grammar::Tsx));
        assert_eq!(Grammar::for_path("Makefile"), None);
    }

    /// Every span the highlighter reports for `number`, paired with the scope
    /// name it resolved to.
    fn scoped<'a>(
        highlighted: &HighlightedSource,
        source: &'a str,
        number: u32,
    ) -> Vec<(&'a str, &'static str)> {
        let text = source.lines().nth(number as usize - 1).unwrap();
        highlighted
            .line(number, text.len())
            .expect("line is highlighted")
            .iter()
            .map(|(range, scope)| (&text[range.clone()], SCOPE_NAMES[*scope]))
            .collect()
    }

    #[test]
    fn highlights_are_indexed_by_line() {
        let source = "fn main() {\n    let value = \"text\";\n}\n";
        let highlighted = SyntaxIndex::new()
            .highlight(Grammar::Rust, source)
            .expect("rust grammar should load");

        assert!(scoped(&highlighted, source, 1).contains(&("fn", "keyword")));

        let second = scoped(&highlighted, source, 2);
        assert!(second.contains(&("let", "keyword")));
        assert!(second.contains(&("\"text\"", "string")));
    }

    #[test]
    fn stale_line_lengths_are_rejected() {
        let highlighted = SyntaxIndex::new()
            .highlight(Grammar::Rust, "fn main() {}\n")
            .expect("rust grammar should load");
        assert!(highlighted.line(1, 12).is_some());
        assert!(highlighted.line(1, 99).is_none());
    }
}
