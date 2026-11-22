use super::policy::{GithubReleaseTrack, GithubReleaseVerification, ToolLayout};
use super::*;
use crate::command::http::build_client as build_shared_http_client;
#[cfg(test)]
use crate::command::http::{
    proxy_env_keys_for_scheme as shared_proxy_env_keys_for_scheme,
    split_no_proxy_rules as shared_split_no_proxy_rules,
};
use flate2::read::GzDecoder;
use semver::Version;
use serde::de::DeserializeOwned;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
};
use tar::Archive;
use xz2::read::XzDecoder;
use zip::ZipArchive;

const TEMP_DIR_PREFIX_DOWNLOAD: &str = "za-tool-download";
const TEMP_DIR_PREFIXES: [&str; 1] = [TEMP_DIR_PREFIX_DOWNLOAD];
const DOWNLOAD_READ_CHUNK_SIZE: usize = 64 * 1024;
const PARALLEL_DOWNLOAD_MIN_BYTES: u64 = 2 * 1024 * 1024;
const PARALLEL_DOWNLOAD_MIN_PART_BYTES: u64 = 1024 * 1024;
const PARALLEL_DOWNLOAD_MAX_PARTS: usize = 4;
// Each range request has its own timeout. Bound the bytes covered by that deadline.
const RANGE_REQUEST_BYTES: u64 = 8 * 1024 * 1024;
const SINGLE_STREAM_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const HTTP_TRANSIENT_RETRY_MAX_ATTEMPTS: usize = 3;
const HTTP_TRANSIENT_RETRY_BASE_DELAY_MS: u64 = 200;
const GITHUB_RELEASE_SCAN_PER_PAGE: usize = 30;
const GITHUB_RELEASE_SCAN_MAX_PAGES: usize = 10;
// Release lists include every asset and release note, even when only tags are needed.
// Asset-heavy projects can exceed reqx's default 8 MiB response limit.
const GITHUB_RELEASE_METADATA_MAX_BYTES: usize = 64 * 1024 * 1024;

static ACTIVE_TEMP_DIRS: LazyLock<Mutex<HashSet<PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));
static TEMP_DIR_NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub(super) struct DownloadProgressReporter {
    tool: String,
    sink: DownloadProgressSink,
}

impl DownloadProgressReporter {
    pub(super) fn new(tool: impl Into<String>, sink: DownloadProgressSink) -> Self {
        Self {
            tool: tool.into(),
            sink,
        }
    }
}

pub(super) type DownloadProgressSink = Arc<dyn Fn(DownloadProgress) + Send + Sync + 'static>;

#[derive(Debug, Clone)]
pub(super) struct DownloadProgress {
    pub(super) tool: String,
    pub(super) downloaded: u64,
    pub(super) total_bytes: Option<u64>,
    pub(super) elapsed: Duration,
    pub(super) finished: bool,
}

#[derive(Clone, Copy)]
struct DownloadFeedback<'a> {
    display: DownloadDisplay,
    reporter: Option<&'a DownloadProgressReporter>,
}

impl<'a> DownloadFeedback<'a> {
    fn new(display: DownloadDisplay, reporter: Option<&'a DownloadProgressReporter>) -> Self {
        Self { display, reporter }
    }

    fn is_detailed(self) -> bool {
        self.display.is_detailed()
    }

    fn shows_progress(self) -> bool {
        self.display.shows_progress()
    }

    fn tracks_progress(self) -> bool {
        self.shows_progress() || self.reporter.is_some()
    }

    fn progress_bar(self, total_bytes: Option<u64>) -> Option<ProgressBar> {
        if !self.shows_progress() || !io::stderr().is_terminal() {
            return None;
        }
        new_download_progress_bar(ProgressDrawTarget::stderr(), total_bytes)
    }

    fn reporter(self) -> Option<&'a DownloadProgressReporter> {
        self.reporter
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DownloadRange {
    start: u64,
    end: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParallelDownloadPlan {
    total_bytes: u64,
    parts: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DownloadProbe {
    total_bytes: u64,
    range_supported: bool,
}

struct SingleStreamDownload<'a> {
    client: &'a Client,
    path_and_query: &'a str,
    url: &'a str,
    asset_path: &'a Path,
    total_bytes: Option<u64>,
}

enum ParallelDownloadError {
    Unsupported,
    Failed(anyhow::Error),
}

enum DownloadExtractionMode<'a> {
    KeepArchive,
    PrimaryEntry(&'a ToolRef),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DownloadDisplay {
    Detailed,
    Compact,
    Quiet,
}

impl DownloadDisplay {
    fn is_detailed(self) -> bool {
        matches!(self, Self::Detailed)
    }

    fn shows_progress(self) -> bool {
        !matches!(self, Self::Quiet)
    }
}

pub(super) fn unregister_temp_dir(path: &Path) {
    if let Ok(mut dirs) = ACTIVE_TEMP_DIRS.lock() {
        dirs.remove(path);
    }
}

pub(super) fn cleanup_stale_temp_dirs() -> usize {
    let base = env::temp_dir();
    let Ok(entries) = fs::read_dir(&base) else {
        return 0;
    };

    let active = ACTIVE_TEMP_DIRS
        .lock()
        .map(|dirs| dirs.clone())
        .unwrap_or_default();
    let mut removed = 0usize;
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let path = entry.path();
        if active.contains(&path) {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(prefix) = matched_temp_prefix(&name) else {
            continue;
        };
        let Some(pid) = parse_temp_dir_pid(&name, prefix) else {
            continue;
        };
        if pid == std::process::id() || process_is_alive(pid) {
            continue;
        }
        if fs::remove_dir_all(&path).is_ok() {
            removed = removed.saturating_add(1);
        }
    }
    removed
}

fn emit_download_progress(
    reporter: Option<&DownloadProgressReporter>,
    downloaded: u64,
    total_bytes: Option<u64>,
    elapsed: Duration,
    finished: bool,
) {
    if let Some(reporter) = reporter {
        (reporter.sink)(DownloadProgress {
            tool: reporter.tool.clone(),
            downloaded,
            total_bytes,
            elapsed,
            finished,
        });
    }
}

fn matched_temp_prefix(name: &str) -> Option<&'static str> {
    TEMP_DIR_PREFIXES
        .iter()
        .copied()
        .find(|prefix| name.starts_with(prefix))
}

fn parse_temp_dir_pid(name: &str, prefix: &str) -> Option<u32> {
    let rest = name.strip_prefix(prefix)?.strip_prefix('-')?;
    let parts = rest.split('-').collect::<Vec<_>>();
    let pid = match parts.as_slice() {
        [nonce, pid] if nonce.parse::<u64>().is_ok() => pid,
        [nonce, millis, pid] if nonce.parse::<u64>().is_ok() && millis.parse::<u128>().is_ok() => {
            pid
        }
        _ => return None,
    };
    pid.parse::<u32>().ok()
}

#[cfg(target_os = "linux")]
fn process_is_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(not(target_os = "linux"))]
fn process_is_alive(_pid: u32) -> bool {
    // Be conservative where PID liveness cannot be checked without platform APIs.
    true
}

fn register_temp_dir(path: &Path) {
    if let Ok(mut dirs) = ACTIVE_TEMP_DIRS.lock() {
        dirs.insert(path.to_path_buf());
    }
}

pub(super) fn resolve_requested_version(
    name: &str,
    requested_version: Option<&str>,
    proxy_scope: za_config::ProxyScope,
) -> Result<String> {
    ensure_not_interrupted()?;

    if let Some(v) = requested_version {
        let v = normalize_version(v);
        if v.is_empty() {
            bail!("version must not be empty");
        }
        return Ok(v);
    }

    let Some(policy) = find_tool_policy(name) else {
        bail!("{}", unsupported_tool_message(name));
    };
    let Some(release) = policy.github_release else {
        bail!("latest version resolution is not defined for `{name}`");
    };
    fetch_latest_version_from_github_release(policy, release, proxy_scope)
}

pub(super) fn resolve_install_source(
    tool: &ToolRef,
    proxy_scope: za_config::ProxyScope,
    display: DownloadDisplay,
    progress_sink: Option<DownloadProgressSink>,
) -> Result<PullSource> {
    ensure_not_interrupted()?;

    let Some(policy) = find_tool_policy(&tool.name) else {
        bail!("{}", unsupported_tool_message(&tool.name));
    };

    let progress_reporter =
        progress_sink.map(|sink| DownloadProgressReporter::new(tool.name.clone(), sink));

    let Some(release) = policy.github_release else {
        bail!(
            "GitHub Release source is not configured for `{}`",
            tool.name
        );
    };

    download_from_github_release(
        tool,
        policy.layout,
        release,
        proxy_scope,
        display,
        progress_reporter.as_ref(),
    )
    .with_context(|| format!("resolve GitHub Release source for `{}`", tool.name))
}

pub(super) fn preview_install_source(
    tool: &ToolRef,
    proxy_scope: za_config::ProxyScope,
) -> Result<InstallSource> {
    ensure_not_interrupted()?;

    let Some(policy) = find_tool_policy(&tool.name) else {
        bail!("{}", unsupported_tool_message(&tool.name));
    };

    let Some(release) = policy.github_release else {
        bail!(
            "GitHub Release source is not configured for `{}`",
            tool.name
        );
    };

    preview_github_release_source(tool, release, proxy_scope)
        .with_context(|| format!("preview GitHub Release source for `{}`", tool.name))
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<GithubReleaseAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubReleaseAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
    #[serde(default)]
    updated_at: String,
}

pub(super) fn fetch_latest_version_from_github_release(
    tool_policy: ToolPolicy,
    release_policy: GithubReleasePolicy,
    proxy_scope: za_config::ProxyScope,
) -> Result<String> {
    fetch_exact_latest_version_from_github_release(tool_policy, release_policy, proxy_scope)
}

