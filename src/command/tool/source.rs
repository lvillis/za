use super::*;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
};

const TEMP_DIR_PREFIX_DOWNLOAD: &str = "za-tool-download";
const TEMP_DIR_PREFIX_CARGO_INSTALL: &str = "za-tool-cargo-install";
const TEMP_DIR_PREFIXES: [&str; 2] = [TEMP_DIR_PREFIX_DOWNLOAD, TEMP_DIR_PREFIX_CARGO_INSTALL];
const DOWNLOAD_READ_CHUNK_SIZE: usize = 64 * 1024;
const PARALLEL_DOWNLOAD_MIN_BYTES: u64 = 2 * 1024 * 1024;
const PARALLEL_DOWNLOAD_MIN_PART_BYTES: u64 = 1024 * 1024;
const PARALLEL_DOWNLOAD_MAX_PARTS: usize = 4;

static ACTIVE_TEMP_DIRS: LazyLock<Mutex<HashSet<PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));
static TEMP_DIR_NONCE: AtomicU64 = AtomicU64::new(0);

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

enum ParallelDownloadError {
    Unsupported(anyhow::Error),
    Failed(anyhow::Error),
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
    for entry in entries.flatten() {
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

fn matched_temp_prefix(name: &str) -> Option<&'static str> {
    TEMP_DIR_PREFIXES
        .iter()
        .copied()
        .find(|prefix| name.starts_with(prefix))
}

fn parse_temp_dir_pid(name: &str, prefix: &str) -> Option<u32> {
    let rest = name.strip_prefix(prefix)?.strip_prefix('-')?;
    let (_, pid) = rest.rsplit_once('-')?;
    pid.parse::<u32>().ok()
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    false
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
        bail!(
            "latest version resolution is not defined for `{name}`. supported tools: {}",
            supported_tool_names_csv()
        );
    };
    let Some(release) = policy.github_release else {
        bail!("latest version resolution is not defined for `{name}`");
    };
    fetch_latest_version_from_github_release(release, proxy_scope)
}

pub(super) fn resolve_install_source(
    tool: &ToolRef,
    proxy_scope: za_config::ProxyScope,
) -> Result<PullSource> {
    ensure_not_interrupted()?;

    let Some(policy) = find_tool_policy(&tool.name) else {
        bail!(
            "unsupported tool `{}`: no built-in source policy. currently supported: {}",
            tool.name,
            supported_tool_names_csv()
        );
    };

    let mut errors = Vec::new();

    if let Some(release) = policy.github_release {
        match download_from_github_release(tool, release, proxy_scope) {
            Ok(src) => return Ok(src),
            Err(err) => errors.push(format!("github release: {err:#}")),
        }
    }
    if let Some(package) = policy.cargo_fallback_package {
        match install_from_cargo_package(tool, package) {
            Ok(src) => return Ok(src),
            Err(err) => errors.push(format!("cargo install: {err:#}")),
        }
    }

    bail!(
        "failed to resolve source for `{}` via automatic policies:\n- {}",
        tool.name,
        errors.join("\n- ")
    )
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<GithubReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubReleaseAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
}

pub(super) fn fetch_latest_version_from_github_release(
    policy: GithubReleasePolicy,
    proxy_scope: za_config::ProxyScope,
) -> Result<String> {
    let release = fetch_github_release(
        policy.project_label,
        &format!("/repos/{}/{}/releases/latest", policy.owner, policy.repo),
        proxy_scope,
    )?;
    parse_release_version(&release.tag_name, policy.tag_prefix)
}

fn download_from_github_release(
    tool: &ToolRef,
    policy: GithubReleasePolicy,
    proxy_scope: za_config::ProxyScope,
) -> Result<PullSource> {
    let version = normalize_version(&tool.version);
    let expected_asset_name = (policy.expected_asset_name)(&version)?;
    let tag = format!("{}{}", policy.tag_prefix, version);
    let path = format!(
        "/repos/{}/{}/releases/tags/{tag}",
        policy.owner, policy.repo
    );
    let release = fetch_github_release(policy.project_label, &path, proxy_scope)?;
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == expected_asset_name)
        .ok_or_else(|| {
            anyhow!("release `{tag}` does not contain expected asset `{expected_asset_name}`")
        })?;
    let expected_sha256 = asset
        .digest
        .as_deref()
        .and_then(parse_github_sha256_digest)
        .ok_or_else(|| anyhow!("release asset `{}` missing valid sha256 digest", asset.name))?;

    download_from_url(
        tool,
        &asset.browser_download_url,
        Some(&expected_sha256),
        proxy_scope,
    )
}

