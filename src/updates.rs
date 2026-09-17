//! Daily update check against GitHub releases.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

pub mod install;
#[cfg(target_os = "macos")]
mod macos;
mod transfer;
pub use transfer::{Source, download};

#[derive(Default)]
pub enum DownloadState {
    #[default]
    Idle,
    Downloading {
        received: u64,
        total: u64,
    },
    Ready(Box<install::Prepared>),
    Installing,
    Failed(String),
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub const ZAPEXT_VERSION: &str = include_str!("../VERSION");
/// Canonical fork version without surrounding whitespace.
/// `VERSION` is the single source of truth; trimming keeps a trailing newline
/// from breaking User-Agent headers, title strings, and version comparisons.
pub fn zapext_version() -> &'static str {
    ZAPEXT_VERSION.trim()
}
const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/vitorhubdev/zapfast-extra/releases/latest";

/// Update-check interval.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// The version number, without a leading `v`.
    pub version: String,
    /// The release page, with every download.
    pub url: String,
}

#[derive(Deserialize)]
struct LatestRelease {
    tag_name: String,
    html_url: String,
}

/// The newest release, when it is newer than this build.
pub fn newer_release() -> Result<Option<Release>> {
    let current = zapext_version();
    let mut response = ureq::get(LATEST_RELEASE_URL)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", format!("ZapExt/{current}"))
        .call()?;
    let body = response
        .body_mut()
        .read_to_string()
        .context("could not read the release listing")?;
    let latest: LatestRelease =
        serde_json::from_str(&body).context("unexpected release listing")?;
    let version = latest.tag_name.trim_start_matches('v').to_string();
    Ok(is_newer(&version, current).then_some(Release {
        version,
        url: latest.html_url,
    }))
}

/// `major.minor.patch`, and whether a suffix marks it as a pre-release;
/// anything else is `None`.
fn parse(version: &str) -> Option<([u64; 3], bool)> {
    let version = version.trim();
    let (numbers, pre_release) = match version.split_once('-') {
        Some((numbers, _)) => (numbers, true),
        None => (version, false),
    };
    let mut parts = numbers.split('.').map(|part| part.parse::<u64>().ok());
    let numbers = [parts.next()??, parts.next()??, parts.next()??];
    parts.next().is_none().then_some((numbers, pre_release))
}

/// Whether `candidate` is a newer stable version than `current`.
/// Stable releases supersede release candidates. Other pre-releases and
/// invalid versions are ignored.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse(candidate), parse(current)) {
        (Some((candidate, false)), Some((current, current_pre))) => {
            candidate > current || (candidate == current && current_pre)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("0.10.1", "0.10.0"));
        assert!(is_newer("0.11.0", "0.10.9"));
        assert!(is_newer("1.0.0", "0.99.9"));
        assert!(is_newer("0.10.10", "0.10.9"));
        assert!(!is_newer("0.10.0", "0.10.0"));
        assert!(!is_newer("0.9.9", "0.10.0"));
        assert!(
            !is_newer("0.11.0-rc1", "0.10.0"),
            "pre-releases are not announced"
        );
        assert!(!is_newer("nightly", "0.10.0"));
        assert!(!is_newer("99.0.0.1", "0.10.0"));
        // A release candidate hears about its release, and nothing older.
        assert!(is_newer("0.11.0", "0.11.0-rc1"));
        assert!(is_newer("0.11.1", "0.11.0-rc1"));
        assert!(!is_newer("0.11.0-rc1", "0.11.0"));
        assert!(!is_newer("0.11.0-rc2", "0.11.0-rc1"));
        assert!(!is_newer("0.10.0", "0.11.0-rc1"));
    }

    #[test]
    fn zapext_version_is_clean_and_comparable() {
        let raw = ZAPEXT_VERSION;
        let clean = zapext_version();
        assert_eq!(clean, raw.trim(), "VERSION must not carry whitespace");
        assert!(!clean.is_empty(), "VERSION must not be empty");
        assert!(
            parse(clean).is_some(),
            "VERSION must be major.minor.patch, got {clean:?}"
        );
        // Whitespace and `v` prefixes must not break update checks.
        assert!(is_newer("9.9.9", clean));
        assert!(!is_newer(clean, clean));
        assert!(!is_newer(clean, "9.9.9"));
        assert!(is_newer("1.0.5", "1.0.4"));
        assert!(!is_newer("1.0.4", "1.0.5"));
        assert!(parse(" 1.0.4 \n").is_some());
    }

    #[test]
    fn version_parsing_rejects_bad_input() {
        assert!(parse("").is_none());
        assert!(parse("1.0").is_none());
        assert!(parse("1.0.0.0").is_none());
        assert!(parse("v1.0.0").is_none());
        assert!(parse("1.0.x").is_none());
        assert!(parse("1.0.0-").is_some());
    }
}
