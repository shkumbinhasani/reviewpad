//! Author identity for review comments.
//!
//! Nobody uploads anything. Every author gets a deterministic monogram — a
//! color and a letter derived from their name — so the same agent looks the
//! same in every repository with no assets to ship and no network to wait on.
//! Well-known agents get their real mark and brand color rather than a letter,
//! and the local user can have their Gravatar layered on top if they have one.
//!
//! The logos are Simple Icons' CC0 SVGs — see `assets/icons/README.md` for the
//! provenance and the trademark position.

use sha2::{Digest, Sha256};
use std::time::Duration;

/// How an author is drawn: a chip in their color carrying either their mark or,
/// failing that, their initial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// Asset path of the brand mark, for authors we recognize.
    pub icon: Option<&'static str>,
    /// Initial, drawn when there is no mark.
    pub label: String,
    /// Chip color — the brand's own, or a stable pick from the palette.
    pub color: u32,
}

impl Identity {
    /// Ink for whatever sits on the chip. Dark marks need a light chip and the
    /// reverse, so the contrast is decided from the color's luminance rather
    /// than fixed per brand.
    pub fn is_light(&self) -> bool {
        let (red, green, blue) = (
            (self.color >> 16) & 0xff,
            (self.color >> 8) & 0xff,
            self.color & 0xff,
        );
        // Rec. 601 luma, good enough to pick between black and white ink.
        (299 * red + 587 * green + 114 * blue) / 1000 > 140
    }
}

/// Agents worth recognizing on sight, matched loosely so `claude-code`,
/// `Claude` and `anthropic` all land on the same identity. Colors are the
/// official brand hexes Simple Icons publishes alongside each mark.
const KNOWN: &[(&[&str], &str, u32)] = &[
    (&["claude", "anthropic"], "icons/claude.svg", 0xd97757),
    (
        &["openai", "chatgpt", "gpt", "codex"],
        "icons/openai.svg",
        0x412991,
    ),
    (&["gemini", "bard"], "icons/gemini.svg", 0x8e75b2),
    (&["copilot"], "icons/copilot.svg", 0x24292f),
    (&["cursor"], "icons/cursor.svg", 0x1c1c1c),
];

/// Colors for everyone else, picked by name so an author keeps the same one.
const PALETTE: &[u32] = &[
    0x7abdff, 0x6bdb8f, 0xffc261, 0xff806b, 0xb794f6, 0x4fd1c5, 0xf687b3, 0x9ca3af,
];

pub fn identity(author: &str) -> Identity {
    let name = author.trim().to_lowercase();

    for (aliases, icon, color) in KNOWN {
        if aliases.iter().any(|alias| name.contains(alias)) {
            return Identity {
                icon: Some(icon),
                label: String::new(),
                color: *color,
            };
        }
    }

    let label = name
        .chars()
        .find(|character| character.is_alphanumeric())
        .map(|character| character.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_string());

    // Sum of bytes is a weak hash, but the only requirement is that a name
    // always lands on the same swatch.
    let index = name.bytes().map(usize::from).sum::<usize>() % PALETTE.len();
    Identity {
        icon: None,
        label,
        color: PALETTE[index],
    }
}

/// Gravatar's URL for an email address. `d=404` means "no fallback image": a
/// user without an account gets nothing, and the monogram stays.
pub fn gravatar_url(email: &str) -> String {
    let digest = Sha256::digest(email.trim().to_lowercase().as_bytes());
    let hash: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("https://gravatar.com/avatar/{hash}?s=96&d=404")
}

/// Fetch the local user's Gravatar, or `None` if they have none, the network is
/// away, or they opted out.
///
/// This is the one place ReviewPad sends anything derived from your identity
/// anywhere: a SHA-256 of your Git email, to gravatar.com. Set
/// `REVIEWPAD_NO_GRAVATAR` to skip it.
pub fn fetch_gravatar(email: &str) -> Option<Vec<u8>> {
    if std::env::var_os("REVIEWPAD_NO_GRAVATAR").is_some() {
        return None;
    }

    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .user_agent(concat!("reviewpad/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent();

    let mut response = agent.get(gravatar_url(email)).call().ok()?;
    if response.status() != 200 {
        return None;
    }
    response
        .body_mut()
        .with_config()
        .limit(2 * 1024 * 1024)
        .read_to_vec()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_agents_get_their_mark_and_brand_color() {
        assert_eq!(identity("claude").icon, Some("icons/claude.svg"));
        assert_eq!(identity("Claude Code").color, 0xd97757);
        assert_eq!(identity("anthropic").icon, Some("icons/claude.svg"));
        assert_eq!(identity("gpt-5-codex").icon, Some("icons/openai.svg"));
        assert_eq!(identity("github copilot").icon, Some("icons/copilot.svg"));
    }

    /// Dark marks need light ink and light marks need dark ink, or the chip is
    /// a solid block at 18px.
    #[test]
    fn ink_follows_the_chip_luminance() {
        assert!(identity("claude").is_light());
        assert!(!identity("openai").is_light());
        assert!(!identity("copilot").is_light());
    }

    #[test]
    fn unknown_authors_are_stable_and_initialled() {
        let first = identity("shkumbin");
        assert_eq!(first.icon, None);
        assert_eq!(first.label, "S");
        assert_eq!(first, identity("shkumbin"));
        assert!(PALETTE.contains(&first.color));
    }

    #[test]
    fn odd_names_still_render() {
        assert_eq!(identity("").label, "?");
        assert_eq!(identity("  ").label, "?");
        assert_eq!(identity("_ghost").label, "G");
        assert_eq!(identity("42").label, "4");
    }

    /// Gravatar hashes the trimmed, lowercased address — the same person typed
    /// two ways has to reach the same avatar.
    #[test]
    fn gravatar_normalizes_the_address() {
        assert_eq!(
            gravatar_url("  Person@Example.COM "),
            gravatar_url("person@example.com")
        );
        assert!(gravatar_url("person@example.com").contains("d=404"));
    }
}
