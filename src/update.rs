//! Self-update against the GitHub release that CI publishes.
//!
//! Every release carries a `latest.json` manifest, and GitHub resolves
//! `releases/latest/download/<asset>` to the newest release, so a single
//! unauthenticated fetch answers "is there something newer?" without spending
//! an API rate-limit token.
//!
//! Homebrew installs are deliberately left alone: rewriting a file Homebrew
//! tracks would desynchronize its manifest, so those are told to `brew upgrade`
//! instead.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{env, fs, path::Path, process::Command, time::Duration};

const MANIFEST_URL: &str =
    "https://github.com/shkumbinhasani/reviewpad/releases/latest/download/latest.json";
const TIMEOUT: Duration = Duration::from_secs(20);

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub tag: String,
    pub notes: String,
    pub cli: Asset,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub url: String,
    pub sha256: String,
}

/// How this copy of ReviewPad got onto the machine, which decides whether it
/// may replace itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Install {
    Homebrew,
    Standalone,
}

/// Where Homebrew records that it owns this cask, under either prefix.
const CASK_RECEIPTS: [&str; 2] = [
    "/opt/homebrew/Caskroom/reviewpad",
    "/usr/local/Caskroom/reviewpad",
];

/// The shims a cask's `binary` stanza links into, under either prefix.
const BREW_SHIMS: [&str; 2] = ["/opt/homebrew/bin/reviewpad", "/usr/local/bin/reviewpad"];

impl Install {
    pub fn detect() -> Self {
        let Ok(path) = env::current_exe() else {
            return Install::Standalone;
        };
        let has_receipt = CASK_RECEIPTS
            .iter()
            .any(|receipt| Path::new(receipt).exists());

        // `current_exe` reports the shim when ReviewPad is invoked through the
        // symlink on PATH and the bundle when it is invoked directly, so both
        // spellings have to be classified.
        if Self::classify(&path, has_receipt) == Install::Homebrew {
            return Install::Homebrew;
        }
        let resolved = fs::canonicalize(&path).unwrap_or(path);
        Self::classify(&resolved, has_receipt)
    }

    /// A formula keeps its binary under `Cellar`, but a cask *moves* the bundle
    /// into `/Applications` and links the executable out of it, so the running
    /// path carries no Homebrew marker at all. The Caskroom receipt is what
    /// proves ownership there; without it, `reviewpad update` would overwrite a
    /// bundle Homebrew tracks — which desynchronizes its manifest and leaves
    /// `brew upgrade` unable to complete.
    fn classify(path: &Path, has_cask_receipt: bool) -> Self {
        let text = path.to_string_lossy();
        if text.contains("/Caskroom/") || text.contains("/Cellar/") {
            return Install::Homebrew;
        }
        if has_cask_receipt
            && (text.contains("/ReviewPad.app/") || BREW_SHIMS.contains(&text.as_ref()))
        {
            return Install::Homebrew;
        }
        Install::Standalone
    }

    pub fn upgrade_hint(self) -> &'static str {
        match self {
            Install::Homebrew => "brew upgrade --cask reviewpad",
            Install::Standalone => "reviewpad update",
        }
    }
}

/// Fetch the manifest for the newest release. `None` means the check failed —
/// an offline machine is not an error worth interrupting a review for.
pub fn latest() -> Option<Manifest> {
    fetch_manifest().ok()
}

