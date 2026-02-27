use super::*;

pub(super) fn resolve_requested_version(
    name: &str,
    requested_version: Option<&str>,
) -> Result<String> {
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
    fetch_latest_version_from_github_release(release)
}

pub(super) fn resolve_install_source(tool: &ToolRef) -> Result<PullSource> {
    let Some(policy) = find_tool_policy(&tool.name) else {
        bail!(
            "unsupported tool `{}`: no built-in source policy. currently supported: {}",
            tool.name,
            supported_tool_names_csv()
        );
    };

    let mut errors = Vec::new();

    if let Some(release) = policy.github_release {
        match download_from_github_release(tool, release) {
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
) -> Result<String> {
    let release = fetch_github_release(
        policy.project_label,
        &format!("/repos/{}/{}/releases/latest", policy.owner, policy.repo),
    )?;
    parse_release_version(&release.tag_name, policy.tag_prefix)
}

fn download_from_github_release(tool: &ToolRef, policy: GithubReleasePolicy) -> Result<PullSource> {
    let version = normalize_version(&tool.version);
    let expected_asset_name = (policy.expected_asset_name)(&version)?;
    let tag = format!("{}{}", policy.tag_prefix, version);
    let path = format!(
        "/repos/{}/{}/releases/tags/{tag}",
        policy.owner, policy.repo
    );
    let release = fetch_github_release(policy.project_label, &path)?;
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

    download_from_url(tool, &asset.browser_download_url, Some(&expected_sha256))
}

fn install_from_cargo_package(tool: &ToolRef, package: &str) -> Result<PullSource> {
    let install_root = unique_temp_dir("za-tool-cargo-install")?;
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

    let bin_dir = install_root.join("bin");
    let mut candidates = command_candidates(&tool.name);
    if !candidates.iter().any(|c| c == "codex") {
        candidates.push("codex".to_string());
    }

    for candidate in candidates {
        let p = bin_dir.join(&candidate);
        if is_executable_file(&p) {
            return Ok(PullSource::temp(
                p,
                format!("cargo install {package}"),
                install_root,
            ));
        }
    }

    let mut files = Vec::new();
    collect_files_recursive(&bin_dir, &mut files)?;
    if files.len() == 1 {
        return Ok(PullSource::temp(
            files.remove(0),
            format!("cargo install {package}"),
            install_root,
        ));
    }

    bail!(
        "could not determine installed executable in {}",
        bin_dir.display()
    )
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

fn apply_proxy_from_env(mut builder: ClientBuilder, scheme: &str) -> Result<ClientBuilder> {
    let Some((proxy_var, proxy_value)) = first_env_value(proxy_env_keys_for_scheme(scheme)) else {
        return Ok(builder);
    };

    let proxy_uri = proxy_value
        .parse()
        .with_context(|| format!("invalid proxy URI in `{proxy_var}`"))?;
    builder = builder.http_proxy(proxy_uri);

    if let Some((_, no_proxy_raw)) = first_env_value(&["NO_PROXY", "no_proxy"]) {
        let rules = split_no_proxy_rules(&no_proxy_raw);
        if !rules.is_empty() {
            builder = builder
                .try_no_proxy(rules)
                .context("invalid `NO_PROXY`/`no_proxy` rules")?;
        }
    }

    Ok(builder)
}

fn build_http_client(base_url: &str, client_name: &str, follow_redirects: bool) -> Result<Client> {
    let mut builder = Client::builder(base_url)
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
    builder = apply_proxy_from_env(builder, scheme)
        .with_context(|| format!("configure HTTP client proxy for `{base_url}`"))?;
    builder
        .build()
        .with_context(|| format!("build HTTP client for `{base_url}`"))
}

fn fetch_github_release(project_label: &str, path: &str) -> Result<GithubRelease> {
    let client = build_http_client(GITHUB_API_BASE, "za-tool-manager", false)
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
        .send_with_status()
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
) -> Result<PullSource> {
    let download_root = unique_temp_dir("za-tool-download")?;
    let url_parts = parse_url_parts(url)?;
    let asset_name = url_parts.file_name.clone();
    let asset_path = download_root.join(&asset_name);

    let client = build_http_client(&url_parts.base_url, "za-tool-manager", true)
        .context("build HTTP client")?;
    let mut req = client.get(url_parts.path_and_query);
    req = req
        .try_header("user-agent", HTTP_USER_AGENT)
        .context("set download user-agent")?;
    let mut resp = req
        .send_stream()
        .with_context(|| format!("download from `{url}` ({PROXY_HINT})"))?;
    let total_bytes = resp
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());

    let mut out = File::create(&asset_path)
        .with_context(|| format!("create downloaded file {}", asset_path.display()))?;
    let mut chunk = [0_u8; 64 * 1024];
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

    if let Some(expected_sha256) = expected_sha256 {
        verify_sha256_file(&asset_path, expected_sha256)?;
    }

    let executable_path = if is_tar_gz_asset(&asset_name) {
        extract_tar_gz_executable(tool, &asset_path, &download_root)?
    } else {
        asset_path
    };

    Ok(PullSource::temp(
        executable_path,
        match expected_sha256 {
            Some(expected) => format!("URL {url} (sha256={expected})"),
            None => format!("URL {url}"),
        },
        download_root,
    ))
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
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let pid = std::process::id();
    let dir = base.join(format!("{prefix}-{ts}-{pid}"));
    fs::create_dir(&dir).with_context(|| format!("create temp dir {}", dir.display()))?;
    Ok(dir)
}