pub(super) fn fetch_fast_latest_version_from_github_release(
    tool_policy: ToolPolicy,
    release_policy: GithubReleasePolicy,
    proxy_scope: za_config::ProxyScope,
) -> Result<String> {
    match release_policy.track {
        GithubReleaseTrack::VersionedTags => {
            fetch_latest_stable_version_from_github_latest(tool_policy, release_policy, proxy_scope)
        }
        GithubReleaseTrack::VersionedTagsBySemver => {
            fetch_latest_stable_version_from_github_release(
                tool_policy,
                release_policy,
                proxy_scope,
                true,
            )
        }
        GithubReleaseTrack::RollingTagAssets {
            tag,
            asset_prefix,
            asset_suffix,
            version_prefix,
        } => {
            let release = fetch_github_release(
                release_policy.project_label,
                &format!(
                    "/repos/{}/{}/releases/tags/{tag}",
                    release_policy.owner, release_policy.repo
                ),
                proxy_scope,
            )?;
            latest_rolling_asset_version(
                &release,
                tool_policy,
                asset_prefix,
                asset_suffix,
                version_prefix,
            )
        }
    }
}

fn fetch_exact_latest_version_from_github_release(
    tool_policy: ToolPolicy,
    release_policy: GithubReleasePolicy,
    proxy_scope: za_config::ProxyScope,
) -> Result<String> {
    match release_policy.track {
        GithubReleaseTrack::VersionedTags => fetch_latest_stable_version_from_github_release(
            tool_policy,
            release_policy,
            proxy_scope,
            false,
        ),
        GithubReleaseTrack::VersionedTagsBySemver => {
            fetch_latest_stable_version_from_github_release(
                tool_policy,
                release_policy,
                proxy_scope,
                true,
            )
        }
        GithubReleaseTrack::RollingTagAssets {
            tag,
            asset_prefix,
            asset_suffix,
            version_prefix,
        } => {
            let release = fetch_github_release(
                release_policy.project_label,
                &format!(
                    "/repos/{}/{}/releases/tags/{tag}",
                    release_policy.owner, release_policy.repo
                ),
                proxy_scope,
            )?;
            latest_rolling_asset_version(
                &release,
                tool_policy,
                asset_prefix,
                asset_suffix,
                version_prefix,
            )
        }
    }
}

fn fetch_latest_stable_version_from_github_release(
    tool_policy: ToolPolicy,
    release_policy: GithubReleasePolicy,
    proxy_scope: za_config::ProxyScope,
    exhaustive: bool,
) -> Result<String> {
    let releases =
        fetch_versioned_release_candidates(release_policy, None, proxy_scope, exhaustive)?;
    latest_stable_version_for_tags(&releases, release_policy.tag_prefix).with_context(|| {
        format!(
            "resolve latest stable release for `{}`",
            tool_policy.canonical_name
        )
    })
}

fn fetch_latest_stable_version_from_github_latest(
    tool_policy: ToolPolicy,
    release_policy: GithubReleasePolicy,
    proxy_scope: za_config::ProxyScope,
) -> Result<String> {
    let release = fetch_github_release(
        release_policy.project_label,
        &format!(
            "/repos/{}/{}/releases/latest",
            release_policy.owner, release_policy.repo
        ),
        proxy_scope,
    )?;
    latest_stable_version_for_latest_release(&release, release_policy.tag_prefix).with_context(
        || {
            format!(
                "resolve latest GitHub release for `{}`",
                tool_policy.canonical_name
            )
        },
    )
}

pub(super) fn fetch_latest_codex_alpha_version(
    proxy_scope: za_config::ProxyScope,
) -> Result<String> {
    let policy = find_tool_policy("codex").context("codex source policy is not registered")?;
    let release_policy = policy
        .github_release
        .context("codex source policy has no GitHub release policy")?;
    fetch_latest_prerelease_version_from_github_release(
        policy,
        release_policy,
        "alpha",
        proxy_scope,
    )
}

fn fetch_latest_prerelease_version_from_github_release(
    tool_policy: ToolPolicy,
    release_policy: GithubReleasePolicy,
    channel: &str,
    proxy_scope: za_config::ProxyScope,
) -> Result<String> {
    if !matches!(
        release_policy.track,
        GithubReleaseTrack::VersionedTags | GithubReleaseTrack::VersionedTagsBySemver
    ) {
        bail!(
            "`{}` does not support semver pre-release channels",
            tool_policy.canonical_name
        );
    }

    let releases =
        fetch_versioned_release_candidates(release_policy, Some(channel), proxy_scope, false)?;
    latest_prerelease_version_for_channel(&releases, release_policy.tag_prefix, channel)
        .with_context(|| {
            format!(
                "resolve latest {channel} pre-release for `{}`",
                tool_policy.canonical_name
            )
        })
}

fn fetch_versioned_release_candidates(
    release_policy: GithubReleasePolicy,
    prerelease_channel: Option<&str>,
    proxy_scope: za_config::ProxyScope,
    exhaustive: bool,
) -> Result<Vec<GithubRelease>> {
    let mut releases = Vec::new();
    for page in 1..=GITHUB_RELEASE_SCAN_MAX_PAGES {
        let path = format!(
            "/repos/{}/{}/releases?per_page={GITHUB_RELEASE_SCAN_PER_PAGE}&page={page}",
            release_policy.owner, release_policy.repo,
        );
        let mut page_releases =
            fetch_github_releases(release_policy.project_label, &path, proxy_scope)?;
        let is_last_page = page_releases.len() < GITHUB_RELEASE_SCAN_PER_PAGE;
        let page_has_candidate = page_releases.iter().any(|release| {
            if let Some(channel) = prerelease_channel {
                release_matches_prerelease_channel(release, release_policy.tag_prefix, channel)
            } else {
                release_matches_stable_versioned_tag(release, release_policy.tag_prefix)
            }
        });
        releases.append(&mut page_releases);
        if (!exhaustive && page_has_candidate) || is_last_page {
            break;
        }
    }

    Ok(releases)
}

fn latest_stable_version_for_tags(releases: &[GithubRelease], tag_prefix: &str) -> Result<String> {
    releases
        .iter()
        .filter(|release| release_matches_stable_versioned_tag(release, tag_prefix))
        .filter_map(|release| {
            let version = parse_release_version(&release.tag_name, tag_prefix).ok()?;
            let parsed = parse_release_semver(&version).ok()?;
            Some((parsed, version))
        })
        .max_by(|(left, _), (right, _)| left.cmp(right))
        .map(|(_, version)| version)
        .ok_or_else(|| anyhow!("no stable release was found"))
}

fn latest_stable_version_for_latest_release(
    release: &GithubRelease,
    tag_prefix: &str,
) -> Result<String> {
    if release.prerelease || release.draft {
        bail!("latest GitHub release `{}` is not stable", release.tag_name);
    }
    if !tag_prefix.is_empty() && !release.tag_name.starts_with(tag_prefix) {
        bail!(
            "latest GitHub release tag `{}` does not match expected prefix `{tag_prefix}`",
            release.tag_name
        );
    }

    let version = parse_release_version(&release.tag_name, tag_prefix)?;
    let parsed = parse_release_semver(&version).with_context(|| {
        format!(
            "latest GitHub release tag `{}` is not semver",
            release.tag_name
        )
    })?;
    if !parsed.pre.is_empty() {
        bail!(
            "latest GitHub release `{}` is a pre-release",
            release.tag_name
        );
    }
    Ok(version)
}

fn latest_prerelease_version_for_channel(
    releases: &[GithubRelease],
    tag_prefix: &str,
    channel: &str,
) -> Result<String> {
    releases
        .iter()
        .filter(|release| release.prerelease && !release.draft)
        .filter_map(|release| {
            let version = parse_release_version(&release.tag_name, tag_prefix).ok()?;
            let parsed = parse_release_semver(&version).ok()?;
            prerelease_channel_matches(&parsed, channel).then_some((parsed, version))
        })
        .max_by(|(left, _), (right, _)| left.cmp(right))
        .map(|(_, version)| version)
        .ok_or_else(|| anyhow!("no `{channel}` pre-release was found"))
}

fn release_matches_prerelease_channel(
    release: &GithubRelease,
    tag_prefix: &str,
    channel: &str,
) -> bool {
    if !release.prerelease || release.draft {
        return false;
    }
    let Ok(version) = parse_release_version(&release.tag_name, tag_prefix) else {
        return false;
    };
    let Ok(parsed) = parse_release_semver(&version) else {
        return false;
    };
    prerelease_channel_matches(&parsed, channel)
}

fn release_matches_stable_versioned_tag(release: &GithubRelease, tag_prefix: &str) -> bool {
    if release.prerelease || release.draft {
        return false;
    }
    let Ok(version) = parse_release_version(&release.tag_name, tag_prefix) else {
        return false;
    };
    let Ok(parsed) = parse_release_semver(&version) else {
        return false;
    };
    parsed.pre.is_empty()
}

fn prerelease_channel_matches(version: &Version, channel: &str) -> bool {
    version
        .pre
        .as_str()
        .split('.')
        .next()
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(channel))
}

fn download_from_github_release(
    tool: &ToolRef,
    layout: ToolLayout,
    release_policy: GithubReleasePolicy,
    proxy_scope: za_config::ProxyScope,
    display: DownloadDisplay,
    progress_reporter: Option<&DownloadProgressReporter>,
) -> Result<PullSource> {
    let (asset, expected_sha256) = resolve_github_release_asset(tool, release_policy, proxy_scope)?;

    let extraction = match layout {
        ToolLayout::Binary => DownloadExtractionMode::PrimaryEntry(tool),
        ToolLayout::Package => DownloadExtractionMode::KeepArchive,
    };
    let feedback = DownloadFeedback::new(display, progress_reporter);
    download_from_url(
        &asset.browser_download_url,
        expected_sha256.as_deref(),
        proxy_scope,
        extraction,
        feedback,
    )
}

fn preview_github_release_source(
    tool: &ToolRef,
    release_policy: GithubReleasePolicy,
    proxy_scope: za_config::ProxyScope,
) -> Result<InstallSource> {
    let (asset, expected_sha256) = resolve_github_release_asset(tool, release_policy, proxy_scope)?;
    Ok(InstallSource {
        kind: SOURCE_KIND_DOWNLOAD,
        detail: match expected_sha256 {
            Some(expected) => format!("URL {} (sha256={expected})", asset.browser_download_url),
            None => format!("URL {}", asset.browser_download_url),
        },
    })
}