fn install_from_cargo_package(tool: &ToolRef, package: &str) -> Result<PullSource> {
    ensure_not_interrupted()?;
    let install_root = unique_temp_dir(TEMP_DIR_PREFIX_CARGO_INSTALL)?;
    let run = (|| -> Result<PullSource> {
        let root_arg = install_root.to_string_lossy().to_string();
        let version = normalize_version(&tool.version);

        let output = Command::new("cargo")
            .arg("install")
            .arg("--locked")
            .arg("--version")
            .arg(&version)
            .arg(package)
            .arg("--root")
            .arg(&root_arg)
            .output()
            .context("run `cargo install`")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("`cargo install` failed: {}", stderr.trim());
        }
        ensure_not_interrupted()?;

        let bin_dir = install_root.join("bin");
        let mut candidates = command_candidates(&tool.name);
        if !candidates.iter().any(|c| c == "codex") {
            candidates.push("codex".to_string());
        }

        for candidate in candidates {
            let p = bin_dir.join(&candidate);
            if is_executable_file(&p) {
                return Ok(PullSource::temp(
                    SOURCE_KIND_CARGO_INSTALL,
                    p,
                    format!("cargo install {package}"),
                    install_root.clone(),
                ));
            }
        }

        let mut files = Vec::new();
        collect_files_recursive(&bin_dir, &mut files)?;
        if files.len() == 1 {
            return Ok(PullSource::temp(
                SOURCE_KIND_CARGO_INSTALL,
                files.remove(0),
                format!("cargo install {package}"),
                install_root.clone(),
            ));
        }

        bail!(
            "could not determine installed executable in {}",
            bin_dir.display()
        )
    })();

    if run.is_err() {
        unregister_temp_dir(&install_root);
        let _ = fs::remove_dir_all(&install_root);
    }
    run
}

const HTTPS_PROXY_ENV_KEYS: [&str; 6] = [
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "HTTP_PROXY",
    "http_proxy",
];
const HTTP_PROXY_ENV_KEYS: [&str; 4] = ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"];

pub(super) fn proxy_env_keys_for_scheme(scheme: &str) -> &'static [&'static str] {
    if scheme.eq_ignore_ascii_case("https") {
        &HTTPS_PROXY_ENV_KEYS
    } else {
        &HTTP_PROXY_ENV_KEYS
    }
}

fn first_env_value(names: &[&str]) -> Option<(String, String)> {
    for name in names {
        let Ok(value) = env::var(name) else {
            continue;
        };
        let value = value.trim();
        if !value.is_empty() {
            return Some(((*name).to_string(), value.to_string()));
        }
    }
    None
}

