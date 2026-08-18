//! The little bit of Markdown a review conversation is written in.
//!
//! Agents answer in Markdown whether or not anybody asked them to — backticked
//! identifiers, fenced patches, bulleted lists of what they changed — and a
//! panel that shows the source of that is a panel that makes the person read
//! punctuation. So the bodies are parsed here, into blocks flat enough for the
//! view to lay out directly.
//!
//! It is deliberately not a Markdown implementation. It covers what turns up in
//! a review note and leaves everything else as the characters that were typed,
//! which is the failure mode worth having: unrecognized syntax reads as itself
//! rather than disappearing.

/// A run of text with whatever emphasis was around it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Span {
    pub text: String,
    /// `` `like this` `` — an identifier, a path, a command.
    pub code: bool,
    /// `**like this**`.
    pub strong: bool,
    /// `*like this*`.
    pub emphasis: bool,
}

impl Span {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }
}

/// One stacked piece of a body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Paragraph(Vec<Span>),
    /// A fenced block, with the language if the fence named one.
    Code {
        language: Option<String>,
        text: String,
    },
    /// One line of a list, carrying the marker it should be drawn with so a
    /// numbered list keeps its numbers.
    Item {
        marker: String,
        spans: Vec<Span>,
    },
    Quote(Vec<Span>),
    Heading {
        level: u8,
        spans: Vec<Span>,
    },
    Rule,
}

/// Split a body into blocks.
pub fn parse(source: &str) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut paragraph: Vec<String> = Vec::new();
    let mut lines = source.lines().peekable();

    while let Some(line) = lines.next() {
        let line = line.trim_end();

        // A fence swallows everything up to its partner, untouched — the point
        // of a code block is that nothing happens to what is inside it.
        if fence(line).is_some() {
            flush(&mut paragraph, &mut blocks);
            let language = fence(line).unwrap_or_default().trim().to_string();
            let mut code: Vec<&str> = Vec::new();
            for line in lines.by_ref() {
                if fence(line.trim_end()).is_some() {
                    break;
                }
                code.push(line);
            }
            blocks.push(Block::Code {
                language: (!language.is_empty()).then_some(language),
                text: code.join("\n"),
            });
            continue;
        }

        if line.trim().is_empty() {
            flush(&mut paragraph, &mut blocks);
            continue;
        }

        if let Some((level, rest)) = heading(line) {
            flush(&mut paragraph, &mut blocks);
            blocks.push(Block::Heading {
                level,
                spans: inline(rest),
            });
            continue;
        }

        if is_rule(line) {
            flush(&mut paragraph, &mut blocks);
            blocks.push(Block::Rule);
            continue;
        }

        if let Some((marker, rest)) = item(line) {
            flush(&mut paragraph, &mut blocks);
            blocks.push(Block::Item {
                marker,
                spans: inline(rest),
            });
            continue;
        }

        if let Some(rest) = line.trim_start().strip_prefix('>') {
            flush(&mut paragraph, &mut blocks);
            blocks.push(Block::Quote(inline(rest.trim_start())));
            continue;
        }

        paragraph.push(line.to_string());
    }

    flush(&mut paragraph, &mut blocks);
    blocks
}

/// Close off the paragraph being gathered, if there is one. Its own line breaks
/// are kept: somebody writing a note in short lines meant them.
fn flush(paragraph: &mut Vec<String>, blocks: &mut Vec<Block>) {
    if paragraph.is_empty() {
        return;
    }
    let text = paragraph.join("\n");
    paragraph.clear();
    blocks.push(Block::Paragraph(inline(&text)));
}

/// The language after a ``` fence, or nothing if this is not a fence.
fn fence(line: &str) -> Option<&str> {
    let line = line.trim_start();
    line.strip_prefix("```")
        .or_else(|| line.strip_prefix("~~~"))
}

fn heading(line: &str) -> Option<(u8, &str)> {
    let hashes = line.len() - line.trim_start_matches('#').len();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = line[hashes..].strip_prefix(' ')?;
    Some((hashes as u8, rest))
}

fn is_rule(line: &str) -> bool {
    let line = line.trim();
    line.len() >= 3
        && (line.chars().all(|glyph| glyph == '-')
            || line.chars().all(|glyph| glyph == '*')
            || line.chars().all(|glyph| glyph == '_'))
}

/// A list marker and what follows it: `- `, `* `, `+ `, or `12. `.
fn item(line: &str) -> Option<(String, &str)> {
    let trimmed = line.trim_start();

    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(bullet) {
            return Some(("•".to_string(), rest));
        }
    }

    let digits = trimmed.len()
        - trimmed
            .trim_start_matches(|glyph: char| glyph.is_ascii_digit())
            .len();
    if digits == 0 {
        return None;
    }
    let rest = trimmed[digits..].strip_prefix(". ")?;
    Some((format!("{}.", &trimmed[..digits]), rest))
}