fn resolve_github_release_asset(
    tool: &ToolRef,
    release_policy: GithubReleasePolicy,
    proxy_scope: za_config::ProxyScope,
) -> Result<(GithubReleaseAsset, Option<String>)> {
    let version = normalize_version(&tool.version);
    let asset = match release_policy.track {
        GithubReleaseTrack::VersionedTags | GithubReleaseTrack::VersionedTagsBySemver => {
            let expected_asset_name = (release_policy.expected_asset_name.ok_or_else(|| {
                anyhow!(
                    "release policy for `{}` has no expected asset resolver",
                    tool.name
                )
            })?)(&version)?;
            let tag = format!("{}{}", release_policy.tag_prefix, version);
            let path = format!(
                "/repos/{}/{}/releases/tags/{tag}",
                release_policy.owner, release_policy.repo
            );
            let release = fetch_github_release(release_policy.project_label, &path, proxy_scope)?;
            release
                .assets
                .into_iter()
                .find(|asset| asset.name == expected_asset_name)
                .ok_or_else(|| {
                    anyhow!(
                        "release `{tag}` does not contain expected asset `{expected_asset_name}`"
                    )
                })?
        }
        GithubReleaseTrack::RollingTagAssets {
            tag,
            asset_prefix,
            asset_suffix,
            version_prefix,
        } => {
            let path = format!(
                "/repos/{}/{}/releases/tags/{tag}",
                release_policy.owner, release_policy.repo
            );
            let release = fetch_github_release(release_policy.project_label, &path, proxy_scope)?;
            find_rolling_asset_for_version(
                &release,
                &version,
                asset_prefix,
                asset_suffix,
                version_prefix,
            )?
            .clone()
        }
    };

    let expected_sha256 = match release_policy.verification {
        GithubReleaseVerification::RequiredSha256Digest => Some(
            asset
                .digest
                .as_deref()
                .and_then(parse_github_sha256_digest)
                .ok_or_else(|| {
                    anyhow!("release asset `{}` missing valid sha256 digest", asset.name)
                })?,
        ),
        GithubReleaseVerification::NoSha256Digest => None,
    };

    Ok((asset, expected_sha256))
}

fn latest_rolling_asset_version(
    release: &GithubRelease,
    tool_policy: ToolPolicy,
    asset_prefix: &str,
    asset_suffix: &str,
    version_prefix: &str,
) -> Result<String> {
    release
        .assets
        .iter()
        .filter_map(|asset| {
            parse_rolling_asset_version(&asset.name, asset_prefix, asset_suffix, version_prefix)
                .map(|version| (asset, version))
        })
        .max_by(|(left_asset, left_version), (right_asset, right_version)| {
            left_asset
                .updated_at
                .cmp(&right_asset.updated_at)
                .then_with(|| left_version.cmp(right_version))
        })
        .map(|(_, version)| version)
        .ok_or_else(|| {
            anyhow!(
                "release metadata for `{}` had no versioned rolling assets",
                tool_policy.canonical_name
            )
        })
}

fn find_rolling_asset_for_version<'a>(
    release: &'a GithubRelease,
    requested_version: &str,
    asset_prefix: &str,
    asset_suffix: &str,
    version_prefix: &str,
) -> Result<&'a GithubReleaseAsset> {
    release
        .assets
        .iter()
        .find(|asset| {
            parse_rolling_asset_version(&asset.name, asset_prefix, asset_suffix, version_prefix)
                .is_some_and(|version| {
                    normalize_version(&version) == normalize_version(requested_version)
                })
        })
        .ok_or_else(|| {
            anyhow!("rolling release does not contain asset for version `{requested_version}`")
        })
}

pub(super) fn parse_rolling_asset_version(
    asset_name: &str,
    asset_prefix: &str,
    asset_suffix: &str,
    version_prefix: &str,
) -> Option<String> {
    let middle = asset_name
        .strip_prefix(asset_prefix)?
        .strip_suffix(asset_suffix)?;
    if middle.trim().is_empty() {
        return None;
    }
    Some(format!("{version_prefix}{middle}"))
}

#[cfg(test)]
pub(super) fn proxy_env_keys_for_scheme(scheme: &str) -> &'static [&'static str] {
    shared_proxy_env_keys_for_scheme(scheme)
}

#[cfg(test)]
pub(super) fn split_no_proxy_rules(value: &str) -> Vec<String> {
    shared_split_no_proxy_rules(value)
}

pub(super) type RetryReporter = Arc<dyn Fn(String) + Send + Sync>;

thread_local! {
    // Each tool worker owns its progress row. Scope the reporter to one task
    // so nested retries cannot print over another worker's terminal output.
    static RETRY_REPORTER: std::cell::RefCell<Option<RetryReporter>> = const { std::cell::RefCell::new(None) };
}

pub(super) fn with_retry_reporter<T>(
    reporter: Option<RetryReporter>,
    operation: impl FnOnce() -> T,
) -> T {
    struct RestoreReporter(Option<RetryReporter>);
    impl Drop for RestoreReporter {
        fn drop(&mut self) {
            RETRY_REPORTER.with(|slot| {
                slot.replace(self.0.take());
            });
        }
    }
    let _restore = RestoreReporter(RETRY_REPORTER.with(|slot| slot.replace(reporter)));
    operation()
}

fn report_retry(message: String) -> bool {
    let reporter = RETRY_REPORTER.with(|slot| slot.borrow().clone());
    if let Some(reporter) = reporter {
        reporter(message);
        true
    } else {
        false
    }
}

fn retry_transient_http_operation<T, F>(stage: &str, subject: &str, mut operation: F) -> Result<T>
where
    F: FnMut() -> Result<T>,
{
    for attempt in 1..=HTTP_TRANSIENT_RETRY_MAX_ATTEMPTS {
        ensure_not_interrupted()?;
        match operation() {
            Ok(value) => return Ok(value),
            Err(err) => {
                ensure_not_interrupted()?;
                if attempt >= HTTP_TRANSIENT_RETRY_MAX_ATTEMPTS || !is_retryable_http_error(&err) {
                    return Err(err);
                }
                let reason = summarize_retryable_error(&err);
                let retry_status = format!(
                    "retry {}/{} · {reason}",
                    attempt + 1,
                    HTTP_TRANSIENT_RETRY_MAX_ATTEMPTS
                );
                if !report_retry(retry_status) {
                    eprintln!(
                        "{stage}: retry {}/{} for {subject} after {reason}",
                        attempt + 1,
                        HTTP_TRANSIENT_RETRY_MAX_ATTEMPTS
                    );
                }
                wait_before_http_retry(attempt)?;
            }
        }
    }
    unreachable!("retry loop must return on success or terminal failure")
}

fn wait_before_http_retry(attempt: usize) -> Result<()> {
    let until = Instant::now() + transient_http_retry_backoff(attempt);
    while let Some(left) = until.checked_duration_since(Instant::now()) {
        ensure_not_interrupted()?;
        thread::sleep(left.min(INTERRUPT_POLL_INTERVAL));
    }
    Ok(())
}

fn transient_http_retry_backoff(attempt: usize) -> Duration {
    Duration::from_millis(HTTP_TRANSIENT_RETRY_BASE_DELAY_MS.saturating_mul(attempt.max(1) as u64))
}

fn summarize_retryable_error(err: &anyhow::Error) -> String {
    for cause in err.chain() {
        if let Some(io_err) = cause.downcast_ref::<io::Error>() {
            return match io_err.kind() {
                io::ErrorKind::UnexpectedEof => "connection closed early".to_string(),
                io::ErrorKind::TimedOut => "request timed out".to_string(),
                io::ErrorKind::ConnectionReset => "connection reset".to_string(),
                io::ErrorKind::ConnectionAborted => "connection aborted".to_string(),
                io::ErrorKind::BrokenPipe => "broken pipe".to_string(),
                io::ErrorKind::Interrupted => "operation interrupted".to_string(),
                io::ErrorKind::WouldBlock => "socket would block".to_string(),
                _ => truncate_for_log(&io_err.to_string(), 120),
            };
        }
        if let Some(reqx_err) = cause.downcast_ref::<reqx::Error>() {
            return summarize_reqx_retryable_error(reqx_err);
        }
    }
    truncate_for_log(&format!("{err:#}").replace('\n', ": "), 180)
}

fn summarize_reqx_retryable_error(err: &reqx::Error) -> String {
    match err {
        reqx::Error::Transport { kind, .. } => match kind {
            reqx::TransportErrorKind::Dns => "dns lookup failed".to_string(),
            reqx::TransportErrorKind::Connect => "connect failed".to_string(),
            reqx::TransportErrorKind::Tls => "tls read failed".to_string(),
            reqx::TransportErrorKind::Read => "connection closed early".to_string(),
            _ => truncate_for_log(&err.to_string(), 120),
        },
        reqx::Error::Timeout { phase, .. } => match phase {
            reqx::TimeoutPhase::Transport => "request timed out".to_string(),
            reqx::TimeoutPhase::ResponseBody => "response read timed out".to_string(),
            _ => truncate_for_log(&err.to_string(), 120),
        },
        reqx::Error::ReadBody { .. } => "response body read failed".to_string(),
        _ => truncate_for_log(&err.to_string(), 120),
    }
}

fn is_retryable_http_error(err: &anyhow::Error) -> bool {
    for cause in err.chain() {
        if let Some(reqx_err) = cause.downcast_ref::<reqx::Error>()
            && is_retryable_reqx_error(reqx_err)
        {
            return true;
        }
        if let Some(io_err) = cause.downcast_ref::<io::Error>()
            && matches!(
                io_err.kind(),
                io::ErrorKind::UnexpectedEof
                    | io::ErrorKind::TimedOut
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::BrokenPipe
                    | io::ErrorKind::Interrupted
                    | io::ErrorKind::WouldBlock
            )
        {
            return true;
        }
    }

    let message = format!("{err:#}").to_ascii_lowercase();
    message.contains("unexpected end of file")
        || message.contains("connection reset")
        || message.contains("connection aborted")
        || message.contains("broken pipe")
        || message.contains("tls connect error")
        || message.contains("tls handshake eof")
}

fn is_retryable_reqx_error(err: &reqx::Error) -> bool {
    match err {
        reqx::Error::Transport { kind, .. } => matches!(
            kind,
            reqx::TransportErrorKind::Dns
                | reqx::TransportErrorKind::Connect
                | reqx::TransportErrorKind::Tls
                | reqx::TransportErrorKind::Read
        ),
        reqx::Error::Timeout { phase, .. } => matches!(
            phase,
            reqx::TimeoutPhase::Transport | reqx::TimeoutPhase::ResponseBody
        ),
        reqx::Error::ReadBody { .. } => true,
        _ => false,
    }
}

