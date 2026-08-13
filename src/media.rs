//! Reviewing things that are not text.
//!
//! A rendered video or an exported image shows up in `git status` like any
//! other change, so it already reaches the sidebar. What it needs is a way to
//! point at a moment or a place in it, and a way to say that in terms an agent
//! can act on.
//!
//! Video decoding lives in [`crate::player`], which hands it to AVFoundation.
//! What remains here is the part that has nothing to do with pixels: deciding
//! what a file is, and naming a moment in a way a person reads and an agent can
//! act on.

use std::{path::Path, process::Command};

/// What kind of thing a changed file is, decided by extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Medium {
    Text,
    Image,
    Video,
}

const IMAGES: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "tiff", "tif", "avif",
];
const VIDEOS: &[&str] = &["mp4", "mov", "webm", "mkv", "avi", "m4v"];

impl Medium {
    pub fn of(path: &str) -> Self {
        let extension = path
            .rsplit('/')
            .next()
            .unwrap_or(path)
            .rsplit_once('.')
            .map(|(_, extension)| extension.to_ascii_lowercase())
            .unwrap_or_default();

        if IMAGES.contains(&extension.as_str()) {
            Medium::Image
        } else if VIDEOS.contains(&extension.as_str()) {
            Medium::Video
        } else {
            Medium::Text
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Medium::Text => "text",
            Medium::Image => "image",
            Medium::Video => "video",
        }
    }
}

/// What `ffprobe` can tell us about a video, which is all a scrubber needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Probe {
    pub duration: f64,
    pub fps: f64,
}

impl Probe {
    /// The frame a time lands on — the unit a Remotion composition is written
    /// in, so a review comment can name it directly.
    pub fn frame_at(&self, seconds: f64) -> u32 {
        (seconds * self.fps).round().max(0.) as u32
    }

    pub fn frames(&self) -> u32 {
        self.frame_at(self.duration)
    }
}

/// Read a video's duration and frame rate. `None` when ffprobe is missing or
/// the file is not a video it understands — the UI falls back to a plain
/// timeline and the comment simply carries no frame number.
pub fn probe(path: &Path) -> Option<Probe> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=r_frame_rate:format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=0",
        ])
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut duration = None;
    let mut fps = None;
    for line in text.lines() {
        match line.split_once('=') {
            Some(("duration", value)) => duration = value.trim().parse::<f64>().ok(),
            Some(("r_frame_rate", value)) => fps = parse_rate(value.trim()),
            _ => {}
        }
    }

    Some(Probe {
        duration: duration.filter(|seconds| *seconds > 0.)?,
        // A still or a stream with no declared rate still scrubs; 30 is only
        // used to name frames.
        fps: fps.filter(|rate| *rate > 0.).unwrap_or(30.),
    })
}

/// ffprobe reports frame rates as a rational, `30000/1001` for 29.97.
fn parse_rate(value: &str) -> Option<f64> {
    match value.split_once('/') {
        Some((numerator, denominator)) => {
            let numerator: f64 = numerator.parse().ok()?;
            let denominator: f64 = denominator.parse().ok()?;
            (denominator != 0.).then_some(numerator / denominator)
        }
        None => value.parse().ok(),
    }
}

/// `1:02.500`, the form a person reads and an agent can parse back.
pub fn timecode(seconds: f64) -> String {
    let seconds = seconds.max(0.);
    let minutes = (seconds / 60.).floor() as u64;
    let rest = seconds - (minutes as f64) * 60.;
    format!("{minutes}:{rest:06.3}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_is_recognized_by_extension() {
        assert_eq!(Medium::of("out/video.mp4"), Medium::Video);
        assert_eq!(Medium::of("design/Hero.PNG"), Medium::Image);
        assert_eq!(Medium::of("src/lib.rs"), Medium::Text);
        assert_eq!(Medium::of("Makefile"), Medium::Text);
        // An extension in a directory name is not the file's.
        assert_eq!(Medium::of("assets.png/readme"), Medium::Text);
    }

    #[test]
    fn frame_rates_parse_as_rationals() {
        assert_eq!(parse_rate("30/1"), Some(30.));
        assert_eq!(parse_rate("60"), Some(60.));
        assert_eq!(parse_rate("0/0"), None);
        // 29.97, the one that catches naive parsing.
        let ntsc = parse_rate("30000/1001").unwrap();
        assert!((ntsc - 29.97).abs() < 0.01);
    }

    #[test]
    fn times_map_to_frames() {
        let probe = Probe {
            duration: 10.,
            fps: 30.,
        };
        assert_eq!(probe.frame_at(0.), 0);
        assert_eq!(probe.frame_at(1.), 30);
        // The number a Remotion composition would name for this moment.
        assert_eq!(probe.frame_at(12.5), 375);
        assert_eq!(probe.frames(), 300);
    }

    #[test]
    fn timecodes_read_as_minutes_and_seconds() {
        assert_eq!(timecode(0.), "0:00.000");
        assert_eq!(timecode(12.5), "0:12.500");
        assert_eq!(timecode(62.25), "1:02.250");
        assert_eq!(timecode(-1.), "0:00.000");
    }
}
