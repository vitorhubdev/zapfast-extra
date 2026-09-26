//! Daily update check against GitHub releases.

use std::time::Duration;

use anyhow::Result;

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

pub const VESPERA_VERSION: &str = match option_env!("VESPERA_RELEASE_VERSION") {
    Some(version) => version,
    None => include_str!("../VERSION"),
};
/// Canonical fork version without surrounding whitespace.
/// `VERSION` is the single source of truth for a local build. A release tag
/// sets `VESPERA_RELEASE_VERSION` so a candidate reports `X.Y.Z-rc.N`.
pub fn vespera_version() -> &'static str {
    VESPERA_VERSION.trim()
}

/// Commit compiled into a release binary, or `unknown` for a local build.
/// Release jobs set `VESPERA_GIT_SHA`. The string is kept live so the file
/// itself carries the commit it was built from.
pub fn vespera_commit() -> &'static str {
    option_env!("VESPERA_GIT_SHA").unwrap_or("unknown")
}

/// Version a tag must compile into the binary and the package.
/// `vX.Y.Z` must match `VERSION`. `vX.Y.Z-rc.N` keeps the candidate suffix
/// even though `VERSION` stays the stable triple.
pub fn version_for_tag(tag: &str, file_version: &str) -> Option<String> {
    let bare = tag.strip_prefix('v')?;
    let file_version = file_version.trim();
    if bare == file_version {
        return Some(bare.to_owned());
    }
    let (base, suffix) = bare.split_once("-rc.")?;
    if base == file_version
        && !suffix.is_empty()
        && suffix.bytes().all(|byte| byte.is_ascii_digit())
    {
        Some(bare.to_owned())
    } else {
        None
    }
}
const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/vitorhubdev/Vespera/releases/latest";

/// Update-check interval.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Update-check timeout: a stalled listing fails fast instead of pinning
/// a blocking worker thread forever.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// The version number, without a leading `v`.
    pub version: String,
    /// The release page, with every download.
    pub url: String,
}