fn build_http_client(
    base_url: &str,
    client_name: &str,
    follow_redirects: bool,
    proxy_scope: za_config::ProxyScope,
) -> Result<Client> {
    build_shared_http_client(
        base_url,
        client_name,
        if follow_redirects {
            ClientProfile::HighThroughput
        } else {
            ClientProfile::StandardSdk
        },
        follow_redirects,
        Duration::from_secs(HTTP_TIMEOUT_SECS),
        proxy_scope,
    )
}

fn fetch_github_release(
    project_label: &str,
    path: &str,
    proxy_scope: za_config::ProxyScope,
) -> Result<GithubRelease> {
    fetch_github_json(project_label, path, proxy_scope)
}

fn fetch_github_releases(
    project_label: &str,
    path: &str,
    proxy_scope: za_config::ProxyScope,
) -> Result<Vec<GithubRelease>> {
    fetch_github_json(project_label, path, proxy_scope)
}

fn fetch_github_json<T: DeserializeOwned>(
    project_label: &str,
    path: &str,
    proxy_scope: za_config::ProxyScope,
) -> Result<T> {
    ensure_not_interrupted()?;

    let client = build_http_client(GITHUB_API_BASE, "za-tool-manager", false, proxy_scope)
        .context("build GitHub API client")?;
    let github_token = resolve_github_token()?;
    fetch_github_json_with_client(&client, project_label, path, github_token.as_deref())
}

fn fetch_github_json_with_client<T: DeserializeOwned>(
    client: &Client,
    project_label: &str,
    path: &str,
    github_token: Option<&str>,
) -> Result<T> {
    retry_transient_http_operation(
        "source",
        &format!("{project_label} release metadata"),
        || {
            let mut req = client
                .get(path)
                .max_response_body_bytes(GITHUB_RELEASE_METADATA_MAX_BYTES)
                .redirect_policy(reqx::advanced::RedirectPolicy::follow());
            req = req
                .try_header("user-agent", HTTP_USER_AGENT)
                .context("set GitHub user-agent")?;
            req = req
                .try_header("accept", "application/vnd.github+json")
                .context("set GitHub accept header")?;
            if let Some(token) = github_token {
                req = req
                    .try_header("authorization", &format!("Bearer {token}"))
                    .context("set GitHub authorization header")?;
            }

            let response = network::wait_for(move || req.send()).with_context(|| {
                format!("query {project_label} release metadata ({PROXY_HINT})")
            })?;
            let status = response.status();
            if !status.is_success() {
                bail!(
                    "query {project_label} release metadata failed: status {} body {}",
                    status,
                    truncate_for_log(&response.text_lossy(), 200)
                );
            }
            response
                .json::<T>()
                .with_context(|| format!("parse {project_label} release JSON"))
        },
    )
}

pub(super) fn parse_release_version(tag_name: &str, tag_prefix: &str) -> Result<String> {
    let version = tag_name
        .strip_prefix(tag_prefix)
        .map(str::to_string)
        .unwrap_or_else(|| normalize_version(tag_name));
    if version.is_empty() {
        bail!("latest release tag had no version");
    }
    Ok(version)
}

fn parse_release_semver(version: &str) -> Result<Version> {
    if let Ok(parsed) = Version::parse(version) {
        return Ok(parsed);
    }

    let (core, suffix) = split_semver_core_suffix(version);
    if !is_two_segment_numeric_version(core) {
        return Version::parse(version).with_context(|| format!("parse semver `{version}`"));
    }

    Version::parse(&format!("{core}.0{suffix}"))
        .with_context(|| format!("parse semver `{version}`"))
}

fn split_semver_core_suffix(version: &str) -> (&str, &str) {
    match version.find(['-', '+']) {
        Some(index) => version.split_at(index),
        None => (version, ""),
    }
}

fn is_two_segment_numeric_version(core: &str) -> bool {
    let mut parts = core.split('.');
    let Some(major) = parts.next() else {
        return false;
    };
    let Some(minor) = parts.next() else {
        return false;
    };
    parts.next().is_none()
        && !major.is_empty()
        && !minor.is_empty()
        && major.bytes().all(|byte| byte.is_ascii_digit())
        && minor.bytes().all(|byte| byte.is_ascii_digit())
}

fn resolve_github_token() -> Result<Option<String>> {
    if let Ok(token) = env::var("GITHUB_TOKEN") {
        let token = token.trim();
        if !token.is_empty() {
            return Ok(Some(token.to_string()));
        }
    }
    if let Ok(token) = env::var("GH_TOKEN") {
        let token = token.trim();
        if !token.is_empty() {
            return Ok(Some(token.to_string()));
        }
    }
    za_config::load_github_token()
}

pub(super) fn parse_github_sha256_digest(digest: &str) -> Option<String> {
    let normalized = digest.trim();
    let (algo, value) = normalized.split_once(':')?;
    if !algo.eq_ignore_ascii_case("sha256") {
        return None;
    }
    let value = value.trim();
    if value.len() != 64 || !value.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(value.to_ascii_lowercase())
}

fn download_from_url(
    url: &str,
    expected_sha256: Option<&str>,
    proxy_scope: za_config::ProxyScope,
    extraction: DownloadExtractionMode<'_>,
    feedback: DownloadFeedback<'_>,
) -> Result<PullSource> {
    ensure_not_interrupted()?;
    let download_root = unique_temp_dir(TEMP_DIR_PREFIX_DOWNLOAD)?;
    let run = (|| -> Result<PullSource> {
        let url_parts = parse_url_parts(url)?;
        let asset_name = url_parts.file_name.clone();
        let asset_path = download_root.join(&asset_name);
        let client = build_http_client(&url_parts.base_url, "za-tool-manager", true, proxy_scope)
            .context("build HTTP client")?;
        if feedback.is_detailed() {
            eprintln!("download: probing server capabilities for `{asset_name}`");
        }
        let probe = probe_parallel_download_support(&client, &url_parts.path_and_query).unwrap_or(
            DownloadProbe {
                total_bytes: 0,
                range_supported: false,
            },
        );
        let total_bytes = (probe.total_bytes > 0).then_some(probe.total_bytes);

        if let Some(plan) = build_parallel_download_plan(probe.total_bytes, probe.range_supported) {
            if feedback.is_detailed() {
                eprintln!(
                    "download: parallel transfer ({} parts, {})",
                    plan.parts,
                    format_bytes_u64(plan.total_bytes)
                );
            }
            match download_to_path_parallel(
                &url_parts,
                url,
                &asset_path,
                proxy_scope,
                plan,
                feedback,
            ) {
                Ok(()) => {}
                Err(ParallelDownloadError::Unsupported) => {
                    retry_single_stream_after_parallel_failure(
                        format!(
                            "range transfer unavailable for `{asset_name}`; falling back to single stream"
                        ),
                        SingleStreamDownload {
                            client: &client,
                            path_and_query: &url_parts.path_and_query,
                            url,
                            asset_path: &asset_path,
                            total_bytes,
                        },
                        feedback,
                    )?;
                }
                Err(ParallelDownloadError::Failed(err)) => return Err(err),
            }
        } else {
            if feedback.is_detailed() {
                eprintln!("download: single-stream transfer");
            }
            download_to_path_single(
                &client,
                &url_parts.path_and_query,
                url,
                &asset_path,
                total_bytes,
                feedback,
            )?;
        }
        ensure_not_interrupted()?;

        if let Some(expected_sha256) = expected_sha256 {
            verify_sha256_file(&asset_path, expected_sha256)?;
            if feedback.is_detailed() {
                eprintln!("verify: sha256 ok");
            }
        }

        let (artifact, resolved_path) = match extraction {
            DownloadExtractionMode::KeepArchive => {
                if detect_archive_kind(&asset_name).is_none() {
                    bail!("downloaded asset `{asset_name}` is not a supported archive");
                }
                (PullArtifactKind::Archive, asset_path)
            }
            DownloadExtractionMode::PrimaryEntry(tool) => {
                let resolved_path = if detect_archive_kind(&asset_name).is_some() {
                    if feedback.is_detailed() {
                        eprintln!("extract: {asset_name}");
                    }
                    extract_archive_primary_entry(tool, &asset_path, &download_root)?
                } else {
                    asset_path
                };
                (PullArtifactKind::File, resolved_path)
            }
        };

        Ok(PullSource::temp(
            SOURCE_KIND_DOWNLOAD,
            artifact,
            resolved_path,
            match expected_sha256 {
                Some(expected) => format!("URL {url} (sha256={expected})"),
                None => format!("URL {url}"),
            },
            download_root.clone(),
        ))
    })();

    if run.is_err() {
        emit_download_progress(feedback.reporter(), 0, None, Duration::ZERO, true);
        unregister_temp_dir(&download_root);
        let _ = fs::remove_dir_all(&download_root);
    }
    run
}

fn retry_single_stream_after_parallel_failure(
    reason: String,
    request: SingleStreamDownload<'_>,
    feedback: DownloadFeedback<'_>,
) -> Result<()> {
    if !report_retry(reason.clone()) && feedback.shows_progress() {
        eprintln!("download: {reason}");
    }
    if feedback.is_detailed() {
        eprintln!("download: single-stream transfer");
    }
    download_to_path_single(
        request.client,
        request.path_and_query,
        request.url,
        request.asset_path,
        request.total_bytes,
        feedback,
    )
}

fn build_download_request(
    client: &Client,
    path_and_query: &str,
    range: Option<DownloadRange>,
) -> Result<reqx::blocking::RequestBuilder> {
    let mut req = client
        .get(path_and_query.to_string())
        .auto_accept_encoding(false);
    req = req
        .try_header("user-agent", HTTP_USER_AGENT)
        .context("set download user-agent")?;
    if let Some(range) = range {
        req = req
            .try_header("range", &format!("bytes={}-{}", range.start, range.end))
            .context("set HTTP range header")?;
    }
    Ok(req)
}

fn probe_parallel_download_support(client: &Client, path_and_query: &str) -> Option<DownloadProbe> {
    let range = DownloadRange { start: 0, end: 0 };
    let req = build_download_request(client, path_and_query, Some(range)).ok()?;
    let resp = network::wait_for(move || req.send_stream()).ok()?;
    let status = resp.status();
    if status != 206 {
        return Some(DownloadProbe {
            total_bytes: 0,
            range_supported: false,
        });
    }
    let total_bytes = resp
        .headers()
        .get("content-range")
        .and_then(|value| value.to_str().ok())
        .and_then(parse_content_range_total)?;
    Some(DownloadProbe {
        total_bytes,
        range_supported: true,
    })
}