/// Split a line into runs of emphasis.
///
/// `_` is left alone on purpose: `snake_case` names are everywhere in the notes
/// this renders, and mangling an identifier into italics loses information that
/// italics do not carry. `*` is the marker that means it here.
pub fn inline(text: &str) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    let mut plain = String::new();
    let glyphs: Vec<char> = text.chars().collect();
    let mut at = 0;

    while at < glyphs.len() {
        // Backticks: however many open it, the same number close it, and what
        // is between them is literal.
        if glyphs[at] == '`' {
            let ticks = run_of(&glyphs, at, '`');
            if let Some(close) = closing_run(&glyphs, at + ticks, '`', ticks) {
                push(&mut spans, &mut plain);
                spans.push(Span {
                    text: glyphs[at + ticks..close].iter().collect(),
                    code: true,
                    ..Span::default()
                });
                at = close + ticks;
                continue;
            }
        }

        if glyphs[at] == '*' {
            let stars = run_of(&glyphs, at, '*').min(2);
            if let Some(close) = closing_run(&glyphs, at + stars, '*', stars) {
                let inner: String = glyphs[at + stars..close].iter().collect();
                if !inner.trim().is_empty() {
                    push(&mut spans, &mut plain);
                    // Nested emphasis is not a thing a review note needs; the
                    // inner text keeps its own markers if it had any.
                    spans.push(Span {
                        text: inner,
                        strong: stars == 2,
                        emphasis: stars == 1,
                        ..Span::default()
                    });
                    at = close + stars;
                    continue;
                }
            }
        }

        plain.push(glyphs[at]);
        at += 1;
    }

    push(&mut spans, &mut plain);
    spans
}

fn push(spans: &mut Vec<Span>, plain: &mut String) {
    if !plain.is_empty() {
        spans.push(Span::plain(std::mem::take(plain)));
    }
}

/// How many of `marker` start at `from`.
fn run_of(glyphs: &[char], from: usize, marker: char) -> usize {
    glyphs[from..]
        .iter()
        .take_while(|glyph| **glyph == marker)
        .count()
}

/// Where the matching run of `length` markers starts, searching from `from`.
fn closing_run(glyphs: &[char], from: usize, marker: char, length: usize) -> Option<usize> {
    let mut at = from;
    while at < glyphs.len() {
        if glyphs[at] == marker && run_of(glyphs, at, marker) >= length {
            return (at > from).then_some(at);
        }
        at += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paragraph(source: &str) -> Vec<Span> {
        match parse(source).into_iter().next() {
            Some(Block::Paragraph(spans)) => spans,
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    #[test]
    fn plain_prose_is_one_span() {
        assert_eq!(
            paragraph("Handle the error."),
            vec![Span::plain("Handle the error.")]
        );
    }

    #[test]
    fn backticks_mark_code() {
        let spans = paragraph("call `work()` first");
        assert_eq!(spans[0], Span::plain("call "));
        assert_eq!(spans[1].text, "work()");
        assert!(spans[1].code);
        assert_eq!(spans[2], Span::plain(" first"));
    }

    #[test]
    fn stars_mark_emphasis() {
        let spans = paragraph("**must** and *maybe*");
        assert!(spans[0].strong && !spans[0].emphasis);
        assert_eq!(spans[0].text, "must");
        assert!(spans[2].emphasis && !spans[2].strong);
        assert_eq!(spans[2].text, "maybe");
    }

    /// The failure mode worth having: what is not understood reads as what was
    /// typed, rather than vanishing.
    #[test]
    fn unclosed_markers_stay_literal() {
        assert_eq!(
            paragraph("2 * 3 is `six"),
            vec![Span::plain("2 * 3 is `six")]
        );
    }

    /// Identifiers are the substance of a review note. Underscores in them are
    /// not emphasis.
    #[test]
    fn snake_case_survives() {
        assert_eq!(
            paragraph("rename open_review_window"),
            vec![Span::plain("rename open_review_window")]
        );
    }

    #[test]
    fn a_fence_keeps_its_contents_and_language() {
        let blocks = parse("before\n\n```rust\nlet x = *y;\n```\nafter");
        assert_eq!(
            blocks[1],
            Block::Code {
                language: Some("rust".into()),
                text: "let x = *y;".into(),
            }
        );
        assert_eq!(blocks[2], Block::Paragraph(vec![Span::plain("after")]));
    }

    #[test]
    fn an_unclosed_fence_runs_to_the_end() {
        let blocks = parse("```\nstill code");
        assert_eq!(
            blocks[0],
            Block::Code {
                language: None,
                text: "still code".into(),
            }
        );
    }

    #[test]
    fn lists_keep_their_markers() {
        let blocks = parse("- one\n- two\n\n1. first\n2. second");
        assert_eq!(
            blocks[0],
            Block::Item {
                marker: "•".into(),
                spans: vec![Span::plain("one")]
            }
        );
        assert_eq!(
            blocks[2],
            Block::Item {
                marker: "1.".into(),
                spans: vec![Span::plain("first")]
            }
        );
        assert_eq!(
            blocks[3],
            Block::Item {
                marker: "2.".into(),
                spans: vec![Span::plain("second")]
            }
        );
    }

    #[test]
    fn headings_quotes_and_rules_are_their_own_blocks() {
        let blocks = parse("## Why\n> because\n\n---");
        assert_eq!(
            blocks[0],
            Block::Heading {
                level: 2,
                spans: vec![Span::plain("Why")]
            }
        );
        assert_eq!(blocks[1], Block::Quote(vec![Span::plain("because")]));
        assert_eq!(blocks[2], Block::Rule);
        // A bare `#` with nothing after it is a character, not a heading.
        assert_eq!(
            parse("#tag"),
            vec![Block::Paragraph(vec![Span::plain("#tag")])]
        );
    }

    #[test]
    fn line_breaks_inside_a_paragraph_are_kept() {
        assert_eq!(
            paragraph("first line\nsecond line"),
            vec![Span::plain("first line\nsecond line")]
        );
    }
}
