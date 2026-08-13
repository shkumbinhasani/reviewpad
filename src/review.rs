use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

use crate::git::Repository;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Old,
    New,
}

impl Side {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Old => "old",
            Self::New => "new",
        }
    }
}

/// Who left a note. Free-form so an agent can sign with its own name.
pub const DEFAULT_AUTHOR: &str = "reviewer";

fn default_author() -> String {
    DEFAULT_AUTHOR.to_string()
}

/// A follow-up on a comment. Replies carry no anchor of their own — they belong
/// to the thread their parent opened.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Reply {
    pub id: String,
    #[serde(default = "default_author")]
    pub author: String,
    pub body: String,
}

/// A place on an image or a video frame, normalized to 0..1 so it survives the
/// image being displayed at any size.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Spot {
    pub x: f32,
    pub y: f32,
}

impl Spot {
    /// How a person reads it and an agent can act on it.
    pub fn label(&self) -> String {
        format!("{:.0}%,{:.0}%", self.x * 100., self.y * 100.)
    }
}

impl Eq for Spot {}

/// What a comment is attached to. Code review anchors to a line; reviewing a
/// render anchors to a moment or a place in it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Anchor {
    /// A line in a text diff.
    Line { side: Side, line: u32 },
    /// A moment in a video. `frame` is carried when the frame rate was
    /// readable, because a composition is written in frames, not seconds.
    Time {
        seconds: OrderedF64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        frame: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        spot: Option<Spot>,
    },
    /// A place on an image.
    Spot { spot: Spot },
}

/// `f64` that can sit inside an `Eq` type. Times are only ever compared for
/// identity here, never ordered by it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(transparent)]
pub struct OrderedF64(pub f64);

impl Eq for OrderedF64 {}

impl Anchor {
    /// The anchor written the way it appears in the review and the CLI.
    pub fn label(&self) -> String {
        match self {
            Anchor::Line { side, line } => format!("line {line} · {}", side.label()),
            Anchor::Time {
                seconds,
                frame,
                spot,
            } => {
                let mut label = crate::media::timecode(seconds.0);
                if let Some(frame) = frame {
                    label.push_str(&format!(" · frame {frame}"));
                }
                if let Some(spot) = spot {
                    label.push_str(&format!(" · {}", spot.label()));
                }
                label
            }
            Anchor::Spot { spot } => spot.label(),
        }
    }

    /// The line a text comment sits on, for the diff gutter.
    pub fn line(&self) -> Option<(Side, u32)> {
        match self {
            Anchor::Line { side, line } => Some((*side, *line)),
            _ => None,
        }
    }

    pub fn seconds(&self) -> Option<f64> {
        match self {
            Anchor::Time { seconds, .. } => Some(seconds.0),
            _ => None,
        }
    }

    pub fn spot(&self) -> Option<Spot> {
        match self {
            Anchor::Spot { spot } => Some(*spot),
            Anchor::Time { spot, .. } => *spot,
            Anchor::Line { .. } => None,
        }
    }
}

/// A thread root: one note anchored to a line, plus whatever it started.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewComment {
    // Defaulted so a review written before threading still loads; `Review::load`
    // backfills the missing ids.
    #[serde(default)]
    pub id: String,
    pub path: String,
    /// What the note points at — a diff line, a moment in a video, a place on
    /// an image. Reviews written before media support carried `side` and
    /// `line`; `Review::load` folds those into an anchor.
    pub anchor: Anchor,
    pub body: String,
    pub context: String,
    #[serde(default = "default_author")]
    pub author: String,
    #[serde(default)]
    pub replies: Vec<Reply>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Review {
    pub comments: Vec<ReviewComment>,
}

/// Fold the pre-media `side`/`line` pair into an anchor, in the raw JSON so the
/// struct itself carries no legacy fields.
fn migrate_line_anchors(value: &mut serde_json::Value) {
    let Some(comments) = value
        .get_mut("comments")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };

    for comment in comments {
        let Some(comment) = comment.as_object_mut() else {
            continue;
        };
        if comment.contains_key("anchor") {
            continue;
        }
        let (Some(side), Some(line)) = (comment.remove("side"), comment.remove("line")) else {
            continue;
        };
        comment.insert(
            "anchor".into(),
            serde_json::json!({ "kind": "line", "side": side, "line": line }),
        );
    }
}