pub(super) fn split_no_proxy_rules(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|rule| !rule.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn apply_proxy_with_scope(
    mut builder: ClientBuilder,
    scheme: &str,
    proxy_scope: za_config::ProxyScope,
) -> Result<ClientBuilder> {
    let overrides = za_config::load_proxy_overrides(proxy_scope)?;
    let proxy_value = if scheme.eq_ignore_ascii_case("https") {
        overrides
            .https_proxy
            .clone()
            .or_else(|| overrides.all_proxy.clone())
            .or_else(|| overrides.http_proxy.clone())
    } else {
        overrides
            .http_proxy
            .clone()
            .or_else(|| overrides.all_proxy.clone())
            .or_else(|| overrides.https_proxy.clone())
    };

    let (proxy_var, proxy_value) = if let Some(value) = proxy_value {
        ("config".to_string(), value)
    } else if let Some((name, value)) = first_env_value(proxy_env_keys_for_scheme(scheme)) {
        (name, value)
    } else {
        return Ok(builder);
    };

    let proxy_uri = proxy_value
        .parse()
        .with_context(|| format!("invalid proxy URI in `{proxy_var}`"))?;
    builder = builder.http_proxy(proxy_uri);

    let no_proxy_raw = overrides
        .no_proxy
        .clone()
        .or_else(|| first_env_value(&["NO_PROXY", "no_proxy"]).map(|(_, value)| value));
    if let Some(no_proxy_raw) = no_proxy_raw {
        let rules = split_no_proxy_rules(&no_proxy_raw);
        if !rules.is_empty() {
            builder = builder
                .try_no_proxy(rules)
                .context("invalid `NO_PROXY`/`no_proxy` rules")?;
        }
    }

    Ok(builder)
}

fn build_http_client(
    base_url: &str,
    client_name: &str,
    follow_redirects: bool,
    proxy_scope: za_config::ProxyScope,
) -> Result<Client> {
    let mut builder = Client::builder(base_url)
        .profile(if follow_redirects {
            ClientProfile::HighThroughput
        } else {
            ClientProfile::StandardSdk
        })
        .request_timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .total_timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .retry_policy(RetryPolicy::disabled())
        .client_name(client_name);
    if follow_redirects {
        builder = builder.redirect_policy(RedirectPolicy::follow());
    }
    let scheme = base_url
        .split_once("://")
        .map(|(s, _)| s)
        .unwrap_or("https");
    builder = apply_proxy_with_scope(builder, scheme, proxy_scope)
        .with_context(|| format!("configure HTTP client proxy for `{base_url}`"))?;
    builder
        .build()
        .with_context(|| format!("build HTTP client for `{base_url}`"))
}

fn fetch_github_release(
    project_label: &str,
    path: &str,
    proxy_scope: za_config::ProxyScope,
) -> Result<GithubRelease> {
    ensure_not_interrupted()?;

    let client = build_http_client(GITHUB_API_BASE, "za-tool-manager", false, proxy_scope)
        .context("build GitHub API client")?;
    let github_token = resolve_github_token()?;

    let mut req = client.get(path);
    req = req
        .try_header("user-agent", HTTP_USER_AGENT)
        .context("set GitHub user-agent")?;
    req = req
        .try_header("accept", "application/vnd.github+json")
        .context("set GitHub accept header")?;
    if let Some(token) = github_token.as_deref() {
        req = req
            .try_header("authorization", &format!("Bearer {token}"))
            .context("set GitHub authorization header")?;
    }

    let response = req
        .send_response()
        .with_context(|| format!("query {project_label} release metadata ({PROXY_HINT})"))?;
    let status = response.status();
    if !status.is_success() {
        bail!(
            "query {project_label} release metadata failed: status {} body {}",
            status,
            truncate_for_log(&response.text_lossy(), 200)
        );
    }
    response
        .json::<GithubRelease>()
        .with_context(|| format!("parse {project_label} release JSON"))
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
    tool: &ToolRef,
    url: &str,
    expected_sha256: Option<&str>,
    proxy_scope: za_config::ProxyScope,
) -> Result<PullSource> {
    ensure_not_interrupted()?;
    let download_root = unique_temp_dir(TEMP_DIR_PREFIX_DOWNLOAD)?;
    let run = (|| -> Result<PullSource> {
        let url_parts = parse_url_parts(url)?;
        let asset_name = url_parts.file_name.clone();
        let asset_path = download_root.join(&asset_name);

        let client = build_http_client(&url_parts.base_url, "za-tool-manager", true, proxy_scope)
            .context("build HTTP client")?;
        let probe = probe_parallel_download_support(&client, &url_parts.path_and_query).unwrap_or(
            DownloadProbe {
                total_bytes: 0,
                range_supported: false,
            },
        );
        let total_bytes = (probe.total_bytes > 0).then_some(probe.total_bytes);

        if let Some(plan) = build_parallel_download_plan(probe.total_bytes, probe.range_supported) {
            match download_to_path_parallel(&url_parts, url, &asset_path, proxy_scope, plan) {
                Ok(()) => {}
                Err(ParallelDownloadError::Unsupported(err)) => {
                    eprintln!(
                        "↩️  Parallel download unavailable for `{}`; falling back to single stream ({err:#})",
                        asset_name
                    );
                    download_to_path_single(
                        &client,
                        &url_parts.path_and_query,
                        url,
                        &asset_path,
                        total_bytes,
                    )?;
                }
                Err(ParallelDownloadError::Failed(err)) => return Err(err),
            }
        } else {
            download_to_path_single(
                &client,
                &url_parts.path_and_query,
                url,
                &asset_path,
                total_bytes,
            )?;
        }
        ensure_not_interrupted()?;

        if let Some(expected_sha256) = expected_sha256 {
            verify_sha256_file(&asset_path, expected_sha256)?;
        }

        let executable_path = if is_tar_gz_asset(&asset_name) {
            extract_tar_gz_executable(tool, &asset_path, &download_root)?
        } else {
            asset_path
        };

        Ok(PullSource::temp(
            SOURCE_KIND_DOWNLOAD,
            executable_path,
            match expected_sha256 {
                Some(expected) => format!("URL {url} (sha256={expected})"),
                None => format!("URL {url}"),
            },
            download_root.clone(),
        ))
    })();

    if run.is_err() {
        unregister_temp_dir(&download_root);
        let _ = fs::remove_dir_all(&download_root);
    }
    run
}

fn build_download_request<'a>(
    client: &'a Client,
    path_and_query: &str,
    range: Option<DownloadRange>,
) -> Result<reqx::blocking::RequestBuilder<'a>> {
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
    let resp = req.send_response_stream().ok()?;
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
) -> Result<()> {
    let req = build_download_request(client, path_and_query, None)?;
    let mut resp = req
        .send_response_stream()
        .with_context(|| format!("download from `{url}` ({PROXY_HINT})"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp
            .into_text_lossy_limited(16 * 1024)
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
    let mut chunk = [0_u8; DOWNLOAD_READ_CHUNK_SIZE];
    let mut downloaded = 0_u64;
    let start = Instant::now();
    let mut last_report = Instant::now();
    let use_tty_line = io::stderr().is_terminal();
    if use_tty_line {
        eprint!(
            "\r{}",
            render_download_progress(downloaded, total_bytes, start.elapsed())
        );
        let _ = io::stderr().flush();
    }
    loop {
        ensure_not_interrupted()?;
        let read = resp
            .read_chunk(&mut chunk)
            .with_context(|| format!("read bytes from `{url}`"))?;
        if read == 0 {
            break;
        }
        out.write_all(&chunk[..read])
            .with_context(|| format!("write downloaded file {}", asset_path.display()))?;
        downloaded = downloaded.saturating_add(read as u64);
        report_download_progress(
            downloaded,
            total_bytes,
            start.elapsed(),
            &mut last_report,
            false,
            use_tty_line,
        );
    }
    report_download_progress(
        downloaded,
        total_bytes,
        start.elapsed(),
        &mut last_report,
        true,
        use_tty_line,
    );
    out.flush()
        .with_context(|| format!("flush downloaded file {}", asset_path.display()))?;
    Ok(())
}

fn download_to_path_parallel(
    url_parts: &UrlParts,
    url: &str,
    asset_path: &Path,
    proxy_scope: za_config::ProxyScope,
    plan: ParallelDownloadPlan,
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
    let use_tty_line = io::stderr().is_terminal();

    let reporter = spawn_parallel_download_reporter(
        Arc::clone(&progress),
        Arc::clone(&reporter_stop),
        total_bytes,
        start,
        use_tty_line,
    );

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
                    progress,
                );
                if result.is_err() {
                    reporter_stop.store(true, Ordering::SeqCst);
                }
                result.map(|_| (index, part_path))
            })
        })
        .collect::<Vec<_>>();

    let mut ordered_parts = vec![PathBuf::new(); part_paths.len()];
    for handle in handles {
        match handle.join() {
            Ok(Ok((index, part_path))) => ordered_parts[index] = part_path,
            Ok(Err(err)) => {
                reporter_stop.store(true, Ordering::SeqCst);
                let _ = reporter.join();
                cleanup_parallel_part_files(&part_paths);
                return Err(err);
            }
            Err(_) => {
                reporter_stop.store(true, Ordering::SeqCst);
                let _ = reporter.join();
                cleanup_parallel_part_files(&part_paths);
                return Err(ParallelDownloadError::Failed(anyhow!(
                    "parallel download worker panicked for `{url}`"
                )));
            }
        }
    }

    reporter_stop.store(true, Ordering::SeqCst);
    let _ = reporter.join();
    let mut last_report = Instant::now();
    report_download_progress(
        progress.load(Ordering::SeqCst),
        total_bytes,
        start.elapsed(),
        &mut last_report,
        true,
        use_tty_line,
    );

    merge_parallel_part_files(&ordered_parts, asset_path).map_err(ParallelDownloadError::Failed)?;
    cleanup_parallel_part_files(&part_paths);
    Ok(())
}