fn parse_content_range_total(value: &str) -> Option<u64> {
    let (_, total) = value.trim().split_once('/')?;
    if total.trim() == "*" {
        return None;
    }
    total.trim().parse::<u64>().ok()
}

fn build_parallel_download_plan(
    total_bytes: u64,
    range_supported: bool,
) -> Option<ParallelDownloadPlan> {
    if !range_supported || total_bytes < PARALLEL_DOWNLOAD_MIN_BYTES {
        return None;
    }
    let parts = ((total_bytes / PARALLEL_DOWNLOAD_MIN_PART_BYTES) as usize)
        .min(PARALLEL_DOWNLOAD_MAX_PARTS);
    if parts < 2 {
        return None;
    }
    Some(ParallelDownloadPlan { total_bytes, parts })
}

fn split_download_ranges(plan: ParallelDownloadPlan) -> Vec<DownloadRange> {
    let mut ranges = Vec::with_capacity(plan.parts);
    let base = plan.total_bytes / plan.parts as u64;
    let remainder = plan.total_bytes % plan.parts as u64;
    let mut start = 0_u64;
    for index in 0..plan.parts {
        let extra = if (index as u64) < remainder { 1 } else { 0 };
        let len = base + extra;
        let end = start + len.saturating_sub(1);
        ranges.push(DownloadRange { start, end });
        start = end.saturating_add(1);
    }
    ranges
}

fn download_to_path_single(
    client: &Client,
    path_and_query: &str,
    url: &str,
    asset_path: &Path,
    known_total_bytes: Option<u64>,
    feedback: DownloadFeedback<'_>,
) -> Result<()> {
    let asset_label = asset_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("asset")
        .to_string();

    retry_transient_http_operation("download", &format!("`{asset_label}`"), || {
        download_to_path_single_once(
            client,
            path_and_query,
            url,
            asset_path,
            known_total_bytes,
            feedback,
        )
    })
}

fn download_to_path_single_once(
    client: &Client,
    path_and_query: &str,
    url: &str,
    asset_path: &Path,
    known_total_bytes: Option<u64>,
    feedback: DownloadFeedback<'_>,
) -> Result<()> {
    let req = build_download_request(client, path_and_query, None)?
        .request_timeout(SINGLE_STREAM_TIMEOUT)
        .total_timeout(Some(SINGLE_STREAM_TIMEOUT));
    let resp = network::wait_for(move || req.send_stream())
        .with_context(|| format!("download from `{url}` ({PROXY_HINT})"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = network::wait_for(move || resp.into_text_lossy_limited(16 * 1024))
            .unwrap_or_else(|_| "<body unavailable>".to_string());
        bail!(
            "download from `{url}` failed: status {} body {}",
            status,
            truncate_for_log(&body, 200)
        );
    }
    let total_bytes = known_total_bytes.or_else(|| {
        resp.headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
    });

    let mut out = File::create(asset_path)
        .with_context(|| format!("create downloaded file {}", asset_path.display()))?;
    let mut downloaded = 0_u64;
    let start = Instant::now();
    let mut last_report = Instant::now();
    let print_progress = feedback.shows_progress();
    let track_progress = feedback.tracks_progress();
    let progress_bar = feedback.progress_bar(total_bytes);
    if track_progress {
        emit_download_progress(
            feedback.reporter(),
            downloaded,
            total_bytes,
            start.elapsed(),
            false,
        );
    }
    let chunks = network::chunks(resp, DOWNLOAD_READ_CHUNK_SIZE);
    while let Some(bytes) = chunks
        .next_chunk()
        .with_context(|| format!("read bytes from `{url}`"))?
    {
        out.write_all(&bytes)
            .with_context(|| format!("write downloaded file {}", asset_path.display()))?;
        downloaded = downloaded.saturating_add(bytes.len() as u64);
        if track_progress {
            report_download_progress(
                &mut last_report,
                DownloadProgressReport {
                    downloaded,
                    total_bytes,
                    elapsed: start.elapsed(),
                    finished: false,
                    progress_bar: progress_bar.as_ref(),
                    print_line: print_progress,
                    progress_reporter: feedback.reporter(),
                },
            );
        }
    }
    if let Some(total_bytes) = total_bytes
        && downloaded != total_bytes
    {
        bail!(
            "download from `{url}` ended after {} bytes, expected {} bytes",
            downloaded,
            total_bytes
        );
    }
    out.flush()
        .with_context(|| format!("flush downloaded file {}", asset_path.display()))?;
    if track_progress {
        report_download_progress(
            &mut last_report,
            DownloadProgressReport {
                downloaded,
                total_bytes,
                elapsed: start.elapsed(),
                finished: true,
                progress_bar: progress_bar.as_ref(),
                print_line: print_progress,
                progress_reporter: feedback.reporter(),
            },
        );
    }
    Ok(())
}

fn download_to_path_parallel(
    url_parts: &UrlParts,
    url: &str,
    asset_path: &Path,
    proxy_scope: za_config::ProxyScope,
    plan: ParallelDownloadPlan,
    feedback: DownloadFeedback<'_>,
) -> Result<(), ParallelDownloadError> {
    let part_paths = split_download_ranges(plan)
        .into_iter()
        .enumerate()
        .map(|(index, range)| {
            (
                index,
                range,
                asset_path.with_extension(format!("part-{index}")),
            )
        })
        .collect::<Vec<_>>();
    let start = Instant::now();
    let progress = Arc::new(AtomicU64::new(0));
    let reporter_stop = Arc::new(AtomicBool::new(false));
    let total_bytes = Some(plan.total_bytes);
    let print_progress = feedback.shows_progress();
    let track_progress = feedback.tracks_progress();
    let progress_bar = feedback.progress_bar(total_bytes);

    let mut reporter = track_progress.then(|| {
        spawn_parallel_download_reporter(
            Arc::clone(&progress),
            Arc::clone(&reporter_stop),
            total_bytes,
            start,
            progress_bar.clone(),
            print_progress,
            feedback.reporter().cloned(),
        )
    });

    let handles = part_paths
        .clone()
        .into_iter()
        .map(|(index, range, part_path)| {
            let base_url = url_parts.base_url.clone();
            let path_and_query = url_parts.path_and_query.clone();
            let progress = Arc::clone(&progress);
            let reporter_stop = Arc::clone(&reporter_stop);
            thread::spawn(move || {
                let result = download_range_part(
                    &base_url,
                    &path_and_query,
                    range,
                    &part_path,
                    proxy_scope,
                    RangeTransfer {
                        progress: &progress,
                        stop: &reporter_stop,
                        total_bytes: plan.total_bytes,
                    },
                );
                if result.is_err() {
                    reporter_stop.store(true, Ordering::SeqCst);
                }
                result.map(|_| (index, part_path))
            })
        })
        .collect::<Vec<_>>();

    let mut ordered_parts = vec![PathBuf::new(); part_paths.len()];
    let mut failure = None;
    for handle in handles {
        match handle.join() {
            Ok(Ok((index, part_path))) => ordered_parts[index] = part_path,
            Ok(Err(err)) => {
                failure.get_or_insert(err);
            }
            Err(_) => {
                reporter_stop.store(true, Ordering::SeqCst);
                failure.get_or_insert_with(|| {
                    ParallelDownloadError::Failed(anyhow!(
                        "parallel download worker panicked for `{url}`"
                    ))
                });
            }
        }
    }

    reporter_stop.store(true, Ordering::SeqCst);
    if let Some(reporter) = reporter.take() {
        let _ = reporter.join();
    }
    if let Some(err) = failure {
        cleanup_parallel_part_files(&part_paths);
        return Err(err);
    }
    merge_parallel_part_files(&ordered_parts, asset_path).map_err(ParallelDownloadError::Failed)?;
    cleanup_parallel_part_files(&part_paths);
    if track_progress {
        let mut last_report = Instant::now();
        report_download_progress(
            &mut last_report,
            DownloadProgressReport {
                downloaded: progress.load(Ordering::SeqCst),
                total_bytes,
                elapsed: start.elapsed(),
                finished: true,
                progress_bar: progress_bar.as_ref(),
                print_line: print_progress,
                progress_reporter: feedback.reporter(),
            },
        );
    }

    Ok(())
}

fn spawn_parallel_download_reporter(
    progress: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    total_bytes: Option<u64>,
    start: Instant,
    progress_bar: Option<ProgressBar>,
    print_line: bool,
    progress_reporter: Option<DownloadProgressReporter>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut last_report = Instant::now();
        emit_download_progress(
            progress_reporter.as_ref(),
            progress.load(Ordering::SeqCst),
            total_bytes,
            start.elapsed(),
            false,
        );
        while !stop.load(Ordering::SeqCst) {
            report_download_progress(
                &mut last_report,
                DownloadProgressReport {
                    downloaded: progress.load(Ordering::SeqCst),
                    total_bytes,
                    elapsed: start.elapsed(),
                    finished: false,
                    progress_bar: progress_bar.as_ref(),
                    print_line,
                    progress_reporter: progress_reporter.as_ref(),
                },
            );
            thread::sleep(Duration::from_millis(150));
        }
    })
}

struct RangeTransfer<'a> {
    progress: &'a AtomicU64,
    stop: &'a AtomicBool,
    total_bytes: u64,
}

fn download_range_part(
    base_url: &str,
    path_and_query: &str,
    range: DownloadRange,
    part_path: &Path,
    proxy_scope: za_config::ProxyScope,
    transfer: RangeTransfer<'_>,
) -> Result<(), ParallelDownloadError> {
    let client = build_http_client(base_url, "za-tool-manager", true, proxy_scope)
        .map_err(ParallelDownloadError::Failed)?;
    let mut out = File::create(part_path)
        .with_context(|| format!("create partial file {}", part_path.display()))
        .map_err(ParallelDownloadError::Failed)?;
    let mut next = range.start;
    while next <= range.end && !transfer.stop.load(Ordering::Relaxed) {
        let end = next.saturating_add(RANGE_REQUEST_BYTES - 1).min(range.end);
        for attempt in 1..=HTTP_TRANSIENT_RETRY_MAX_ATTEMPTS {
            ensure_not_interrupted().map_err(ParallelDownloadError::Failed)?;
            if transfer.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            match read_range_to_file(&client, path_and_query, &mut next, end, &mut out, &transfer) {
                Err(ParallelDownloadError::Failed(err))
                    if next <= end
                        && attempt < HTTP_TRANSIENT_RETRY_MAX_ATTEMPTS
                        && is_retryable_http_error(&err) =>
                {
                    wait_before_http_retry(attempt).map_err(ParallelDownloadError::Failed)?;
                }
                result => {
                    result?;
                    break;
                }
            }
        }
    }
    out.flush()
        .context("flush downloaded range")
        .map_err(ParallelDownloadError::Failed)
}

