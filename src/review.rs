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
    /// The file itself, with nothing pointed at inside it. A note about a
    /// render as a whole, or about a file rather than a line.
    File,
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
            Anchor::File => "the file".to_string(),
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
            Anchor::Line { .. } | Anchor::File => None,
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
    /// Whether this note has been handed to whoever asked for the review.
    ///
    /// A note written in the panel starts as a draft: the person is still
    /// thinking, and an agent that acted on it now would be acting on half a
    /// thought. Submitting flips it. A note written *by* an agent arrives
    /// already delivered, so `place` marks those submitted outright.
    ///
    /// Defaulted, so a review saved before rounds existed loads as drafts and
    /// goes out with the next submission rather than vanishing.
    #[serde(default)]
    pub submitted: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Review {
    /// What the review was taken against — `working tree`, or a range like
    /// `main...HEAD`.
    ///
    /// Line numbers only mean something relative to a diff, so a reader that
    /// does not know the base cannot tell whether a note refers to uncommitted
    /// work or to a branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
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

/// Make the `.reviewpad` directory, and have it ignore itself so review state
/// stays out of `git status` — ReviewPad never dirties the working tree it is
/// inspecting. Everything written beside a review goes through here: the review
/// itself, the session a panel announces, a submitted round.
pub fn prepare_state_dir(directory: &Path) -> Result<()> {
    fs::create_dir_all(directory)
        .with_context(|| format!("failed to create {}", directory.display()))?;

    let ignore = directory.join(".gitignore");
    if !ignore.exists() {
        fs::write(&ignore, "*\n")
            .with_context(|| format!("failed to write {}", ignore.display()))?;
    }
    Ok(())
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
            prepare_state_dir(parent)?;
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
            submitted: false,
        });
        id
    }

    /// Ids of the notes still waiting to be sent.
    pub fn draft_ids(&self) -> Vec<String> {
        self.comments
            .iter()
            .filter(|comment| !comment.submitted)
            .map(|comment| comment.id.clone())
            .collect()
    }

    /// Hand these notes over, so the next round is only what came after them.
    pub fn mark_submitted(&mut self, ids: &[String]) {
        for comment in &mut self.comments {
            if ids.contains(&comment.id) {
                comment.submitted = true;
            }
        }
    }

    /// The same review narrowed to a few threads, for writing one round's brief
    /// without teaching `markdown` about rounds.
    pub fn round(&self, ids: &[String]) -> Self {
        Self {
            base: self.base.clone(),
            comments: self
                .comments
                .iter()
                .filter(|comment| ids.contains(&comment.id))
                .cloned()
                .collect(),
        }
    }

    /// How many replies the review holds, which is how the panel notices that
    /// somebody else has answered while it was open.
    pub fn reply_count(&self) -> usize {
        self.comments
            .iter()
            .map(|comment| comment.replies.len())
            .sum()
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
        let mut output = format!("# Code review\n\nRepository: `{}`\n", repository.display());
        // The base decides what the line numbers refer to, so it is stated
        // rather than assumed.
        if let Some(base) = &self.base {
            output.push_str(&format!("Reviewing: `{base}`\n"));
        }
        output.push('\n');

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
    fn a_note_is_a_draft_until_it_is_sent() {
        let mut review = review();
        assert_eq!(review.draft_ids(), vec!["c1".to_string()]);

        let drafts = review.draft_ids();
        review.mark_submitted(&drafts);
        assert!(review.draft_ids().is_empty());

        // What comes after a submission is the next round, not part of the one
        // already sent.
        review.add_comment("src/lib.rs", Anchor::File, "you", "And this.", "");
        assert_eq!(review.draft_ids(), vec!["c2".to_string()]);
    }

    /// A round is one submission's worth of notes, so implementing it twice is
    /// not something an agent can be asked to do by accident.
    #[test]
    fn a_round_carries_only_the_notes_it_sent() {
        let mut review = review();
        review.add_comment("src/other.rs", Anchor::File, "you", "Second note.", "");

        let round = review.round(&["c2".to_string()]);
        assert_eq!(round.comments.len(), 1);
        assert_eq!(round.comments[0].body, "Second note.");
        // The base travels with it: line numbers mean nothing without one.
        assert_eq!(round.base, review.base);
    }

    #[test]
    fn replies_are_counted_so_new_ones_can_be_noticed() {
        let mut review = review();
        assert_eq!(review.reply_count(), 0);
        review.add_reply("c1", "claude", "Renamed it.").unwrap();
        review.add_reply("c1", "claude", "And tested it.").unwrap();
        assert_eq!(review.reply_count(), 2);
    }

    /// A review saved before rounds existed has no `submitted` field at all.
    /// Loading it as drafts is what puts those notes in the next submission
    /// rather than dropping them on the floor.
    #[test]
    fn a_review_written_before_rounds_loads_as_drafts() {
        let earlier = r#"{"comments":[
            {"id":"c1","path":"a.rs","anchor":{"kind":"file"},"body":"one","context":"",
             "author":"you","replies":[]}
        ]}"#;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("comments.json");
        fs::write(&path, earlier).unwrap();

        let review = Review::load(&path).unwrap();
        assert!(!review.comments[0].submitted);
        assert_eq!(review.draft_ids(), vec!["c1".to_string()]);
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

    /// A note does not have to point at anything inside the file.
    #[test]
    fn a_file_anchor_carries_no_place() {
        let mut review = Review::default();
        review.add_comment(
            "out/promo.mp4",
            Anchor::File,
            "reviewer",
            "The whole thing feels rushed.",
            "",
        );
        assert_eq!(review.comments[0].anchor.spot(), None);
        assert_eq!(review.comments[0].anchor.seconds(), None);
        assert!(review.markdown(Path::new("/tmp")).contains("the file"));
    }

    #[test]
    fn a_moment_can_be_noted_without_a_place() {
        let anchor = Anchor::Time {
            seconds: OrderedF64(12.5),
            frame: Some(375),
            spot: None,
        };
        assert_eq!(anchor.spot(), None);
        assert_eq!(anchor.seconds(), Some(12.5));
        // The pin is an attachment; without one the moment still reads.
        assert_eq!(anchor.label(), "0:12.500 · frame 375");
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