fn fetch_manifest() -> Result<Manifest> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .user_agent(concat!("reviewpad/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent();

    let manifest = agent
        .get(MANIFEST_URL)
        .call()
        .context("could not reach the release feed")?
        .body_mut()
        .read_json::<Manifest>()
        .context("the release feed was not the manifest we expected")?;

    Ok(manifest)
}

/// Whether `candidate` is a newer release than what is running, comparing
/// dotted numeric components so `0.10.0` sorts above `0.9.0`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    /// The numbers, and whether a pre-release suffix followed them.
    fn parts(version: &str) -> (Vec<u64>, bool) {
        let version = version.trim_start_matches('v');
        let (numbers, prerelease) = match version.split_once('-') {
            Some((numbers, _)) => (numbers, true),
            None => (version, false),
        };
        let numbers = numbers
            .split('.')
            .map(|part| part.parse().unwrap_or(0))
            .collect();
        (numbers, prerelease)
    }

    let ((candidate, candidate_pre), (current, current_pre)) = (parts(candidate), parts(current));
    let width = candidate.len().max(current.len());
    for index in 0..width {
        let (new, old) = (
            candidate.get(index).copied().unwrap_or(0),
            current.get(index).copied().unwrap_or(0),
        );
        if new != old {
            return new > old;
        }
    }

    // Same numbers: a final release supersedes its own pre-releases, which is
    // what gets somebody testing `0.9.0-rc.1` onto `0.9.0` when it ships. The
    // reverse is never an upgrade — the release feed does not offer
    // pre-releases, and being moved onto one would be a downgrade anyway.
    current_pre && !candidate_pre
}

/// Print whether a newer release exists, without touching anything on disk.
pub fn check() -> Result<()> {
    let Some(manifest) = latest() else {
        bail!("could not reach the release feed");
    };

    if is_newer(&manifest.version, VERSION) {
        println!(
            "ReviewPad {} is available (running {VERSION})",
            manifest.version
        );
        println!("{}", manifest.notes);
        println!("Install it with: {}", Install::detect().upgrade_hint());
    } else {
        println!("ReviewPad {VERSION} is up to date");
    }
    Ok(())
}

/// Download the newest release and swap it in over the running executable.
pub fn install() -> Result<()> {
    let install = Install::detect();
    if install == Install::Homebrew {
        bail!(
            "this copy is managed by Homebrew — run `{}` instead",
            install.upgrade_hint()
        );
    }

    let manifest = fetch_manifest()?;
    if !is_newer(&manifest.version, VERSION) {
        println!("ReviewPad {VERSION} is already up to date");
        return Ok(());
    }

    let current = env::current_exe().context("could not locate the running binary")?;
    let directory = current
        .parent()
        .context("the running binary has no parent directory")?;

    println!("Downloading ReviewPad {}…", manifest.version);
    let archive = download(&manifest.cli.url)?;
    verify(&archive, &manifest.cli.sha256)?;

    // Stage everything beside the target so the final swap is a rename on the
    // same filesystem, which cannot leave a half-written binary behind.
    let staging = directory.join(format!(".reviewpad-update-{}", std::process::id()));
    fs::create_dir_all(&staging).with_context(|| {
        format!(
            "could not write to {} — reinstall with `brew upgrade --cask reviewpad` \
             or re-run with write access",
            directory.display()
        )
    })?;
    let result = swap(&archive, &staging, &current);
    let _ = fs::remove_dir_all(&staging);
    result?;

    println!("Updated to ReviewPad {}", manifest.version);
    println!("{}", manifest.notes);
    Ok(())
}

