//! Where a comment lands.
//!
//! Deciding that is the same job whether the note arrives from the `comment`
//! subcommand or from an MCP client, and it is the part with all the rules in
//! it — the file has to be under review, a line number has to point at a text
//! file, a timestamp has to be inside the video. So it lives here rather than
//! in either caller.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::{
    git::{Base, Repository},
    media::{self, Medium},
    review::{Anchor, OrderedF64, Review, Side, Spot},
};

/// Everything a caller can say about where a comment goes, bundled so the
/// resolution below reads as one decision rather than nine parameters.
#[derive(Debug, Clone)]
pub struct Placement {
    pub repo: PathBuf,
    pub base: Option<String>,
    pub file: String,
    pub line: Option<u32>,
    pub time: Option<f64>,
    pub spot: Option<String>,
    pub side: Side,
    pub author: String,
    pub body: String,
}

/// What became of it.
#[derive(Debug, Clone)]
pub struct Placed {
    pub id: String,
    /// How the anchor reads, e.g. `line 11 · new` or `0:12.500 · f375`.
    pub label: String,
    /// Something the caller should pass on, such as a base that disagrees with
    /// the notes already saved.
    pub warning: Option<String>,
}

/// Anchor a comment to whatever the file is: a diff line, a moment in a video,
/// or a place on an image. Saves the review and returns the new comment's id.
pub fn place(placement: Placement) -> Result<Placed> {
    let Placement {
        repo,
        base,
        file,
        line,
        time,
        spot,
        side,
        author,
        body,
    } = placement;

    let repository = Repository::discover(&repo)?;
    let base = base.as_deref().map(Base::parse).unwrap_or_default();
    let diff = repository.diff_from(&base)?;

    // A path git does not show is normal for a render — `out/` is ignored in a
    // Remotion project — so a media file that exists on disk is reviewable. A
    // text file that is not in the diff is still almost always a typo.
    let changed = diff.files.iter().find(|changed| changed.path == file);
    if changed.is_none() {
        let on_disk = repository.root.join(&file).is_file();
        if !on_disk || Medium::of(&file) == Medium::Text {
            let changed = diff
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>();
            bail!(
                "`{file}` has no changes to review. Changed files: {}",
                if changed.is_empty() {
                    "none".to_string()
                } else {
                    changed.join(", ")
                }
            );
        }
    }

    let spot = spot.map(|spot| parse_spot(&spot)).transpose()?;
    let medium = Medium::of(&file);
    let (anchor, context) = match (line, time, spot) {
        (Some(line), None, None) => {
            if medium != Medium::Text {
                bail!(
                    "`{file}` is {}, so a line number does not point at anything — \
                     use a time or a spot",
                    medium.label()
                );
            }
            let context = changed
                .and_then(|changed| changed.index_of(side, line).map(|i| (changed, i)))
                .map(|(changed, index)| changed.context_at(index))
                .unwrap_or_default();
            (Anchor::Line { side, line }, context)
        }
        (None, Some(seconds), spot) => {
            if seconds < 0. {
                bail!("a time cannot be negative");
            }
            // The frame is what a composition is written in, so carry it when
            // the file's frame rate is readable.
            let probe = media::probe(&repository.root.join(&file));
            if let Some(probe) = probe
                && seconds > probe.duration
            {
                bail!(
                    "time {seconds} is past the end of `{file}` ({})",
                    media::timecode(probe.duration)
                );
            }
            (
                Anchor::Time {
                    seconds: OrderedF64(seconds),
                    frame: probe.map(|probe| probe.frame_at(seconds)),
                    spot,
                },
                String::new(),
            )
        }
        (None, None, Some(spot)) => (Anchor::Spot { spot }, String::new()),
        // Nothing pointed at: a note about the file itself.
        (None, None, None) => (Anchor::File, String::new()),
        _ => bail!("a time, a spot and a line number are alternatives, not a combination"),
    };

    let mut review = Review::open(&repository)?;
    // Line numbers only mean something against a base, so the review records
    // which one it was taken from — and says so when a note is about to join
    // comments that were placed against a different diff entirely.
    let base_label = base.label();
    let warning = review
        .base
        .as_deref()
        .filter(|existing| *existing != base_label && !review.comments.is_empty())
        .map(|existing| {
            format!(
                "this review already holds notes taken against {existing}; \
                 their line numbers do not refer to {base_label}"
            )
        });
    review.base = Some(base_label);

    let label = anchor.label();
    let id = review.add_comment(&file, anchor, &author, body, context);
    review.save(&repository.review_path())?;

    Ok(Placed { id, label, warning })
}

/// `0.42,0.31` — a place on an image, normalized so it survives any display
/// size. Percentages are accepted too, since that is how the export reads.
pub fn parse_spot(text: &str) -> Result<Spot> {
    let (x, y) = text
        .split_once(',')
        .context("a spot is `x,y`, for example 0.42,0.31")?;

    let axis = |value: &str| -> Result<f32> {
        let value = value.trim();
        let parsed: f32 = match value.strip_suffix('%') {
            Some(percent) => percent.trim().parse::<f32>().map(|value| value / 100.),
            None => value.parse::<f32>(),
        }
        .with_context(|| format!("`{value}` is not a number"))?;
        if !(0. ..=1.).contains(&parsed) {
            bail!("`{value}` is outside the image — a spot is 0..1, or a percentage");
        }
        Ok(parsed)
    };

    Ok(Spot {
        x: axis(x)?,
        y: axis(y)?,
    })
}