/// The newest release, when it is newer than this build.
pub fn newer_release() -> Result<Option<Release>> {
    let endpoints = Source::GitHub.endpoints();
    match Checker::new().check(&endpoints, Channel::Stable, vespera_version()) {
        Some(CheckOutcome::Available(release)) => Ok(Some(release)),
        Some(CheckOutcome::UpToDate) => Ok(None),
        Some(CheckOutcome::Unavailable(error)) => Err(anyhow::anyhow!("{error}")),
        None => Ok(None),
    }
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

/// Which release stream the updater follows. Stable is the default and only
/// ever offers finished releases; Testing also offers release candidates
/// and never steps down to an older build.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Channel {
    #[default]
    Stable,
    Testing,
}
//
impl Channel {
    /// Short label for settings rows and logs.
    pub fn label(self) -> &'static str {
        match self {
            Channel::Stable => "Stable",
            Channel::Testing => "Testing",
        }
    }
}
//
/// Release-candidate marker of a parsed fork version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Stable,
    Rc(u64),
    Other,
}
//
/// A fork version split into its numeric triple and release kind.
/// Unknown suffixes map to Other and are never offered as updates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Parsed {
    numbers: [u64; 3],
    kind: Kind,
}
//
/// Full parse without a leading `v`: `1.0.61` and `1.0.61-rc.1` (also
/// `rc1` and `rc-1`, any case). A suffix that is not a candidate number
/// maps to Other rather than an error, so callers treat it as not-an-update.
fn parse_full(version: &str) -> Option<Parsed> {
    let version = version.trim();
    let (numbers, suffix) = match version.split_once('-') {
        Some((numbers, suffix)) => (numbers, Some(suffix)),
        None => (version, None),
    };
    let mut parts = numbers.split('.').map(|part| part.parse::<u64>().ok());
    let numbers = [parts.next()??, parts.next()??, parts.next()??];
    if parts.next().is_some() {
        return None;
    }
    let kind = match suffix {
        None => Kind::Stable,
        Some(raw) => match raw
            .to_ascii_lowercase()
            .strip_prefix("rc")
            .and_then(|rest| rest.trim_start_matches(['.', '-']).parse::<u64>().ok())
        {
            Some(number) => Kind::Rc(number),
            None => Kind::Other,
        },
    };
    Some(Parsed { numbers, kind })
}
//
/// Whether a version may be installed from this channel: finished releases
/// everywhere, candidates only while following Testing.
pub(crate) fn installable(channel: Channel, version: &str) -> bool {
    match parse_full(version) {
        Some(parsed) => match parsed.kind {
            Kind::Stable => true,
            Kind::Rc(_) => channel == Channel::Testing,
            Kind::Other => false,
        },
        None => false,
    }
}
//
/// Ordering key among newer candidates: higher triple wins, then finished
/// releases over candidates, then higher candidate numbers.
type Rank = ([u64; 3], u64, u64);
fn rank(parsed: &Parsed) -> Rank {
    let (stable, number) = match parsed.kind {
        Kind::Stable => (2, 0),
        Kind::Rc(number) => (1, number),
        Kind::Other => (0, 0),
    };
    (parsed.numbers, stable, number)
}
//
/// Channel-aware successor check. Never offers a downgrade, never offers
/// unknown suffixes, and a stable build never hears about pre-releases.
pub fn is_newer_in(channel: Channel, candidate: &str, current: &str) -> bool {
    let (cand, cur) = match (parse_full(candidate), parse_full(current)) {
        (Some(cand), Some(cur)) => (cand, cur),
        _ => return false,
    };
    if cand.numbers != cur.numbers {
        if cand.numbers < cur.numbers {
            return false;
        }
        return match channel {
            Channel::Stable => cand.kind == Kind::Stable,
            Channel::Testing => !matches!(cand.kind, Kind::Other),
        };
    }
    match (channel, cand.kind, cur.kind) {
        (_, Kind::Other, _) => false,
        (Channel::Stable, Kind::Stable, Kind::Stable) => false,
        (Channel::Stable, Kind::Stable, _) => true,
        (Channel::Stable, _, _) => false,
        (Channel::Testing, Kind::Stable, Kind::Stable) => false,
        (Channel::Testing, Kind::Stable, _) => true,
        (Channel::Testing, Kind::Rc(first), Kind::Rc(second)) => first > second,
        (Channel::Testing, Kind::Rc(_), _) => false,
    }
}
//
/// One release entry of the GitHub listing. The latest endpoint answers a
/// single object of this shape; the releases endpoint answers an array.
#[derive(Clone, Debug, serde::Deserialize)]
struct ListingEntry {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}
//
/// Newest entry the channel takes, or nothing newer. Drafts never count;
/// on Stable flagged pre-releases never count either (the tag check below
/// rejects them again, so a mislabeled entry cannot slip through).
fn select_release(channel: Channel, current: &str, entries: &[ListingEntry]) -> Option<Release> {
    let mut best: Option<(Rank, Release)> = None;
    for entry in entries {
        if entry.draft {
            continue;
        }
        if channel == Channel::Stable && entry.prerelease {
            continue;
        }
        let version = entry.tag_name.trim_start_matches('v').to_string();
        let parsed = match parse_full(&version) {
            Some(parsed) => parsed,
            None => continue,
        };
        if !is_newer_in(channel, &version, current) {
            continue;
        }
        let key = rank(&parsed);
        let better = best.as_ref().is_none_or(|(known, _)| key > *known);
        if better {
            best = Some((
                key,
                Release {
                    version,
                    url: entry.html_url.clone(),
                },
            ));
        }
    }
    best.map(|(_, release)| release)
}
//
/// Why a check produced no answer. User-facing strings stay free of
/// provider internals; details go to the debug log at the call site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchError {
    /// HTTP 429 or 403 from the listing endpoint.
    RateLimited,
    /// Timeouts, refused connections, DNS failures and broken bodies.
    Network,
    /// Anything else, with a short description.
    Unexpected(String),
}
//
impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::RateLimited => {
                write!(
                    f,
                    "GitHub refused the update check (usually its rate limit)"
                )
            }
            FetchError::Network => write!(f, "Could not reach GitHub to check for updates"),
            FetchError::Unexpected(why) => write!(f, "{why}"),
        }
    }
}
//
/// What one listing round produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckOutcome {
    Available(Release),
    UpToDate,
    Unavailable(FetchError),
}
//
/// Listing endpoints of a source: one latest object plus the releases array.
#[derive(Clone, Debug)]
pub struct Endpoints {
    pub latest: String,
    pub releases: String,
}
//
const RELEASES_URL: &str = "https://api.github.com/repos/vitorhubdev/Vespera/releases?per_page=10";
//
/// Largest listing body accepted; release pages are small JSON documents.
const LISTING_LIMIT: usize = 1024 * 1024;
//
impl Source {
    /// Listing endpoints of this source. The demo feed serves one object;
    /// asking it for the releases array fails gracefully as unexpected.
    pub fn endpoints(&self) -> Endpoints {
        match self {
            Source::GitHub => Endpoints {
                latest: LATEST_RELEASE_URL.into(),
                releases: RELEASES_URL.into(),
            },
            #[cfg(feature = "demo")]
            Source::Local(base) => Endpoints {
                latest: format!("{base}/latest.json"),
                releases: format!("{base}/latest.json"),
            },
        }
    }
}
//
/// Cached listing answer per URL: the ETag the server sent plus the
/// release it described, so a 304 replays the previous outcome.
#[derive(Clone, Debug, Default)]
struct CachedListing {
    etag: Option<String>,
    release: Option<Release>,
}
//
/// Update listings with a timeout, one flight at a time and an ETag cache.
/// Share across checks: the cache and the in-flight flag live inside.
#[derive(Clone)]
pub struct Checker {
    agent: ureq::Agent,
    state: std::sync::Arc<std::sync::Mutex<CheckerState>>,
}
//
#[derive(Debug, Default)]
struct CheckerState {
    listings: std::collections::HashMap<String, CachedListing>,
    busy: bool,
}
//
impl Checker {
    /// Production checks: twenty seconds for the whole listing round.
    pub fn new() -> Self {
        Self::with_timeout(CHECK_TIMEOUT)
    }
    //
    /// Shorter timeouts belong to tests; production uses [`Checker::new`].
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            agent: ureq::Agent::new_with_config(
                ureq::Agent::config_builder()
                    .timeout_global(Some(timeout))
                    .build(),
            ),
            state: std::sync::Arc::new(std::sync::Mutex::new(CheckerState::default())),
        }
    }
    //
    /// Runs one check, or nothing when another check is already flying:
    /// callers skip instead of stacking concurrent listing requests.
    pub fn check(
        &self,
        endpoints: &Endpoints,
        channel: Channel,
        current: &str,
    ) -> Option<CheckOutcome> {
        {
            let mut state = self.state.lock().ok()?;
            if std::mem::replace(&mut state.busy, true) {
                return None;
            }
        }
        let outcome = self.round(endpoints, channel, current);
        if let Ok(mut state) = self.state.lock() {
            state.busy = false;
        }
        Some(outcome)
    }
    //
    fn round(&self, endpoints: &Endpoints, channel: Channel, current: &str) -> CheckOutcome {
        let url = match channel {
            Channel::Stable => &endpoints.latest,
            Channel::Testing => &endpoints.releases,
        };
        let (etag, cached) = self
            .state
            .lock()
            .ok()
            .and_then(|state| state.listings.get(url).cloned())
            .map(|cached| (cached.etag, cached.release))
            .unwrap_or((None, None));
        let mut request = self
            .agent
            .get(url.as_str())
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", format!("Vespera/{current}"));
        if let Some(tag) = etag.as_deref() {
            request = request.header("If-None-Match", tag);
        }
        let mut response = match request.call() {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(429) | ureq::Error::StatusCode(403)) => {
                return CheckOutcome::Unavailable(FetchError::RateLimited);
            }
            Err(
                ureq::Error::Timeout(_)
                | ureq::Error::Io(_)
                | ureq::Error::HostNotFound
                | ureq::Error::ConnectionFailed,
            ) => return CheckOutcome::Unavailable(FetchError::Network),
            Err(_) => {
                return CheckOutcome::Unavailable(FetchError::Unexpected(
                    "Could not read the release listing".into(),
                ));
            }
        };
        if response.status().as_u16() == 304 {
            return match cached {
                Some(release) => CheckOutcome::Available(release),
                None => CheckOutcome::UpToDate,
            };
        }
        let fresh_tag = response
            .headers()
            .get("etag")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = match response.body_mut().read_to_string() {
            Ok(body) => body,
            Err(_) => return CheckOutcome::Unavailable(FetchError::Network),
        };
        if body.len() > LISTING_LIMIT {
            return CheckOutcome::Unavailable(FetchError::Unexpected(
                "The release listing is too large".into(),
            ));
        }
        let entries: Vec<ListingEntry> = match channel {
            Channel::Stable => match serde_json::from_str::<ListingEntry>(&body) {
                Ok(entry) => vec![entry],
                Err(_) => {
                    return CheckOutcome::Unavailable(FetchError::Unexpected(
                        "The release listing is not what was expected".into(),
                    ));
                }
            },
            Channel::Testing => match serde_json::from_str::<Vec<ListingEntry>>(&body) {
                Ok(entries) => entries,
                Err(_) => {
                    return CheckOutcome::Unavailable(FetchError::Unexpected(
                        "The release listing is not what was expected".into(),
                    ));
                }
            },
        };
        let release = select_release(channel, current, &entries);
        let outcome = match release.clone() {
            Some(release) => CheckOutcome::Available(release),
            None => CheckOutcome::UpToDate,
        };
        if let Ok(mut state) = self.state.lock() {
            state.listings.insert(
                url.clone(),
                CachedListing {
                    etag: fresh_tag,
                    release,
                },
            );
        }
        outcome
    }
}
//
impl Default for Checker {
    /// Default production checks.
    fn default() -> Self {
        Self::new()
    }
}
// end of channel listing support
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
    fn vespera_version_is_clean_and_comparable() {
        let raw = VESPERA_VERSION;
        let clean = vespera_version();
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

#[cfg(test)]
mod channel_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    #[test]
    fn a_candidate_tag_reports_its_suffix_and_stable_matches_the_file() {
        let file = "1.0.81\n";
        assert_eq!(version_for_tag("v1.0.81", file).as_deref(), Some("1.0.81"));
        assert_eq!(
            version_for_tag("v1.0.81-rc.2", file).as_deref(),
            Some("1.0.81-rc.2")
        );
        assert!(version_for_tag("v1.0.81-rc.2", "1.0.80").is_none());
        assert!(version_for_tag("v1.0.82", file).is_none());
        let testing = Channel::Testing;
        let stable = Channel::Stable;
        assert!(is_newer_in(testing, "1.0.81", "1.0.81-rc.2"));
        assert!(!is_newer_in(testing, "1.0.81-rc.2", "1.0.81-rc.2"));
        assert!(!is_newer_in(stable, "1.0.81-rc.2", "1.0.80"));
        assert!(installable(testing, "1.0.81-rc.2"));
        assert!(!installable(stable, "1.0.81-rc.2"));
    }

    #[test]
    fn stable_channel_matches_the_legacy_compare() {
        let pairs = [
            ("0.10.1", "0.10.0", true),
            ("0.11.0", "0.10.9", true),
            ("1.0.0", "0.99.9", true),
            ("0.10.10", "0.10.9", true),
            ("0.10.0", "0.10.0", false),
            ("0.9.9", "0.10.0", false),
            ("0.11.0-rc1", "0.10.0", false),
            ("nightly", "0.10.0", false),
            ("99.0.0.1", "0.10.0", false),
            ("0.11.0", "0.11.0-rc1", true),
            ("0.11.1", "0.11.0-rc1", true),
            ("0.11.0-rc1", "0.11.0", false),
            ("0.11.0-rc2", "0.11.0-rc1", false),
            ("0.10.0", "0.11.0-rc1", false),
        ];
        for (candidate, current, newer) in pairs {
            assert_eq!(
                is_newer(candidate, current),
                newer,
                "legacy {candidate} vs {current}"
            );
            assert_eq!(
                is_newer_in(Channel::Stable, candidate, current),
                newer,
                "stable {candidate} vs {current}"
            );
        }
    }
    #[test]
    fn testing_channel_orders_candidates_and_never_downgrades() {
        let stable = Channel::Stable;
        let testing = Channel::Testing;
        assert!(is_newer_in(testing, "1.0.62-rc.1", "1.0.61"));
        assert!(is_newer_in(testing, "1.0.62-rc.2", "1.0.62-rc.1"));
        assert!(is_newer_in(testing, "1.0.62", "1.0.62-rc.2"));
        assert!(!is_newer_in(testing, "1.0.62-rc.1", "1.0.62-rc.2"));
        assert!(!is_newer_in(testing, "1.0.62-rc.1", "1.0.62"));
        assert!(!is_newer_in(testing, "1.0.61", "1.0.62-rc.1"));
        assert!(!is_newer_in(testing, "1.0.61-rc.9", "1.0.61"));
        assert!(!is_newer_in(testing, "1.0.62-nightly", "1.0.61"));
        assert!(!is_newer_in(testing, "nightly", "1.0.61"));
        assert!(!is_newer_in(testing, "1.0.62-rc.1", "nightly"));
        assert!(is_newer_in(testing, "1.0.62-RC.2", "1.0.62-rc.1"));
        assert!(!is_newer_in(stable, "1.0.62-rc.2", "1.0.62-rc.1"));
        assert!(!is_newer_in(stable, "1.0.62-rc.1", "1.0.61"));
    }
    #[test]
    fn full_versions_parse_or_stay_out() {
        assert!(parse_full("1.0.61").is_some_and(|parsed| parsed.kind == Kind::Stable));
        assert!(parse_full("1.0.61-rc.1").is_some_and(|parsed| parsed.kind == Kind::Rc(1)));
        assert!(parse_full("1.0.61-rc1").is_some_and(|parsed| parsed.kind == Kind::Rc(1)));
        assert!(parse_full("1.0.61-rc-2").is_some_and(|parsed| parsed.kind == Kind::Rc(2)));
        assert!(parse_full(" 1.0.61-rc.1 ").is_some());
        assert!(parse_full("1.0.61-rc").is_some_and(|parsed| parsed.kind == Kind::Other));
        assert!(parse_full("1.0.61-nightly").is_some_and(|parsed| parsed.kind == Kind::Other));
        assert!(parse_full("v1.0.61").is_none());
        assert!(parse_full("1.0").is_none());
        assert!(parse_full("").is_none());
    }
    #[test]
    fn installable_matches_the_channel() {
        assert!(installable(Channel::Stable, "1.0.61"));
        assert!(!installable(Channel::Stable, "1.0.61-rc.1"));
        assert!(installable(Channel::Testing, "1.0.61"));
        assert!(installable(Channel::Testing, "1.0.61-rc.1"));
        assert!(!installable(Channel::Testing, "nightly"));
        assert!(!installable(Channel::Stable, "nightly"));
    }
    // part 1 end
    fn entry(tag: &str, draft: bool, prerelease: bool) -> ListingEntry {
        ListingEntry {
            tag_name: tag.into(),
            html_url: format!("https://example.com/{tag}"),
            draft,
            prerelease,
        }
    }
    #[test]
    fn listings_skip_drafts_and_pick_the_newest_take() {
        let entries = vec![
            entry("v9.9.9", true, false),
            entry("v1.0.60", false, false),
            entry("v1.0.62-rc.1", false, true),
            entry("v1.0.63", false, false),
            entry("v1.0.62", false, false),
            entry("nightly", false, false),
        ];
        let picked = select_release(Channel::Testing, "1.0.61", &entries).expect("picks");
        assert_eq!(picked.version, "1.0.63");
        let picked = select_release(Channel::Stable, "1.0.61", &entries).expect("picks");
        assert_eq!(picked.version, "1.0.63");
        let only_rc = vec![
            entry("v1.0.62-rc.2", false, true),
            entry("v1.0.62-rc.1", false, true),
        ];
        assert!(select_release(Channel::Stable, "1.0.61", &only_rc).is_none());
        let picked = select_release(Channel::Testing, "1.0.61", &only_rc).expect("picks");
        assert_eq!(picked.version, "1.0.62-rc.2");
        assert!(select_release(Channel::Testing, "1.0.63", &entries).is_none());
        let mislabeled = vec![entry("v1.0.62-rc.1", false, false)];
        assert!(select_release(Channel::Stable, "1.0.61", &mislabeled).is_none());
        let moved = select_release(Channel::Testing, "1.0.61", &mislabeled).expect("picks");
        assert_eq!(moved.version, "1.0.62-rc.1");
    }
    fn test_endpoints(base: &str) -> Endpoints {
        Endpoints {
            latest: format!("{base}/latest"),
            releases: format!("{base}/releases"),
        }
    }
    /// Serves one response per accepted connection and records request heads.
    fn serve(
        responses: Vec<(String, u16, String, Vec<u8>)>,
        delay: Duration,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        std::thread::JoinHandle<()>,
    ) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let recorded = seen.clone();
        let server = std::thread::spawn(move || {
            listener.set_nonblocking(true).expect("nonblocking");
            for (expected, status, headers, body) in responses {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "client did not connect"
                            );
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("fixture listener: {error}"),
                    }
                };
                stream.set_nonblocking(false).expect("blocking");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("timeout");
                if !delay.is_zero() {
                    std::thread::sleep(delay);
                }
                let mut request = [0; 4096];
                let size = stream.read(&mut request).expect("reads");
                let head = String::from_utf8_lossy(&request[..size]).into_owned();
                assert!(head.starts_with(&expected), "unexpected route: {head}");
                recorded.lock().expect("seen").push(head);
                let reason = match status {
                    200 => "OK",
                    304 => "Not Modified",
                    403 => "Forbidden",
                    429 => "Too Many Requests",
                    _ => "Error",
                };
                write!(stream, "HTTP/1.1 {status} {reason}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()).expect("head");
                stream.write_all(&body).expect("body");
            }
        });
        (base, seen, server)
    }
    // part 2 end
    #[test]
    fn etag_replays_the_cached_answer() {
        let body = br#"{"tag_name":"v9.9.9","html_url":"http://example.com/r"}"#.to_vec();
        let (base, seen, server) = serve(
            vec![
                ("GET /latest ".into(), 200, "ETag: \"abc\"\r\n".into(), body),
                ("GET /latest ".into(), 304, String::new(), Vec::new()),
            ],
            Duration::ZERO,
        );
        let checker = Checker::with_timeout(Duration::from_secs(5));
        let endpoints = test_endpoints(&base);
        let first = checker
            .check(&endpoints, Channel::Stable, "1.0.0")
            .expect("answers");
        assert_eq!(
            first,
            CheckOutcome::Available(Release {
                version: "9.9.9".into(),
                url: "http://example.com/r".into()
            })
        );
        let second = checker
            .check(&endpoints, Channel::Stable, "1.0.0")
            .expect("answers");
        assert_eq!(second, first);
        server.join().expect("serves");
        let seen = seen.lock().expect("seen");
        assert_eq!(seen.len(), 2);
        assert!(
            seen[1].contains("if-none-match: \"abc\""),
            "second round revalidates: {}",
            seen[1]
        );
    }
    #[test]
    fn testing_releases_array_prefers_the_newest_candidate() {
        let body = br#"[{"tag_name":"v9.9.9","html_url":"http://example.com/d","draft":true},{"tag_name":"v1.0.60","html_url":"http://example.com/o"},{"tag_name":"v1.0.62-rc.1","html_url":"http://example.com/a","prerelease":true},{"tag_name":"v1.0.62-rc.2","html_url":"http://example.com/b","prerelease":true},{"tag_name":"nightly","html_url":"http://example.com/n"},{"tag_name":"v1.0.61","html_url":"http://example.com/c"}]"#.to_vec();
        let (base, _, server) = serve(
            vec![("GET /releases ".into(), 200, String::new(), body)],
            Duration::ZERO,
        );
        let checker = Checker::with_timeout(Duration::from_secs(5));
        let outcome = checker
            .check(&test_endpoints(&base), Channel::Testing, "1.0.61")
            .expect("answers");
        assert_eq!(
            outcome,
            CheckOutcome::Available(Release {
                version: "1.0.62-rc.2".into(),
                url: "http://example.com/b".into()
            })
        );
        server.join().expect("serves");
    }
    #[test]
    fn rate_limits_and_garbage_map_to_messages() {
        for (status, body) in [(429u16, Vec::new()), (403, Vec::new())] {
            let (base, _, server) = serve(
                vec![("GET /latest ".into(), status, String::new(), body)],
                Duration::ZERO,
            );
            let outcome = Checker::with_timeout(Duration::from_secs(5))
                .check(&test_endpoints(&base), Channel::Stable, "1.0.0")
                .expect("answers");
            assert_eq!(outcome, CheckOutcome::Unavailable(FetchError::RateLimited));
            assert!(FetchError::RateLimited.to_string().contains("rate limit"));
            server.join().expect("serves");
        }
        let (base, _, server) = serve(
            vec![(
                "GET /latest ".into(),
                200,
                String::new(),
                b"not json".to_vec(),
            )],
            Duration::ZERO,
        );
        let outcome = Checker::with_timeout(Duration::from_secs(5))
            .check(&test_endpoints(&base), Channel::Stable, "1.0.0")
            .expect("answers");
        assert!(matches!(
            outcome,
            CheckOutcome::Unavailable(FetchError::Unexpected(_))
        ));
        server.join().expect("serves");
    }
    #[test]
    fn busy_check_skips_instead_of_stacking() {
        let body = br#"{"tag_name":"v9.9.9","html_url":"http://example.com/r"}"#.to_vec();
        let (base, _, server) = serve(
            vec![("GET /latest ".into(), 200, String::new(), body)],
            Duration::from_secs(2),
        );
        let checker = Checker::with_timeout(Duration::from_secs(10));
        let endpoints = test_endpoints(&base);
        let thread_endpoints = endpoints.clone();
        let flying = std::thread::spawn({
            let checker = checker.clone();
            move || checker.check(&thread_endpoints, Channel::Stable, "1.0.0")
        });
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            checker
                .check(&endpoints, Channel::Stable, "1.0.0")
                .is_none()
        );
        let landed = flying.join().expect("joins").expect("answers");
        assert!(matches!(landed, CheckOutcome::Available(_)));
        server.join().expect("serves");
    }
    #[test]
    fn stalled_listings_fail_fast() {
        // Bespoke quiet server: after the client times out and goes away,
        // reads fail, so nothing here may panic or the run fails elsewhere.
        let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("connects");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("timeout");
            std::thread::sleep(Duration::from_secs(2));
            let mut request = [0; 4096];
            let _ = stream.read(&mut request);
        });
        let outcome = Checker::with_timeout(Duration::from_millis(300))
            .check(&test_endpoints(&base), Channel::Stable, "1.0.0")
            .expect("answers");
        assert_eq!(outcome, CheckOutcome::Unavailable(FetchError::Network));
        server.join().expect("serves");
    }
}
// part 3 end