fn download(url: &str) -> Result<Vec<u8>> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(300)))
        .user_agent(concat!("reviewpad/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent();

    let mut response = agent
        .get(url)
        .call()
        .with_context(|| format!("could not download {url}"))?;

    let body = response
        .body_mut()
        .with_config()
        // Releases are a few tens of megabytes; the cap is a sanity bound, not
        // a tuning knob.
        .limit(256 * 1024 * 1024)
        .read_to_vec()
        .context("the download was interrupted")?;

    Ok(body)
}

fn verify(bytes: &[u8], expected: &str) -> Result<()> {
    let actual = hex(&Sha256::digest(bytes));
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("checksum mismatch: expected {expected}, got {actual}");
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Unpack the archive and move the new binary into place, keeping the old one
/// until the rename succeeds.
fn swap(archive: &[u8], staging: &Path, current: &Path) -> Result<()> {
    let tarball = staging.join("reviewpad.tar.gz");
    fs::write(&tarball, archive).context("could not stage the download")?;

    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(staging)
        .status()
        .context("could not run tar")?;
    if !status.success() {
        bail!("could not unpack the release archive");
    }

    let replacement = staging.join("reviewpad");
    if !replacement.is_file() {
        bail!("the release archive did not contain a reviewpad binary");
    }
    fs::set_permissions(&replacement, permissions(0o755))?;

    // A running executable cannot be overwritten in place, but it can be
    // renamed out of the way — open file handles follow the inode.
    let retired = staging.join("reviewpad.old");
    fs::rename(current, &retired).context("could not move the current binary aside")?;
    if let Err(error) = fs::rename(&replacement, current) {
        // Put the old binary back rather than leaving nothing installed.
        let _ = fs::rename(&retired, current);
        return Err(error).context("could not install the new binary");
    }

    Ok(())
}

#[cfg(unix)]
fn permissions(mode: u32) -> fs::Permissions {
    use std::os::unix::fs::PermissionsExt;
    fs::Permissions::from_mode(mode)
}

#[cfg(not(unix))]
fn permissions(_mode: u32) -> fs::Permissions {
    unreachable!("ReviewPad only ships on macOS and Linux")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_versions_compare_numerically() {
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn tags_and_prereleases_are_normalized() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(!is_newer("0.1.0-rc.1", "0.1.0"));
        assert!(is_newer("0.2.0-rc.1", "0.1.0"));
    }

    /// Somebody testing a beta has to be told when the real thing ships, or the
    /// build they were kind enough to try becomes the build they are stuck on.
    #[test]
    fn a_release_supersedes_its_own_prerelease() {
        assert!(is_newer("0.9.0", "0.9.0-rc.1"));
        assert!(is_newer("0.9.0", "0.9.0-rc.2"));
        // Not the other way, and a pre-release is not newer than itself.
        assert!(!is_newer("0.9.0-rc.1", "0.9.0"));
        assert!(!is_newer("0.9.0-rc.1", "0.9.0-rc.1"));
        // Numbers still decide first.
        assert!(is_newer("0.9.1-rc.1", "0.9.0"));
    }

    #[test]
    fn shorter_versions_pad_with_zeros() {
        assert!(is_newer("0.2", "0.1.9"));
        assert!(!is_newer("0.1", "0.1.0"));
    }

    #[test]
    fn homebrew_paths_are_recognized() {
        assert_eq!(
            Install::classify(
                Path::new("/opt/homebrew/Caskroom/reviewpad/0.1.0/reviewpad"),
                true
            ),
            Install::Homebrew
        );
        assert_eq!(
            Install::classify(
                Path::new("/usr/local/Cellar/reviewpad/0.1.0/bin/reviewpad"),
                false
            ),
            Install::Homebrew
        );
        assert_eq!(
            Install::classify(Path::new("/Users/me/.cargo/bin/reviewpad"), false),
            Install::Standalone
        );
    }

    /// The case the first cut got wrong: a cask links its binary out of the
    /// bundle in /Applications, so the resolved path looks like a manual
    /// install until you notice the Caskroom receipt.
    #[test]
    fn a_cask_linked_bundle_is_homebrew_owned() {
        let linked = Path::new("/Applications/ReviewPad.app/Contents/MacOS/reviewpad");
        assert_eq!(Install::classify(linked, true), Install::Homebrew);
        // The same bundle dragged in by hand is the user's to replace.
        assert_eq!(Install::classify(linked, false), Install::Standalone);
    }

    /// Run from PATH, `current_exe` reports the shim rather than the bundle it
    /// points at, so the shim itself has to be recognized.
    #[test]
    fn the_brew_shim_is_homebrew_owned() {
        let shim = Path::new("/opt/homebrew/bin/reviewpad");
        assert_eq!(Install::classify(shim, true), Install::Homebrew);
        assert_eq!(Install::classify(shim, false), Install::Standalone);
        // A neighbouring binary is not the shim.
        assert_eq!(
            Install::classify(Path::new("/opt/homebrew/bin/reviewpad-dev"), true),
            Install::Standalone
        );
    }
}