fn spawn_parallel_download_reporter(
    progress: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    total_bytes: Option<u64>,
    start: Instant,
    tty_line: bool,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut last_report = Instant::now();
        if tty_line {
            eprint!(
                "\r{}",
                render_download_progress(
                    progress.load(Ordering::SeqCst),
                    total_bytes,
                    start.elapsed()
                )
            );
            let _ = io::stderr().flush();
        }
        while !stop.load(Ordering::SeqCst) {
            report_download_progress(
                progress.load(Ordering::SeqCst),
                total_bytes,
                start.elapsed(),
                &mut last_report,
                false,
                tty_line,
            );
            thread::sleep(Duration::from_millis(150));
        }
    })
}

fn download_range_part(
    base_url: &str,
    path_and_query: &str,
    range: DownloadRange,
    part_path: &Path,
    proxy_scope: za_config::ProxyScope,
    progress: Arc<AtomicU64>,
) -> Result<(), ParallelDownloadError> {
    ensure_not_interrupted().map_err(ParallelDownloadError::Failed)?;
    let client = build_http_client(base_url, "za-tool-manager", true, proxy_scope)
        .map_err(ParallelDownloadError::Failed)?;
    let req = build_download_request(&client, path_and_query, Some(range))
        .map_err(ParallelDownloadError::Failed)?;
    let mut resp = req.send_response_stream().map_err(|err| {
        ParallelDownloadError::Failed(anyhow!(err).context(format!(
            "download byte range {}-{} from `{}` ({PROXY_HINT})",
            range.start, range.end, path_and_query
        )))
    })?;
    if resp.status() != 206 {
        let status = resp.status();
        let body = resp
            .into_text_lossy_limited(1024)
            .unwrap_or_else(|_| "<body unavailable>".to_string());
        return Err(ParallelDownloadError::Unsupported(anyhow!(
            "range request {}-{} returned status {} body {}",
            range.start,
            range.end,
            status,
            truncate_for_log(&body, 120)
        )));
    }

    let mut out = File::create(part_path)
        .with_context(|| format!("create partial file {}", part_path.display()))
        .map_err(ParallelDownloadError::Failed)?;
    let mut chunk = [0_u8; DOWNLOAD_READ_CHUNK_SIZE];
    let mut written = 0_u64;
    let expected = range.end.saturating_sub(range.start).saturating_add(1);
    loop {
        ensure_not_interrupted().map_err(ParallelDownloadError::Failed)?;
        let read = resp
            .read_chunk(&mut chunk)
            .with_context(|| {
                format!(
                    "read partial bytes {}-{} from `{}`",
                    range.start, range.end, path_and_query
                )
            })
            .map_err(ParallelDownloadError::Failed)?;
        if read == 0 {
            break;
        }
        out.write_all(&chunk[..read])
            .with_context(|| format!("write partial file {}", part_path.display()))
            .map_err(ParallelDownloadError::Failed)?;
        written = written.saturating_add(read as u64);
        progress.fetch_add(read as u64, Ordering::Relaxed);
    }
    out.flush()
        .with_context(|| format!("flush partial file {}", part_path.display()))
        .map_err(ParallelDownloadError::Failed)?;
    if written != expected {
        return Err(ParallelDownloadError::Unsupported(anyhow!(
            "range request {}-{} returned {} bytes, expected {}",
            range.start,
            range.end,
            written,
            expected
        )));
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

pub(super) fn render_download_progress(
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
                "⬇️  Downloaded {} / {} ({pct:.1}%, {}/s)",
                format_bytes_u64(downloaded),
                format_bytes_u64(total),
                format_bytes_f64(rate)
            )
        }
        _ => format!(
            "⬇️  Downloaded {} ({}/s)",
            format_bytes_u64(downloaded),
            format_bytes_f64(rate)
        ),
    }
}