fn read_range_to_file(
    client: &Client,
    path: &str,
    next: &mut u64,
    end: u64,
    out: &mut File,
    transfer: &RangeTransfer<'_>,
) -> Result<(), ParallelDownloadError> {
    let range = DownloadRange { start: *next, end };
    let req =
        build_download_request(client, path, Some(range)).map_err(ParallelDownloadError::Failed)?;
    let resp = network::wait_for(move || req.send_stream())
        .with_context(|| format!("fetch bytes {}-{end} from {path}", range.start))
        .map_err(ParallelDownloadError::Failed)?;
    if resp.status() == 200 || resp.status() == 416 {
        return Err(ParallelDownloadError::Unsupported);
    }
    if resp.status() != 206 {
        return Err(ParallelDownloadError::Failed(anyhow!(
            "range request to {path} returned HTTP {}",
            resp.status()
        )));
    }
    let expected = format!("bytes {}-{end}/{}", range.start, transfer.total_bytes);
    if resp
        .headers()
        .get("content-range")
        .and_then(|v| v.to_str().ok())
        != Some(expected.as_str())
    {
        return Err(ParallelDownloadError::Unsupported);
    }
    let chunks = network::chunks(resp, DOWNLOAD_READ_CHUNK_SIZE);
    while *next <= end && !transfer.stop.load(Ordering::Relaxed) {
        let bytes = chunks
            .next_chunk()
            .with_context(|| format!("read bytes {}-{end} from {path}", *next))
            .map_err(ParallelDownloadError::Failed)?
            .ok_or_else(|| {
                ParallelDownloadError::Failed(
                    io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!("range ended before byte {end}"),
                    )
                    .into(),
                )
            })?;
        let n = bytes.len();
        if n as u64 > end - *next + 1 {
            return Err(ParallelDownloadError::Unsupported);
        }
        out.write_all(&bytes)
            .context("write downloaded range")
            .map_err(ParallelDownloadError::Failed)?;
        *next += n as u64;
        transfer.progress.fetch_add(n as u64, Ordering::Relaxed);
    }
    Ok(())
}

fn merge_parallel_part_files(part_paths: &[PathBuf], asset_path: &Path) -> Result<()> {
    let mut out = File::create(asset_path)
        .with_context(|| format!("create downloaded file {}", asset_path.display()))?;
    let mut chunk = [0_u8; DOWNLOAD_READ_CHUNK_SIZE];
    for part_path in part_paths {
        let mut part = File::open(part_path)
            .with_context(|| format!("open partial file {}", part_path.display()))?;
        loop {
            let read = part
                .read(&mut chunk)
                .with_context(|| format!("read partial file {}", part_path.display()))?;
            if read == 0 {
                break;
            }
            out.write_all(&chunk[..read])
                .with_context(|| format!("write downloaded file {}", asset_path.display()))?;
        }
    }
    out.flush()
        .with_context(|| format!("flush downloaded file {}", asset_path.display()))?;
    Ok(())
}

fn cleanup_parallel_part_files(parts: &[(usize, DownloadRange, PathBuf)]) {
    for (_, _, part_path) in parts {
        let _ = fs::remove_file(part_path);
    }
}

#[cfg(test)]
pub(super) fn download_filename(url: &str) -> Result<String> {
    Ok(parse_url_parts(url)?.file_name)
}

#[derive(Debug)]
struct UrlParts {
    base_url: String,
    path_and_query: String,
    file_name: String,
}

fn parse_url_parts(url: &str) -> Result<UrlParts> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| anyhow!("invalid URL `{url}`: missing scheme"))?;
    if scheme != "http" && scheme != "https" {
        bail!("unsupported URL scheme `{scheme}` in `{url}`");
    }

    let slash_idx = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..slash_idx];
    if authority.is_empty() {
        bail!("invalid URL `{url}`: missing host");
    }

    let path_with_query_and_fragment = if slash_idx < rest.len() {
        &rest[slash_idx..]
    } else {
        "/"
    };
    let path_and_query = path_with_query_and_fragment
        .split('#')
        .next()
        .unwrap_or(path_with_query_and_fragment)
        .to_string();
    let path_only = path_and_query
        .split('?')
        .next()
        .unwrap_or(path_and_query.as_str());

    let file_name = path_only
        .rsplit('/')
        .find(|part| !part.is_empty())
        .ok_or_else(|| anyhow!("URL path has no file name: `{url}`"))?
        .to_string();

    Ok(UrlParts {
        base_url: format!("{scheme}://{authority}"),
        path_and_query: if path_and_query.is_empty() {
            "/".to_string()
        } else {
            path_and_query
        },
        file_name,
    })
}

fn format_bytes_u64(bytes: u64) -> String {
    format_bytes_f64(bytes as f64)
}

fn format_bytes_f64(bytes: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes.max(0.0);
    let mut idx = 0usize;
    while value >= 1024.0 && idx < UNITS.len() - 1 {
        value /= 1024.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{value:.0} {}", UNITS[idx])
    } else {
        format!("{value:.1} {}", UNITS[idx])
    }
}

fn new_download_progress_bar(
    target: ProgressDrawTarget,
    total_bytes: Option<u64>,
) -> Option<ProgressBar> {
    let bar = ProgressBar::with_draw_target(None, target);
    if bar.is_hidden() {
        return None;
    }
    bar.set_style(ProgressStyle::with_template("{wide_msg}").expect("download template"));
    bar.set_message(render_download_progress(0, total_bytes, Duration::ZERO));
    Some(bar)
}

pub(super) fn render_download_progress(
    downloaded: u64,
    total_bytes: Option<u64>,
    elapsed: Duration,
) -> String {
    format!(
        "download: {}",
        render_download_progress_brief(downloaded, total_bytes, elapsed)
    )
}

pub(super) fn render_download_progress_brief(
    downloaded: u64,
    total_bytes: Option<u64>,
    elapsed: Duration,
) -> String {
    let rate = if elapsed.is_zero() {
        0.0
    } else {
        downloaded as f64 / elapsed.as_secs_f64()
    };

    match total_bytes {
        Some(total) if total > 0 => {
            let pct = (downloaded as f64 / total as f64 * 100.0).clamp(0.0, 100.0);
            format!(
                "{} / {} ({pct:.1}%, {}/s)",
                format_bytes_u64(downloaded),
                format_bytes_u64(total),
                format_bytes_f64(rate)
            )
        }
        _ => format!(
            "{} ({}/s)",
            format_bytes_u64(downloaded),
            format_bytes_f64(rate)
        ),
    }
}

struct DownloadProgressReport<'a> {
    downloaded: u64,
    total_bytes: Option<u64>,
    elapsed: Duration,
    finished: bool,
    progress_bar: Option<&'a ProgressBar>,
    print_line: bool,
    progress_reporter: Option<&'a DownloadProgressReporter>,
}

impl DownloadProgressReport<'_> {
    fn write_line(&self, mut output: impl Write) -> io::Result<()> {
        let line = render_download_progress(self.downloaded, self.total_bytes, self.elapsed);
        if let Some(bar) = self.progress_bar {
            if !self.finished {
                bar.set_message(line);
                return Ok(());
            }
            bar.finish_and_clear();
        }
        writeln!(output, "{line}")
    }
}

fn report_download_progress(last_report: &mut Instant, report: DownloadProgressReport<'_>) {
    let now = Instant::now();
    if !report.finished && now.duration_since(*last_report) < Duration::from_secs(1) {
        return;
    }
    emit_download_progress(
        report.progress_reporter,
        report.downloaded,
        report.total_bytes,
        report.elapsed,
        report.finished,
    );
    if report.print_line {
        let _ = report.write_line(io::stderr());
    }
    *last_report = now;
}

fn verify_sha256_file(path: &Path, expected_hex: &str) -> Result<()> {
    let actual_hex = sha256_file(path)?;
    if !actual_hex.eq_ignore_ascii_case(expected_hex) {
        bail!(
            "sha256 mismatch for {}: expected {}, got {}",
            path.display(),
            expected_hex,
            actual_hex
        );
    }
    Ok(())
}

pub(super) fn truncate_for_log(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let mut out = String::new();
    for c in input.chars().take(max_chars.saturating_sub(1)) {
        out.push(c);
    }
    out.push('…');
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveKind {
    TarGz,
    TarXz,
    Zip,
}

fn detect_archive_kind(name: &str) -> Option<ArchiveKind> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        Some(ArchiveKind::TarGz)
    } else if lower.ends_with(".tar.xz") || lower.ends_with(".txz") {
        Some(ArchiveKind::TarXz)
    } else if lower.ends_with(".zip") {
        Some(ArchiveKind::Zip)
    } else {
        None
    }
}

#[cfg(test)]
pub(super) fn is_tar_gz_asset(name: &str) -> bool {
    detect_archive_kind(name) == Some(ArchiveKind::TarGz)
}

#[cfg(test)]
pub(super) fn is_tar_xz_asset(name: &str) -> bool {
    detect_archive_kind(name) == Some(ArchiveKind::TarXz)
}

#[cfg(test)]
pub(super) fn is_zip_asset(name: &str) -> bool {
    detect_archive_kind(name) == Some(ArchiveKind::Zip)
}