/// The thread a comment or reply belongs to: `c3.2` lives in thread `c3`.
pub fn thread_of(id: &str) -> &str {
    id.split_once('.').map_or(id, |(root, _)| root)
}

impl Review {
    /// Load a repository's review, migrating it out of the old location under
    /// `.git/` on the way if that is where it still lives.
    pub fn open(repository: &Repository) -> Result<Self> {
        let path = repository.review_path();
        if path.exists() {
            return Self::load(&path);
        }

        let legacy = repository.legacy_review_path();
        if legacy.exists() {
            let review = Self::load(&legacy)?;
            review.save(&path)?;
            return Ok(review);
        }

        Ok(Self::default())
    }

    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes = fs::read(path)
            .with_context(|| format!("failed to read review at {}", path.display()))?;
        let mut value: serde_json::Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse review at {}", path.display()))?;
        migrate_line_anchors(&mut value);
        let mut review: Self = serde_json::from_value(value)
            .with_context(|| format!("failed to parse review at {}", path.display()))?;
        review.assign_missing_ids();
        Ok(review)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;

            // The directory ignores itself, so review state stays out of `git
            // status` in the repository being reviewed — ReviewPad never dirties
            // the working tree it is inspecting.
            let ignore = parent.join(".gitignore");
            if !ignore.exists() {
                fs::write(&ignore, "*\n")
                    .with_context(|| format!("failed to write {}", ignore.display()))?;
            }
        }
        let json = serde_json::to_vec_pretty(self)?;
        fs::write(path, json)
            .with_context(|| format!("failed to save review at {}", path.display()))
    }

    /// Open a thread wherever the anchor points. Returns the new comment's id.
    pub fn add_comment(
        &mut self,
        path: impl Into<String>,
        anchor: Anchor,
        author: impl Into<String>,
        body: impl Into<String>,
        context: impl Into<String>,
    ) -> String {
        let id = self.next_thread_id();
        self.comments.push(ReviewComment {
            id: id.clone(),
            path: path.into(),
            anchor,
            body: body.into(),
            context: context.into(),
            author: author.into(),
            replies: Vec::new(),
        });
        id
    }

    /// Reply into a thread. `target` may be the root or any reply in it, so a
    /// caller can answer the message it just read without walking back to the
    /// root itself.
    pub fn add_reply(
        &mut self,
        target: &str,
        author: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<String> {
        let thread = thread_of(target).to_string();
        let comment = self
            .comments
            .iter_mut()
            .find(|comment| comment.id == thread)
            .with_context(|| format!("no comment with id `{target}`"))?;

        if target != thread && !comment.replies.iter().any(|reply| reply.id == target) {
            bail!("no comment with id `{target}`");
        }

        // Number past the highest reply this thread has ever held, so ids stay
        // unique even after one is removed.
        let next = comment
            .replies
            .iter()
            .filter_map(|reply| reply.id.rsplit_once('.')?.1.parse::<u32>().ok())
            .max()
            .unwrap_or(0)
            + 1;
        let id = format!("{thread}.{next}");
        comment.replies.push(Reply {
            id: id.clone(),
            author: author.into(),
            body: body.into(),
        });
        Ok(id)
    }

    /// Remove a comment or a single reply, whichever the id names.
    pub fn remove(&mut self, id: &str) -> Result<()> {
        let thread = thread_of(id);
        let position = self
            .comments
            .iter()
            .position(|comment| comment.id == thread)
            .with_context(|| format!("no comment with id `{id}`"))?;

        if id == thread {
            self.comments.remove(position);
            return Ok(());
        }

        let replies = &mut self.comments[position].replies;
        let reply = replies
            .iter()
            .position(|reply| reply.id == id)
            .with_context(|| format!("no comment with id `{id}`"))?;
        replies.remove(reply);
        Ok(())
    }

    pub fn find(&self, id: &str) -> Option<&ReviewComment> {
        self.comments
            .iter()
            .find(|comment| comment.id == thread_of(id))
    }

    /// Total notes, counting replies — what the UI and the summary line report.
    pub fn len(&self) -> usize {
        self.comments
            .iter()
            .map(|comment| 1 + comment.replies.len())
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.comments.is_empty()
    }

    fn next_thread_id(&self) -> String {
        let highest = self
            .comments
            .iter()
            .filter_map(|comment| comment.id.strip_prefix('c')?.parse::<u32>().ok())
            .max()
            .unwrap_or(0);
        format!("c{}", highest + 1)
    }

    /// Give ids to anything loaded from a review written before threading.
    fn assign_missing_ids(&mut self) {
        let mut next = self
            .comments
            .iter()
            .filter_map(|comment| comment.id.strip_prefix('c')?.parse::<u32>().ok())
            .max()
            .unwrap_or(0);

        for index in 0..self.comments.len() {
            if self.comments[index].id.is_empty() {
                next += 1;
                self.comments[index].id = format!("c{next}");
            }
            let thread = self.comments[index].id.clone();
            for position in 0..self.comments[index].replies.len() {
                if self.comments[index].replies[position].id.is_empty() {
                    self.comments[index].replies[position].id =
                        format!("{thread}.{}", position + 1);
                }
            }
        }
    }

    pub fn markdown(&self, repository: &Path) -> String {
        let mut output = format!(
            "# Code review\n\nRepository: `{}`\n\n",
            repository.display()
        );

        if self.comments.is_empty() {
            output.push_str("No review comments.\n");
            return output;
        }

        output.push_str("Please address every item below. Preserve unrelated changes and run the relevant tests when finished.\n\n");
        for (index, comment) in self.comments.iter().enumerate() {
            output.push_str(&format!(
                "## {}. `{}` — {} — {}\n\n{}\n\n",
                index + 1,
                comment.path,
                comment.anchor.label(),
                comment.id,
                comment.body.trim()
            ));
            if !comment.context.trim().is_empty() {
                output.push_str("```diff\n");
                output.push_str(comment.context.trim_end());
                output.push_str("\n```\n\n");
            }
            for reply in &comment.replies {
                output.push_str(&format!(
                    "- **{}** ({}): {}\n",
                    reply.author,
                    reply.id,
                    reply.body.trim()
                ));
            }
            if !comment.replies.is_empty() {
                output.push('\n');
            }
        }
        output.push_str(
            "Reply to any item with `reviewpad reply <id> --body \"...\"` to continue its thread.\n",
        );
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review() -> Review {
        let mut review = Review::default();
        review.add_comment(
            "src/lib.rs",
            Anchor::Line {
                side: Side::New,
                line: 12,
            },
            "reviewer",
            "Handle the error instead of unwrapping.",
            "+let value = thing.unwrap();",
        );
        review
    }

    #[test]
    fn markdown_is_agent_ready() {
        let text = review().markdown(Path::new("/tmp/project"));
        assert!(text.contains("`src/lib.rs`"));
        assert!(text.contains("line 12 · new"));
        assert!(text.contains("Handle the error"));
        assert!(text.contains("```diff"));
        assert!(text.contains("c1"));
    }

    #[test]
    fn threads_number_from_their_root() {
        let mut review = review();
        assert_eq!(
            review.add_reply("c1", "agent", "Fixed in 4f2a1c.").unwrap(),
            "c1.1"
        );
        assert_eq!(
            review.add_reply("c1", "reviewer", "Thanks.").unwrap(),
            "c1.2"
        );
        // Replying to a reply continues the same thread rather than nesting.
        assert_eq!(review.add_reply("c1.2", "agent", "👍").unwrap(), "c1.3");
        assert_eq!(review.comments[0].replies.len(), 3);
    }

    #[test]
    fn reply_ids_survive_a_removal() {
        let mut review = review();
        review.add_reply("c1", "agent", "one").unwrap();
        review.add_reply("c1", "agent", "two").unwrap();
        review.remove("c1.1").unwrap();
        // The next id must not reuse c1.2, which is still taken.
        assert_eq!(review.add_reply("c1", "agent", "three").unwrap(), "c1.3");
    }

    #[test]
    fn unknown_ids_are_rejected() {
        let mut review = review();
        assert!(review.add_reply("c9", "agent", "nope").is_err());
        assert!(review.add_reply("c1.7", "agent", "nope").is_err());
        assert!(review.remove("c1.7").is_err());
    }

    #[test]
    fn removing_a_thread_takes_its_replies() {
        let mut review = review();
        review.add_reply("c1", "agent", "one").unwrap();
        review.remove("c1").unwrap();
        assert!(review.comments.is_empty());
    }

    #[test]
    fn threads_are_counted_with_their_replies() {
        let mut review = review();
        review.add_reply("c1", "agent", "one").unwrap();
        assert_eq!(review.len(), 2);
    }

    #[test]
    fn a_review_written_before_threading_still_loads() {
        let legacy = r#"{"comments":[
            {"path":"a.rs","side":"new","line":1,"body":"one","context":""},
            {"path":"b.rs","side":"old","line":2,"body":"two","context":""}
        ]}"#;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("comments.json");
        fs::write(&path, legacy).unwrap();

        let review = Review::load(&path).unwrap();
        assert_eq!(review.comments[0].id, "c1");
        assert_eq!(review.comments[1].id, "c2");
        assert_eq!(review.comments[0].author, DEFAULT_AUTHOR);
        assert!(review.comments[0].replies.is_empty());
    }

    /// Reviews written before media support anchored with a `side`/`line`
    /// pair. They have to keep working, and come back as line anchors.
    #[test]
    fn a_review_written_before_media_migrates_to_anchors() {
        let legacy = r#"{"comments":[
            {"id":"c1","path":"a.rs","side":"new","line":7,"body":"one","context":"",
             "author":"reviewer","replies":[]}
        ]}"#;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("comments.json");
        fs::write(&path, legacy).unwrap();

        let review = Review::load(&path).unwrap();
        assert_eq!(
            review.comments[0].anchor,
            Anchor::Line {
                side: Side::New,
                line: 7
            }
        );
        assert_eq!(review.comments[0].anchor.line(), Some((Side::New, 7)));
    }

    #[test]
    fn media_anchors_read_the_way_an_agent_needs_them() {
        let mut review = Review::default();
        review.add_comment(
            "out/video.mp4",
            Anchor::Time {
                seconds: OrderedF64(12.5),
                frame: Some(375),
                spot: Some(Spot { x: 0.42, y: 0.31 }),
            },
            "reviewer",
            "The logo lands late.",
            "",
        );
        review.add_comment(
            "design/hero.png",
            Anchor::Spot {
                spot: Spot { x: 0.5, y: 0.2 },
            },
            "reviewer",
            "Too much headroom.",
            "",
        );

        let text = review.markdown(Path::new("/tmp/project"));
        // The timecode a person reads and the frame a composition is written in.
        assert!(text.contains("0:12.500"));
        assert!(text.contains("frame 375"));
        assert!(text.contains("42%,31%"));
        assert!(text.contains("50%,20%"));
        assert!(text.contains("out/video.mp4"));
    }

    #[test]
    fn anchors_round_trip_through_json() {
        let mut review = Review::default();
        review.add_comment(
            "out/video.mp4",
            Anchor::Time {
                seconds: OrderedF64(3.25),
                frame: Some(97),
                spot: None,
            },
            "agent",
            "body",
            "",
        );
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("comments.json");
        review.save(&path).unwrap();

        let reloaded = Review::load(&path).unwrap();
        assert_eq!(reloaded.comments[0].anchor, review.comments[0].anchor);
        assert_eq!(reloaded.comments[0].anchor.seconds(), Some(3.25));
    }

    #[test]
    fn saving_keeps_the_directory_out_of_git() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(".reviewpad/comments.json");
        review().save(&path).unwrap();
        assert_eq!(
            fs::read_to_string(directory.path().join(".reviewpad/.gitignore")).unwrap(),
            "*\n"
        );
    }
}