fn report_download_progress(
    downloaded: u64,
    total_bytes: Option<u64>,
    elapsed: Duration,
    last_report: &mut Instant,
    force: bool,
    tty_line: bool,
) {
    let now = Instant::now();
    if !force && now.duration_since(*last_report) < Duration::from_secs(1) {
        return;
    }
    let line = render_download_progress(downloaded, total_bytes, elapsed);
    if tty_line {
        if force {
            eprint!("\r{line}\n");
        } else {
            eprint!("\r{line}");
            let _ = io::stderr().flush();
        }
    } else {
        eprintln!("{line}");
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

pub(super) fn is_tar_gz_asset(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".tar.gz") || lower.ends_with(".tgz")
}

fn extract_tar_gz_executable(tool: &ToolRef, archive_path: &Path, root: &Path) -> Result<PathBuf> {
    let unpack_dir = root.join("unpack");
    fs::create_dir_all(&unpack_dir)?;

    let file = File::open(archive_path)
        .with_context(|| format!("open archive {}", archive_path.display()))?;
    let gz = GzDecoder::new(file);
    let mut archive = Archive::new(gz);
    archive
        .unpack(&unpack_dir)
        .with_context(|| format!("extract archive {}", archive_path.display()))?;

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
        DownloadRange, ParallelDownloadPlan, TEMP_DIR_PREFIX_DOWNLOAD,
        build_parallel_download_plan, matched_temp_prefix, parse_content_range_total,
        parse_temp_dir_pid, split_download_ranges,
    };

    #[test]
    fn parse_temp_dir_pid_accepts_expected_layout() {
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
    }

    #[test]
    fn matched_temp_prefix_finds_known_prefixes() {
        assert_eq!(
            matched_temp_prefix("za-tool-download-1-2"),
            Some("za-tool-download")
        );
        assert_eq!(
            matched_temp_prefix("za-tool-cargo-install-1-2"),
            Some("za-tool-cargo-install")
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
}