pub(super) fn extract_archive_into_dir(archive_path: &Path, dst: &Path) -> Result<()> {
    remove_path_if_exists(dst)?;
    fs::create_dir_all(dst).with_context(|| format!("create {}", dst.display()))?;

    let archive_kind = detect_archive_kind(
        archive_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default(),
    )
    .ok_or_else(|| anyhow!("unsupported archive format `{}`", archive_path.display()))?;

    let file = File::open(archive_path)
        .with_context(|| format!("open archive {}", archive_path.display()))?;
    match archive_kind {
        ArchiveKind::TarGz => {
            let gz = GzDecoder::new(file);
            let mut archive = Archive::new(gz);
            archive
                .unpack(dst)
                .with_context(|| format!("extract archive {}", archive_path.display()))?;
        }
        ArchiveKind::TarXz => {
            let xz = XzDecoder::new(file);
            let mut archive = Archive::new(xz);
            archive
                .unpack(dst)
                .with_context(|| format!("extract archive {}", archive_path.display()))?;
        }
        ArchiveKind::Zip => {
            let mut archive = ZipArchive::new(file)
                .with_context(|| format!("read zip archive {}", archive_path.display()))?;
            archive
                .extract(dst)
                .with_context(|| format!("extract archive {}", archive_path.display()))?;
        }
    }

    Ok(())
}

fn extract_archive_primary_entry(
    tool: &ToolRef,
    archive_path: &Path,
    root: &Path,
) -> Result<PathBuf> {
    let unpack_dir = root.join("unpack");
    extract_archive_into_dir(archive_path, &unpack_dir)?;
    select_executable_from_dir(tool, &unpack_dir)
}

fn select_executable_from_dir(tool: &ToolRef, dir: &Path) -> Result<PathBuf> {
    let mut files = Vec::new();
    collect_files_recursive(dir, &mut files)?;
    files.sort();

    if files.is_empty() {
        bail!("archive has no regular files");
    }

    let candidates = command_candidates(&tool.name);
    let mut named_matches: Vec<PathBuf> = files
        .iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|name| candidates.iter().any(|c| c == name))
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    named_matches.sort();
    if let Some(first) = named_matches.first() {
        return Ok(first.clone());
    }

    let mut executable_files: Vec<PathBuf> = files
        .iter()
        .filter(|p| is_executable_file(p))
        .cloned()
        .collect();
    executable_files.sort();
    if executable_files.len() == 1 {
        return Ok(executable_files.remove(0));
    }

    bail!(
        "cannot determine executable from archive for `{}`; expected one of {:?}",
        tool.name,
        candidates
    )
}

fn collect_files_recursive(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("read dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let typ = entry.file_type()?;
        if typ.is_dir() {
            collect_files_recursive(&path, out)?;
        } else if typ.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

fn unique_temp_dir(prefix: &str) -> Result<PathBuf> {
    let base = env::temp_dir();
    let pid = std::process::id();
    for _ in 0..128 {
        let nonce = TEMP_DIR_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let dir = base.join(format!("{prefix}-{nonce}-{ts}-{pid}"));
        match fs::create_dir(&dir) {
            Ok(()) => {
                register_temp_dir(&dir);
                return Ok(dir);
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err).with_context(|| format!("create temp dir {}", dir.display()));
            }
        }
    }
    bail!("failed to allocate unique temp dir for prefix `{prefix}`")
}

#[cfg(test)]
mod tests {
    use super::{
        DownloadDisplay, DownloadExtractionMode, DownloadFeedback, DownloadRange, GithubRelease,
        ParallelDownloadPlan, TEMP_DIR_PREFIX_DOWNLOAD, build_parallel_download_plan,
        download_from_url, latest_prerelease_version_for_channel,
        latest_stable_version_for_latest_release, latest_stable_version_for_tags,
        matched_temp_prefix, parse_content_range_total, parse_temp_dir_pid,
        prerelease_channel_matches, process_is_alive, retry_transient_http_operation,
        split_download_ranges,
    };
    use crate::command::za_config;
    use semver::Version;
    use std::{
        io::{self, Read, Write},
        net::{Shutdown, TcpListener, TcpStream},
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        thread,
        time::Duration,
    };

    #[test]
    fn github_release_metadata_accepts_responses_larger_than_eight_mib() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let payload = serde_json::json!([{
            "tag_name": "rust-v1.2.3",
            "draft": false,
            "prerelease": false,
            "assets": [],
            "body": "x".repeat(9 * 1024 * 1024)
        }])
        .to_string();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_http_request(&mut stream);
            write_http_response(
                &mut stream,
                200,
                &[],
                payload.as_bytes(),
                Some(payload.len()),
            );
        });
        let client = reqx::blocking::Client::builder(&base)
            .request_timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let releases: Vec<GithubRelease> = super::fetch_github_json_with_client(
            &client,
            "codex",
            "/releases?per_page=30&page=1",
            None,
        )
        .expect("large release metadata must be accepted");
        assert_eq!(releases[0].tag_name, "rust-v1.2.3");
        server.join().unwrap();
    }

    #[test]
    fn github_release_metadata_follows_repository_moves() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let location = format!("{base}/repositories/743011356/releases/latest");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            assert!(request.starts_with("GET /repos/old/repo/releases/latest "));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains(&format!("user-agent: {}", super::HTTP_USER_AGENT))
            );
            write_http_response(&mut stream, 301, &[("Location", location)], b"", Some(0));
            drop(stream);
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            assert!(request.starts_with("GET /repositories/743011356/releases/latest "));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains(&format!("user-agent: {}", super::HTTP_USER_AGENT))
            );
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer test-token")
            );
            let payload = br#"{"tag_name":"v1.2.3","draft":false,"prerelease":false,"assets":[]}"#;
            write_http_response(&mut stream, 200, &[], payload, Some(payload.len()));
        });
        let client = reqx::blocking::Client::builder(&base)
            .request_timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let release: GithubRelease = super::fetch_github_json_with_client(
            &client,
            "tcping",
            "/repos/old/repo/releases/latest",
            Some("test-token"),
        )
        .expect("repository redirect must be followed");
        assert_eq!(release.tag_name, "v1.2.3");
        server.join().unwrap();
    }

    #[test]
    fn prerelease_channel_matches_first_identifier_case_insensitively() {
        assert!(prerelease_channel_matches(
            &Version::parse("0.129.0-alpha.6").expect("semver"),
            "alpha"
        ));
        assert!(prerelease_channel_matches(
            &Version::parse("0.129.0-ALPHA.6").expect("semver"),
            "alpha"
        ));
        assert!(!prerelease_channel_matches(
            &Version::parse("0.129.0-beta.1").expect("semver"),
            "alpha"
        ));
        assert!(!prerelease_channel_matches(
            &Version::parse("0.129.0").expect("semver"),
            "alpha"
        ));
    }

    #[test]
    fn latest_prerelease_version_for_channel_prefers_highest_semver() {
        let releases = vec![
            github_release("rust-v0.129.0-alpha.6", true, false),
            github_release("rust-v0.129.0-alpha.10", true, false),
            github_release("rust-v0.130.0-beta.1", true, false),
            github_release("rust-v0.130.0", false, false),
            github_release("rust-v0.131.0-alpha.1", true, true),
        ];

        assert_eq!(
            latest_prerelease_version_for_channel(&releases, "rust-v", "alpha")
                .expect("latest alpha"),
            "0.129.0-alpha.10"
        );
    }

    #[test]
    fn latest_stable_version_for_tags_prefers_highest_matching_semver() {
        let releases = vec![
            github_release("rust-v0.131.0", false, false),
            github_release("rust-v0.132.0-alpha.1", true, false),
            github_release("rust-v0.132.0", false, false),
            github_release("rusty-v8-v147.4.0", true, false),
            github_release("rust-v0.133.0", false, true),
        ];

        assert_eq!(
            latest_stable_version_for_tags(&releases, "rust-v").expect("latest stable"),
            "0.132.0"
        );
    }

    #[test]
    fn latest_stable_version_accepts_two_segment_release_tags() {
        let releases = vec![
            github_release("v34.1", false, false),
            github_release("v35.1", false, false),
        ];

        assert_eq!(
            latest_stable_version_for_tags(&releases, "v").expect("latest stable"),
            "35.1"
        );
        assert_eq!(
            latest_stable_version_for_latest_release(&github_release("v35.1", false, false), "v")
                .expect("latest release"),
            "35.1"
        );
    }

    #[test]
    fn latest_stable_version_for_latest_release_requires_expected_prefix() {
        assert_eq!(
            latest_stable_version_for_latest_release(
                &github_release("rust-v0.132.0", false, false),
                "rust-v",
            )
            .expect("latest release"),
            "0.132.0"
        );
        assert!(
            latest_stable_version_for_latest_release(
                &github_release("v0.132.0", false, false),
                "rust-v",
            )
            .is_err()
        );
        assert!(
            latest_stable_version_for_latest_release(
                &github_release("rust-v0.132.0-alpha.1", true, false),
                "rust-v",
            )
            .is_err()
        );
    }

    fn github_release(tag_name: &str, prerelease: bool, draft: bool) -> GithubRelease {
        GithubRelease {
            tag_name: tag_name.to_string(),
            prerelease,
            draft,
            assets: Vec::new(),
        }
    }

    #[test]
    fn parse_temp_dir_pid_accepts_expected_layout() {
        let name = "za-tool-download-1-123456789-4242";
        assert_eq!(
            parse_temp_dir_pid(name, TEMP_DIR_PREFIX_DOWNLOAD),
            Some(4242)
        );
    }

    #[test]
    fn parse_temp_dir_pid_accepts_legacy_layout() {
        let name = "za-tool-download-123456789-4242";
        assert_eq!(
            parse_temp_dir_pid(name, TEMP_DIR_PREFIX_DOWNLOAD),
            Some(4242)
        );
    }

    #[test]
    fn parse_temp_dir_pid_rejects_unknown_layout() {
        assert_eq!(
            parse_temp_dir_pid("za-tool-download", "za-tool-download"),
            None
        );
        assert_eq!(
            parse_temp_dir_pid("za-tool-download-abc-xyz", "za-tool-download"),
            None
        );
        assert_eq!(
            parse_temp_dir_pid("za-tool-download-other-123", "za-tool-download"),
            None
        );
        assert_eq!(
            parse_temp_dir_pid("za-tool-download-1-other-123", "za-tool-download"),
            None
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_is_alive_detects_current_process_on_linux() {
        assert!(process_is_alive(std::process::id()));
        assert!(!process_is_alive(0));
    }

    #[test]
    fn matched_temp_prefix_finds_known_prefixes() {
        assert_eq!(
            matched_temp_prefix("za-tool-download-1-2"),
            Some("za-tool-download")
        );
        assert_eq!(matched_temp_prefix("za-other-1-2"), None);
    }

    #[test]
    fn parse_content_range_total_reads_total_bytes() {
        assert_eq!(
            parse_content_range_total("bytes 0-0/2700000"),
            Some(2_700_000)
        );
        assert_eq!(parse_content_range_total("bytes 10-19/*"), None);
        assert_eq!(parse_content_range_total("invalid"), None);
    }

    #[test]
    fn parallel_download_plan_requires_range_support_and_size() {
        assert_eq!(build_parallel_download_plan(1_048_576, true), None);
        assert_eq!(build_parallel_download_plan(8_388_608, false), None);
        assert_eq!(
            build_parallel_download_plan(8_388_608, true),
            Some(ParallelDownloadPlan {
                total_bytes: 8_388_608,
                parts: 4,
            })
        );
    }

    #[test]
    fn split_download_ranges_covers_full_payload_without_gaps() {
        let ranges = split_download_ranges(ParallelDownloadPlan {
            total_bytes: 10,
            parts: 3,
        });
        assert_eq!(
            ranges,
            vec![
                DownloadRange { start: 0, end: 3 },
                DownloadRange { start: 4, end: 6 },
                DownloadRange { start: 7, end: 9 },
            ]
        );
    }

    #[test]
    fn retry_reporter_captures_retries_and_restores_after_unwind() {
        let messages = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&messages);
        let reporter: super::RetryReporter = Arc::new(move |message| {
            captured.lock().unwrap().push(message);
        });
        super::with_retry_reporter(Some(reporter), || {
            // A worker's reporter must not leak into another worker.
            assert!(
                !thread::spawn(|| super::report_retry("other worker".into()))
                    .join()
                    .unwrap()
            );
            let result = std::panic::catch_unwind(|| {
                super::with_retry_reporter(None, || {
                    assert!(!super::report_retry("disabled".into()));
                    panic!("simulate task failure");
                });
            });
            assert!(result.is_err());
            let mut attempts = 0;
            let value = retry_transient_http_operation("source", "metadata", || {
                attempts += 1;
                if attempts < 3 {
                    Err(anyhow::Error::new(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "simulated timeout",
                    )))
                } else {
                    Ok(42)
                }
            })
            .unwrap();
            assert_eq!(value, 42);
        });
        assert!(!super::report_retry("outside scope".into()));
        let messages = messages.lock().unwrap();
        assert_eq!(messages.len(), 2);
        assert!(messages[0].starts_with("retry 2/3"));
        assert!(messages[1].starts_with("retry 3/3"));
        assert!(messages.iter().all(|message| message.contains("timed out")));
    }

    #[test]
    fn retry_transient_http_operation_retries_unexpected_eof_once() {
        let attempts = AtomicUsize::new(0);
        let value = retry_transient_http_operation("source", "test metadata", || {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                return Err(anyhow::Error::new(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "simulated eof",
                )));
            }
            Ok(42usize)
        })
        .expect("retry should succeed on second attempt");

        assert_eq!(value, 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn narrow_download_progress_clears_attempts_and_writes_the_full_final_line() {
        let term = indicatif::InMemoryTerm::new(8, 35);
        let start = || {
            super::new_download_progress_bar(
                indicatif::ProgressDrawTarget::term_like(Box::new(term.clone())),
                Some(524_288),
            )
            .unwrap()
        };
        let first = start();
        assert_eq!(term.contents().lines().count(), 1);
        drop(first);
        assert!(term.contents().trim().is_empty());
        let retry = start();
        let mut output = Vec::new();
        super::DownloadProgressReport {
            downloaded: 524_288,
            total_bytes: Some(524_288),
            elapsed: Duration::from_secs(2),
            finished: true,
            progress_bar: Some(&retry),
            print_line: true,
            progress_reporter: None,
        }
        .write_line(&mut output)
        .unwrap();
        drop(retry);
        assert!(term.contents().trim().is_empty());
        assert_eq!(
            output,
            b"download: 512.0 KiB / 512.0 KiB (100.0%, 256.0 KiB/s)\n"
        );
    }

    #[test]
    fn download_from_url_retries_single_stream_after_transient_read_failure() {
        let payload = Arc::new(vec![b'y'; 512 * 1024]);
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind test listener");
        listener
            .set_nonblocking(true)
            .expect("configure nonblocking listener");
        let addr = listener.local_addr().expect("listener addr");
        let stop = Arc::new(AtomicBool::new(false));
        let failed_once = Arc::new(AtomicBool::new(false));
        let server_payload = Arc::clone(&payload);
        let server_stop = Arc::clone(&stop);
        let server_failed_once = Arc::clone(&failed_once);

        let server = thread::spawn(move || {
            while !server_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => handle_single_stream_retry_request(
                        stream,
                        Arc::clone(&server_payload),
                        Arc::clone(&server_failed_once),
                    ),
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        let url = format!("http://127.0.0.1:{}/artifact.tar.gz", addr.port());
        let pull = download_from_url(
            &url,
            None,
            za_config::ProxyScope::Tool,
            DownloadExtractionMode::KeepArchive,
            DownloadFeedback::new(DownloadDisplay::Detailed, None),
        )
        .expect("single-stream retry should succeed");
        let downloaded = std::fs::read(&pull.path).expect("read downloaded asset");
        assert_eq!(downloaded, *payload);

        drop(pull);
        stop.store(true, Ordering::SeqCst);
        server.join().expect("join test server");
    }

    #[test]
    fn download_from_url_resumes_interrupted_range() {
        check_range_download(false);
    }

    #[test]
    fn download_from_url_falls_back_when_server_ignores_range_after_probe() {
        check_range_download(true);
    }

    fn check_range_download(ignore_range: bool) {
        let size = if ignore_range {
            2_400_000
        } else {
            super::RANGE_REQUEST_BYTES as usize * 4 + 16
        };
        let payload = Arc::new((0..size).map(|n| (n % 251) as u8).collect::<Vec<_>>());
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind test listener");
        listener
            .set_nonblocking(true)
            .expect("configure nonblocking listener");
        let addr = listener.local_addr().expect("listener addr");
        let stop = Arc::new(AtomicBool::new(false));
        let server_payload = Arc::clone(&payload);
        let server_stop = Arc::clone(&stop);

        let server = thread::spawn(move || {
            let mut interrupted = None;
            let mut resumed = false;
            let mut full_requests = 0;
            while !server_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let request = read_http_request(&mut stream);
                        let Some((start, end)) = parse_http_range_header(&request) else {
                            assert!(ignore_range);
                            full_requests += 1;
                            write_http_response(&mut stream, 200, &[], &server_payload, Some(size));
                            continue;
                        };
                        if ignore_range && start != end {
                            write_http_response(&mut stream, 200, &[], b"", Some(0));
                            continue;
                        }
                        let body = &server_payload[start as usize..=end as usize];
                        assert!(body.len() <= super::RANGE_REQUEST_BYTES as usize);
                        let mut bytes = body;
                        if start != end && interrupted.is_none() {
                            bytes = &body[..body.len() / 2];
                            interrupted = Some((start + bytes.len() as u64, end));
                        } else if let Some((offset, failed_end)) = interrupted
                            && end == failed_end
                        {
                            assert_eq!(start, offset);
                            resumed = true;
                        }
                        write_http_response(
                            &mut stream,
                            206,
                            &[("Content-Range", format!("bytes {start}-{end}/{size}"))],
                            bytes,
                            Some(body.len()),
                        );
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
            assert_eq!(full_requests, usize::from(ignore_range));
            assert_eq!(resumed, !ignore_range);
        });

        let url = format!("http://127.0.0.1:{}/artifact.tar.gz", addr.port());
        let pull = download_from_url(
            &url,
            None,
            za_config::ProxyScope::Tool,
            DownloadExtractionMode::KeepArchive,
            DownloadFeedback::new(DownloadDisplay::Detailed, None),
        )
        .expect("download succeeds");
        let downloaded = std::fs::read(&pull.path).expect("read downloaded asset");
        assert!(downloaded == *payload);

        drop(pull);
        stop.store(true, Ordering::SeqCst);
        server.join().expect("join test server");
    }

    fn handle_single_stream_retry_request(
        mut stream: TcpStream,
        payload: Arc<Vec<u8>>,
        failed_once: Arc<AtomicBool>,
    ) {
        let request = read_http_request(&mut stream);
        if parse_http_range_header(&request).is_some() {
            write_http_response(
                &mut stream,
                200,
                &[],
                payload.as_slice(),
                Some(payload.len()),
            );
            return;
        }

        if !failed_once.swap(true, Ordering::SeqCst) {
            let truncated_len = payload.len() / 2;
            write_http_response(
                &mut stream,
                200,
                &[],
                &payload[..truncated_len],
                Some(payload.len()),
            );
            let _ = stream.shutdown(Shutdown::Both);
        } else {
            write_http_response(
                &mut stream,
                200,
                &[],
                payload.as_slice(),
                Some(payload.len()),
            );
        }
    }

    fn read_http_request(stream: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0_u8; 1024];
        loop {
            let read = stream.read(&mut chunk).expect("read request");
            if read == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..read]);
            if buf.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8(buf).expect("utf8 request")
    }

    fn parse_http_range_header(request: &str) -> Option<(u64, u64)> {
        for line in request.lines() {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if !name.eq_ignore_ascii_case("range") {
                continue;
            }
            let value = value.trim();
            let bytes = value.strip_prefix("bytes=")?;
            let (start, end) = bytes.split_once('-')?;
            return Some((start.parse().ok()?, end.parse().ok()?));
        }
        None
    }

    fn write_http_response(
        stream: &mut TcpStream,
        status: u16,
        headers: &[(&str, String)],
        body: &[u8],
        content_length: Option<usize>,
    ) {
        let reason = match status {
            200 => "OK",
            206 => "Partial Content",
            _ => "Test Response",
        };
        write!(stream, "HTTP/1.1 {status} {reason}\r\n").expect("write status line");
        if let Some(content_length) = content_length {
            write!(stream, "Content-Length: {content_length}\r\n").expect("write content length");
        }
        write!(stream, "Connection: close\r\n").expect("write connection header");
        for (name, value) in headers {
            write!(stream, "{name}: {value}\r\n").expect("write header");
        }
        write!(stream, "\r\n").expect("write header terminator");
        stream.write_all(body).expect("write body");
        stream.flush().expect("flush response");
    }
}
