use crate::{
    cli::CiCommands,
    command::{
        style as tty_style,
        za_config::{self, ProxyScope},
    },
};
use anyhow::{Context, Result, anyhow, bail};
use humantime::parse_rfc3339_weak;
use reqx::{
    advanced::ClientProfile,
    blocking::{Client, ClientBuilder},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
    thread,
    time::{Duration, Instant, SystemTime},
};

const GITHUB_API_BASE: &str = "https://api.github.com";
const GITHUB_API_VERSION: &str = "2022-11-28";
const HTTP_USER_AGENT: &str = "za-ci/0.1";
const CI_CONFIG_FILE_NAME: &str = "ci.toml";
const CONFIG_DIR_NAME: &str = "za";
const WATCH_PENDING_INTERVAL_SECS: u64 = 2;
const WATCH_RUNNING_INTERVAL_SECS: u64 = 5;
const WATCH_DETAIL_LIMIT: usize = 3;
const EXIT_RUNNING: i32 = 10;
const EXIT_FAILED: i32 = 11;
const EXIT_NO_RUNS: i32 = 12;
const CI_CACHE_SCHEMA_VERSION: u8 = 1;
const CI_CACHE_FILE_NAME: &str = "gh-ci-cache-v1.json";
const CI_CACHE_TTL_SECS: u64 = 20;

pub fn run(cmd: Option<CiCommands>, json: bool, github_token: Option<String>) -> Result<i32> {
    match cmd {
        None => run_status(json, github_token),
        Some(CiCommands::Watch {
            timeout_secs,
            json,
            github_token,
        }) => run_watch(timeout_secs, json, github_token),
        Some(CiCommands::List {
            group,
            repo,
            file,
            json,
            all,
            github_token,
        }) => run_list(group, repo, file, json, all, github_token),
        Some(CiCommands::Inspect {
            all,
            json,
            github_token,
        }) => run_inspect(all, json, github_token),
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum CiState {
    Pending,
    Running,
    Success,
    Failed,
    Cancelled,
    Skipped,
    NoRuns,
}

impl CiState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
            Self::NoRuns => "no_runs",
        }
    }

    fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending | Self::Running)
    }

    fn sort_weight(self) -> u8 {
        match self {
            Self::Failed => 0,
            Self::Cancelled => 1,
            Self::Running => 2,
            Self::Pending => 3,
            Self::NoRuns => 4,
            Self::Skipped => 5,
            Self::Success => 6,
        }
    }

    fn badge(self) -> &'static str {
        match self {
            Self::Pending => "PEND",
            Self::Running => "RUN",
            Self::Success => "OK",
            Self::Failed => "FAIL",
            Self::Cancelled => "CANC",
            Self::Skipped => "SKIP",
            Self::NoRuns => "NONE",
        }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
struct CiSummary {
    pending: usize,
    running: usize,
    success: usize,
    failed: usize,
    cancelled: usize,
    skipped: usize,
}

impl CiSummary {
    fn push(&mut self, state: CiState) {
        match state {
            CiState::Pending => self.pending += 1,
            CiState::Running => self.running += 1,
            CiState::Success => self.success += 1,
            CiState::Failed => self.failed += 1,
            CiState::Cancelled => self.cancelled += 1,
            CiState::Skipped => self.skipped += 1,
            CiState::NoRuns => {}
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
enum CiSourceKind {
    CurrentRepo,
    LocalPath,
    Repo,
}

#[derive(Debug, Clone, Serialize)]
struct WorkflowRunReport {
    id: u64,
    name: String,
    event: Option<String>,
    state: CiState,
    status: Option<String>,
    conclusion: Option<String>,
    run_attempt: Option<u64>,
    updated_at: Option<String>,
    html_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct WorkflowJobReport {
    id: u64,
    name: String,
    state: CiState,
    status: Option<String>,
    conclusion: Option<String>,
    html_url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    attention_steps: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct WorkflowInspectReport {
    run: WorkflowRunReport,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    jobs: Vec<WorkflowJobReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    job_query_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct CommitCiInspectReport {
    repo: String,
    sha: Option<String>,
    selected_all_runs: bool,
    state: CiState,
    summary: CiSummary,
    workflows: Vec<WorkflowInspectReport>,
}

#[derive(Debug, Clone, Serialize)]
struct CommitCiReport {
    repo: String,
    branch: Option<String>,
    sha: Option<String>,
    state: CiState,
    summary: CiSummary,
    latest_update_at: Option<String>,
    source: CiSourceKind,
    source_path: Option<String>,
    runs: Vec<WorkflowRunReport>,
}

#[derive(Debug, Serialize, Default)]
struct CiBoardSummary {
    total: usize,
    errors: usize,
    pending: usize,
    running: usize,
    success: usize,
    failed: usize,
    cancelled: usize,
    skipped: usize,
    no_runs: usize,
}

impl CiBoardSummary {
    fn push_state(&mut self, state: CiState) {
        self.total += 1;
        match state {
            CiState::Pending => self.pending += 1,
            CiState::Running => self.running += 1,
            CiState::Success => self.success += 1,
            CiState::Failed => self.failed += 1,
            CiState::Cancelled => self.cancelled += 1,
            CiState::Skipped => self.skipped += 1,
            CiState::NoRuns => self.no_runs += 1,
        }
    }
}

#[derive(Debug, Serialize)]
struct CiBoardEntry {
    target: String,
    report: Option<CommitCiReport>,
    query_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct CiBoardOutput {
    summary: CiBoardSummary,
    entries: Vec<CiBoardEntry>,
}

#[derive(Debug, Clone)]
struct RepoSlug {
    owner: String,
    repo: String,
}

impl RepoSlug {
    fn as_str(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

#[derive(Debug)]
struct LocalRepoContext {
    repo_path: PathBuf,
    slug: RepoSlug,
    branch: Option<String>,
    sha: String,
}

#[derive(Debug)]
enum CiTarget {
    LocalPath(PathBuf),
    Remote(RepoSlug),
}

impl CiTarget {
    fn label(&self) -> String {
        match self {
            Self::LocalPath(path) => path.display().to_string(),
            Self::Remote(slug) => slug.as_str(),
        }
    }

    fn dedupe_key(&self) -> String {
        match self {
            Self::LocalPath(path) => format!("path:{}", path.display()),
            Self::Remote(slug) => format!("repo:{}", slug.as_str()),
        }
    }
}

#[derive(Default, Deserialize)]
struct CiManifest {
    #[serde(default)]
    groups: BTreeMap<String, CiManifestGroup>,
}

#[derive(Default, Deserialize)]
struct CiManifestGroup {
    #[serde(default)]
    repos: Vec<String>,
}

#[derive(Deserialize)]
struct WorkflowRunsResponse {
    #[serde(default)]
    workflow_runs: Vec<GitHubWorkflowRun>,
}

#[derive(Deserialize)]
struct WorkflowJobsResponse {
    #[serde(default)]
    jobs: Vec<GitHubWorkflowJob>,
}

#[derive(Debug, Deserialize)]
struct GitHubWorkflowRun {
    id: u64,
    name: Option<String>,
    display_title: Option<String>,
    event: Option<String>,
    head_branch: Option<String>,
    #[serde(default)]
    head_sha: String,
    status: Option<String>,
    conclusion: Option<String>,
    run_attempt: Option<u64>,
    updated_at: Option<String>,
    html_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubWorkflowJob {
    id: u64,
    name: Option<String>,
    status: Option<String>,
    conclusion: Option<String>,
    html_url: Option<String>,
    #[serde(default)]
    steps: Vec<GitHubWorkflowJobStep>,
}

#[derive(Debug, Deserialize)]
struct GitHubWorkflowJobStep {
    name: Option<String>,
    status: Option<String>,
    conclusion: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CiCacheMode {
    ReadWrite,
    Bypass,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CiApiCacheFile {
    schema_version: u8,
    #[serde(default)]
    entries: BTreeMap<String, CiApiCacheEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CiApiCacheEntry {
    fetched_at_unix_secs: u64,
    body: String,
}

#[derive(Debug, Default)]
struct CiApiCacheState {
    path: Option<PathBuf>,
    data: CiApiCacheFile,
    dirty: bool,
}

struct GitHubClient {
    http: Client,
    github_token: Option<String>,
    cache: Option<Mutex<CiApiCacheState>>,
}

impl GitHubClient {
    fn new(github_token_override: Option<String>, cache_mode: CiCacheMode) -> Result<Self> {
        Ok(Self {
            http: build_http_client(GITHUB_API_BASE)?,
            github_token: resolve_github_token(github_token_override)?,
            cache: matches!(cache_mode, CiCacheMode::ReadWrite)
                .then(|| Mutex::new(CiApiCacheState::load())),
        })
    }

    fn flush_cache(&self) -> Result<()> {
        let Some(cache) = self.cache.as_ref() else {
            return Ok(());
        };
        let mut cache = cache
            .lock()
            .map_err(|_| anyhow!("ci cache lock poisoned"))?;
        let _ = cache.save_if_dirty();
        Ok(())
    }

    fn fetch_commit_report_for_local_path(
        &self,
        path: &Path,
        source: CiSourceKind,
    ) -> Result<CommitCiReport> {
        let ctx = resolve_local_repo_context(path)?;
        self.fetch_commit_report_for_sha(
            &ctx.slug,
            ctx.branch,
            Some(ctx.sha),
            source,
            Some(ctx.repo_path.display().to_string()),
        )
    }

    fn fetch_latest_commit_report_for_repo(&self, slug: &RepoSlug) -> Result<CommitCiReport> {
        let recent = self.fetch_recent_workflow_runs(slug)?;
        let Some(latest_sha) = latest_head_sha(&recent) else {
            return Ok(CommitCiReport {
                repo: slug.as_str(),
                branch: None,
                sha: None,
                state: CiState::NoRuns,
                summary: CiSummary::default(),
                latest_update_at: None,
                source: CiSourceKind::Repo,
                source_path: None,
                runs: Vec::new(),
            });
        };
        let branch = recent
            .iter()
            .find(|run| run.head_sha.trim() == latest_sha)
            .and_then(|run| normalize_owned(run.head_branch.clone()));
        self.fetch_commit_report_for_sha(slug, branch, Some(latest_sha), CiSourceKind::Repo, None)
    }

    fn fetch_commit_report_for_sha(
        &self,
        slug: &RepoSlug,
        branch: Option<String>,
        sha: Option<String>,
        source: CiSourceKind,
        source_path: Option<String>,
    ) -> Result<CommitCiReport> {
        let runs = match sha.as_deref() {
            Some(sha) => self.fetch_workflow_runs_for_sha(slug, sha)?,
            None => Vec::new(),
        };
        Ok(build_commit_report(
            slug,
            branch,
            sha,
            source,
            source_path,
            runs,
        ))
    }

    fn fetch_recent_workflow_runs(&self, slug: &RepoSlug) -> Result<Vec<GitHubWorkflowRun>> {
        let path = format!(
            "/repos/{}/{}/actions/runs?per_page=30",
            slug.owner, slug.repo
        );
        self.api_get_json(&path)
            .with_context(|| format!("query recent GitHub Actions runs for {}", slug.as_str()))
            .map(|resp: WorkflowRunsResponse| resp.workflow_runs)
    }

    fn fetch_workflow_runs_for_sha(
        &self,
        slug: &RepoSlug,
        sha: &str,
    ) -> Result<Vec<GitHubWorkflowRun>> {
        let path = format!(
            "/repos/{}/{}/actions/runs?per_page=100&head_sha={sha}",
            slug.owner, slug.repo
        );
        self.api_get_json(&path)
            .with_context(|| {
                format!(
                    "query GitHub Actions runs for {} @ {}",
                    slug.as_str(),
                    short_sha(sha)
                )
            })
            .map(|resp: WorkflowRunsResponse| resp.workflow_runs)
    }

    fn fetch_workflow_jobs(&self, slug: &RepoSlug, run_id: u64) -> Result<Vec<GitHubWorkflowJob>> {
        let path = format!(
            "/repos/{}/{}/actions/runs/{run_id}/jobs?per_page=100",
            slug.owner, slug.repo
        );
        self.api_get_json(&path)
            .with_context(|| {
                format!(
                    "query GitHub Actions jobs for {} run {}",
                    slug.as_str(),
                    run_id
                )
            })
            .map(|resp: WorkflowJobsResponse| resp.jobs)
    }

    fn api_get_json<T>(&self, path: &str) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let cache_key = self.cache_key(path);
        if let Some(body) = self.cache_get_fresh(&cache_key)
            && let Ok(parsed) = serde_json::from_str::<T>(&body)
        {
            return Ok(parsed);
        }

        let mut req = self.http.get(path);
        req = req
            .try_header("user-agent", HTTP_USER_AGENT)
            .context("set GitHub user-agent")?;
        req = req
            .try_header("accept", "application/vnd.github+json")
            .context("set GitHub accept header")?;
        req = req
            .try_header("x-github-api-version", GITHUB_API_VERSION)
            .context("set GitHub API version header")?;
        if let Some(token) = self.github_token.as_deref() {
            req = req
                .try_header("authorization", &format!("Bearer {token}"))
                .context("set GitHub authorization header")?;
        }

        let response = req
            .send_response()
            .with_context(|| format!("request GitHub API `{path}`"))?;
        let status = response.status();
        if !status.is_success() {
            let body = truncate_end(&response.text_lossy(), 200);
            if status.as_u16() == 403 {
                if self.github_token.is_none() {
                    bail!(
                        "GitHub API returned 403 for `{path}`; set GITHUB_TOKEN, GH_TOKEN, or `za config set github-token <token>`. body: {body}"
                    );
                }
                bail!("GitHub API returned 403 for `{path}`. body: {body}");
            }
            if status.as_u16() == 404 {
                bail!("GitHub API returned 404 for `{path}`. body: {body}");
            }
            bail!(
                "GitHub API returned status {} for `{path}`. body: {}",
                status,
                body
            );
        }
        let body = response.text_lossy().to_string();
        let parsed = serde_json::from_str::<T>(&body)
            .with_context(|| format!("parse GitHub API JSON from `{path}`"))?;
        self.cache_put(&cache_key, &body)?;
        Ok(parsed)
    }

    fn cache_key(&self, path: &str) -> String {
        format!(
            "{}:{}",
            if self.github_token.is_some() {
                "token"
            } else {
                "anon"
            },
            path
        )
    }

    fn cache_get_fresh(&self, key: &str) -> Option<String> {
        let cache = self.cache.as_ref()?;
        let mut cache = cache.lock().ok()?;
        cache.get_fresh(key, now_unix_secs())
    }

    fn cache_put(&self, key: &str, body: &str) -> Result<()> {
        let Some(cache) = self.cache.as_ref() else {
            return Ok(());
        };
        let mut cache = cache
            .lock()
            .map_err(|_| anyhow!("ci cache lock poisoned"))?;
        cache.put(key, body.to_string(), now_unix_secs());
        Ok(())
    }
}

impl CiApiCacheState {
    fn load() -> Self {
        let Some(path) = ci_cache_path() else {
            return Self::default();
        };
        let data = match fs::read(&path) {
            Ok(raw) => match serde_json::from_slice::<CiApiCacheFile>(&raw) {
                Ok(parsed) if parsed.schema_version == CI_CACHE_SCHEMA_VERSION => parsed,
                _ => CiApiCacheFile::default(),
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => CiApiCacheFile::default(),
            Err(_) => CiApiCacheFile::default(),
        };
        Self {
            path: Some(path),
            data,
            dirty: false,
        }
    }

    fn get_fresh(&mut self, key: &str, now_unix_secs: u64) -> Option<String> {
        let entry = self.data.entries.get(key)?;
        if now_unix_secs.saturating_sub(entry.fetched_at_unix_secs) <= CI_CACHE_TTL_SECS {
            return Some(entry.body.clone());
        }
        self.data.entries.remove(key);
        self.dirty = true;
        None
    }

    fn put(&mut self, key: &str, body: String, now_unix_secs: u64) {
        self.data.entries.insert(
            key.to_string(),
            CiApiCacheEntry {
                fetched_at_unix_secs: now_unix_secs,
                body,
            },
        );
        self.dirty = true;
    }

    fn save_if_dirty(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create CI cache directory {}", parent.display()))?;
        }
        self.data.schema_version = CI_CACHE_SCHEMA_VERSION;
        let raw = serde_json::to_vec_pretty(&self.data).context("serialize CI cache")?;
        let tmp = path.with_extension(format!("tmp-ci-cache-{}", std::process::id()));
        fs::write(&tmp, raw).with_context(|| format!("write {}", tmp.display()))?;
        fs::rename(&tmp, &path)
            .with_context(|| format!("replace ci cache {} -> {}", path.display(), tmp.display()))?;
        self.dirty = false;
        Ok(())
    }
}

fn ci_cache_path() -> Option<PathBuf> {
    if let Some(base) = env::var_os("XDG_CACHE_HOME") {
        return Some(PathBuf::from(base).join("za").join(CI_CACHE_FILE_NAME));
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".cache").join("za").join(CI_CACHE_FILE_NAME))
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn run_status(json: bool, github_token: Option<String>) -> Result<i32> {
    let client = GitHubClient::new(github_token, CiCacheMode::ReadWrite)?;
    let report = client
        .fetch_commit_report_for_local_path(&env::current_dir()?, CiSourceKind::CurrentRepo)?;
    client.flush_cache()?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).context("serialize ci status output")?
        );
        return Ok(exit_code_for_state(report.state));
    }
    print_commit_report(&report);
    Ok(exit_code_for_state(report.state))
}

fn run_watch(timeout_secs: Option<u64>, json: bool, github_token: Option<String>) -> Result<i32> {
    let client = GitHubClient::new(github_token, CiCacheMode::Bypass)?;
    let cwd = env::current_dir()?;
    let started = Instant::now();
    let mut last_digest = None::<String>;

    let report = loop {
        let report = client.fetch_commit_report_for_local_path(&cwd, CiSourceKind::CurrentRepo)?;
        let digest = report_digest(&report);
        if !json && last_digest.as_deref() != Some(digest.as_str()) {
            if last_digest.is_none() {
                println!(
                    "Watching GitHub Actions for {} @ {}",
                    report.repo,
                    report
                        .sha
                        .as_deref()
                        .map(short_sha)
                        .unwrap_or_else(|| "-".to_string())
                );
            }
            print_watch_update(&report);
            last_digest = Some(digest);
        }

        if report.state.is_terminal() && report.state != CiState::NoRuns {
            break report;
        }

        if let Some(timeout_secs) = timeout_secs
            && started.elapsed() >= Duration::from_secs(timeout_secs)
        {
            break report;
        }

        thread::sleep(watch_interval_for_state(report.state));
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).context("serialize ci watch output")?
        );
        return Ok(exit_code_for_state(report.state));
    }

    if report.state == CiState::NoRuns {
        println!("No GitHub Actions runs found for this commit yet.");
    }
    print_commit_report(&report);
    Ok(exit_code_for_state(report.state))
}

fn run_list(
    group: Option<String>,
    repos: Vec<String>,
    file: Option<PathBuf>,
    json: bool,
    show_all: bool,
    github_token: Option<String>,
) -> Result<i32> {
    let client = GitHubClient::new(github_token, CiCacheMode::ReadWrite)?;
    let targets = resolve_list_targets(group, repos, file)?;
    let mut entries = Vec::with_capacity(targets.len());
    let mut summary = CiBoardSummary::default();

    for target in targets {
        let label = target.label();
        let result = match target {
            CiTarget::LocalPath(path) => {
                client.fetch_commit_report_for_local_path(&path, CiSourceKind::LocalPath)
            }
            CiTarget::Remote(slug) => client.fetch_latest_commit_report_for_repo(&slug),
        };

        match result {
            Ok(report) => {
                summary.push_state(report.state);
                entries.push(CiBoardEntry {
                    target: label,
                    report: Some(report),
                    query_error: None,
                });
            }
            Err(err) => {
                summary.total += 1;
                summary.errors += 1;
                entries.push(CiBoardEntry {
                    target: label,
                    report: None,
                    query_error: Some(err.to_string()),
                });
            }
        }
    }

    entries.sort_by(|a, b| {
        entry_sort_weight(a)
            .cmp(&entry_sort_weight(b))
            .then_with(|| a.target.cmp(&b.target))
    });

    let out = CiBoardOutput { summary, entries };
    client.flush_cache()?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&out).context("serialize ci list output")?
        );
        return Ok(exit_code_for_board(&out));
    }
    print_board_output(&out, show_all);
    Ok(exit_code_for_board(&out))
}

fn run_inspect(all: bool, json: bool, github_token: Option<String>) -> Result<i32> {
    let client = GitHubClient::new(github_token, CiCacheMode::ReadWrite)?;
    let cwd = env::current_dir()?;
    let ctx = resolve_local_repo_context(&cwd)?;
    let report = client.fetch_commit_report_for_sha(
        &ctx.slug,
        ctx.branch.clone(),
        Some(ctx.sha.clone()),
        CiSourceKind::CurrentRepo,
        Some(ctx.repo_path.display().to_string()),
    )?;
    let inspect = build_inspect_report(&client, &ctx.slug, &report, all);
    client.flush_cache()?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&inspect).context("serialize ci inspect output")?
        );
        return Ok(exit_code_for_state(report.state));
    }

    for line in render_inspect_report_lines(&inspect) {
        println!("{line}");
    }
    Ok(exit_code_for_state(report.state))
}

fn resolve_list_targets(
    group: Option<String>,
    repos: Vec<String>,
    file: Option<PathBuf>,
) -> Result<Vec<CiTarget>> {
    let cwd = env::current_dir()?;
    let mut targets = Vec::new();

    if let Some(group) = group {
        let manifest_path = file.unwrap_or(default_ci_manifest_path()?);
        let group_targets = load_manifest_group_targets(&manifest_path, &group)?;
        targets.extend(group_targets);
    }

    for repo in repos {
        targets.push(resolve_target_from_input(&cwd, &repo)?);
    }

    if targets.is_empty() {
        targets.push(CiTarget::LocalPath(cwd));
    }

    let mut seen = BTreeSet::new();
    let mut deduped = Vec::new();
    for target in targets {
        if seen.insert(target.dedupe_key()) {
            deduped.push(target);
        }
    }
    Ok(deduped)
}

fn load_manifest_group_targets(path: &Path, group: &str) -> Result<Vec<CiTarget>> {
    if !path.exists() {
        bail!("ci manifest not found: {}", path.display());
    }
    let raw =
        fs::read_to_string(path).with_context(|| format!("read ci manifest {}", path.display()))?;
    let manifest = toml::from_str::<CiManifest>(&raw)
        .with_context(|| format!("parse ci manifest {}", path.display()))?;
    let group_cfg = manifest
        .groups
        .get(group)
        .ok_or_else(|| anyhow!("ci group `{group}` not found in {}", path.display()))?;
    let base_dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut targets = Vec::new();
    for repo in &group_cfg.repos {
        targets.push(resolve_target_from_input(&base_dir, repo)?);
    }
    if targets.is_empty() {
        bail!("ci group `{group}` in {} is empty", path.display());
    }
    Ok(targets)
}

fn resolve_target_from_input(base_dir: &Path, raw: &str) -> Result<CiTarget> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("repo target must not be empty");
    }

    let candidate = if Path::new(trimmed).is_absolute() {
        PathBuf::from(trimmed)
    } else {
        base_dir.join(trimmed)
    };
    if candidate.exists() {
        let canonical = fs::canonicalize(&candidate)
            .with_context(|| format!("canonicalize repo path {}", candidate.display()))?;
        return Ok(CiTarget::LocalPath(canonical));
    }

    let slug = parse_repo_slug(trimmed).ok_or_else(|| {
        anyhow!("invalid repo target `{trimmed}`: use owner/repo, GitHub URL, or a local path")
    })?;
    Ok(CiTarget::Remote(slug))
}

fn build_commit_report(
    slug: &RepoSlug,
    branch: Option<String>,
    sha: Option<String>,
    source: CiSourceKind,
    source_path: Option<String>,
    mut runs: Vec<GitHubWorkflowRun>,
) -> CommitCiReport {
    let mut reports = runs.drain(..).map(workflow_run_report).collect::<Vec<_>>();
    reports.sort_by(|a, b| {
        a.state
            .sort_weight()
            .cmp(&b.state.sort_weight())
            .then_with(|| a.name.cmp(&b.name))
    });

    let mut summary = CiSummary::default();
    for run in &reports {
        summary.push(run.state);
    }

    let latest_update_at = reports
        .iter()
        .filter_map(|run| run.updated_at.clone())
        .max();

    CommitCiReport {
        repo: slug.as_str(),
        branch,
        sha,
        state: aggregate_commit_state(&reports),
        summary,
        latest_update_at,
        source,
        source_path,
        runs: reports,
    }
}

fn build_inspect_report(
    client: &GitHubClient,
    slug: &RepoSlug,
    report: &CommitCiReport,
    all: bool,
) -> CommitCiInspectReport {
    let selected_runs = report
        .runs
        .iter()
        .filter(|run| all || matches!(run.state, CiState::Failed | CiState::Cancelled))
        .cloned()
        .collect::<Vec<_>>();
    let mut workflows = Vec::with_capacity(selected_runs.len());

    for run in selected_runs {
        match client.fetch_workflow_jobs(slug, run.id) {
            Ok(jobs) => workflows.push(WorkflowInspectReport {
                run,
                jobs: jobs
                    .into_iter()
                    .map(workflow_job_report)
                    .filter(|job| {
                        all || !job.state.is_terminal()
                            || matches!(job.state, CiState::Failed | CiState::Cancelled)
                    })
                    .collect(),
                job_query_error: None,
            }),
            Err(err) => workflows.push(WorkflowInspectReport {
                run,
                jobs: Vec::new(),
                job_query_error: Some(err.to_string()),
            }),
        }
    }

    workflows.sort_by(|a, b| {
        review_detail_priority(a.run.state)
            .cmp(&review_detail_priority(b.run.state))
            .then_with(|| a.run.name.cmp(&b.run.name))
    });

    CommitCiInspectReport {
        repo: report.repo.clone(),
        sha: report.sha.clone(),
        selected_all_runs: all,
        state: report.state,
        summary: report.summary.clone(),
        workflows,
    }
}

fn workflow_run_report(run: GitHubWorkflowRun) -> WorkflowRunReport {
    WorkflowRunReport {
        id: run.id,
        name: normalize_owned(run.name)
            .or_else(|| normalize_owned(run.display_title))
            .unwrap_or_else(|| format!("run-{}", run.id)),
        event: normalize_owned(run.event),
        state: workflow_run_state(run.status.as_deref(), run.conclusion.as_deref()),
        status: normalize_owned(run.status),
        conclusion: normalize_owned(run.conclusion),
        run_attempt: run.run_attempt,
        updated_at: normalize_owned(run.updated_at),
        html_url: normalize_owned(run.html_url),
    }
}

fn workflow_job_report(job: GitHubWorkflowJob) -> WorkflowJobReport {
    let state = workflow_run_state(job.status.as_deref(), job.conclusion.as_deref());
    WorkflowJobReport {
        id: job.id,
        name: normalize_owned(job.name).unwrap_or_else(|| format!("job-{}", job.id)),
        state,
        status: normalize_owned(job.status),
        conclusion: normalize_owned(job.conclusion),
        html_url: normalize_owned(job.html_url),
        attention_steps: job
            .steps
            .into_iter()
            .filter_map(|step| {
                let state = workflow_run_state(step.status.as_deref(), step.conclusion.as_deref());
                (!matches!(state, CiState::Success | CiState::Skipped))
                    .then(|| normalize_owned(step.name))
                    .flatten()
            })
            .collect(),
    }
}

fn workflow_run_state(status: Option<&str>, conclusion: Option<&str>) -> CiState {
    match status.map(|value| value.trim().to_ascii_lowercase()) {
        Some(status)
            if matches!(
                status.as_str(),
                "queued" | "requested" | "waiting" | "pending"
            ) =>
        {
            CiState::Pending
        }
        Some(status) if status == "in_progress" => CiState::Running,
        Some(status) if status == "completed" => match conclusion
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("success") => CiState::Success,
            Some("cancelled") => CiState::Cancelled,
            Some("neutral") | Some("skipped") => CiState::Skipped,
            Some("failure")
            | Some("startup_failure")
            | Some("timed_out")
            | Some("action_required")
            | Some("stale") => CiState::Failed,
            _ => CiState::Failed,
        },
        _ => {
            if conclusion.is_some() {
                workflow_run_state(Some("completed"), conclusion)
            } else {
                CiState::Running
            }
        }
    }
}

fn aggregate_commit_state(runs: &[WorkflowRunReport]) -> CiState {
    if runs.is_empty() {
        return CiState::NoRuns;
    }
    if runs.iter().any(|run| run.state == CiState::Running) {
        return CiState::Running;
    }
    if runs.iter().any(|run| run.state == CiState::Pending) {
        return CiState::Pending;
    }
    if runs.iter().any(|run| run.state == CiState::Failed) {
        return CiState::Failed;
    }
    if runs.iter().any(|run| run.state == CiState::Cancelled) {
        return CiState::Cancelled;
    }
    if runs.iter().any(|run| run.state == CiState::Success) {
        return CiState::Success;
    }
    CiState::Skipped
}

fn entry_sort_weight(entry: &CiBoardEntry) -> u8 {
    match &entry.query_error {
        Some(_) => 0,
        None => entry
            .report
            .as_ref()
            .map(|report| report.state.sort_weight() + 1)
            .unwrap_or(u8::MAX),
    }
}

fn exit_code_for_state(state: CiState) -> i32 {
    match state {
        CiState::Pending | CiState::Running => EXIT_RUNNING,
        CiState::Failed | CiState::Cancelled => EXIT_FAILED,
        CiState::NoRuns => EXIT_NO_RUNS,
        CiState::Success | CiState::Skipped => 0,
    }
}

fn exit_code_for_board(board: &CiBoardOutput) -> i32 {
    if board.summary.errors > 0 || board.summary.failed > 0 || board.summary.cancelled > 0 {
        return EXIT_FAILED;
    }
    if board.summary.running > 0 || board.summary.pending > 0 {
        return EXIT_RUNNING;
    }
    if board.summary.total > 0
        && board.summary.no_runs == board.summary.total
        && board.summary.errors == 0
    {
        return EXIT_NO_RUNS;
    }
    0
}

fn board_entries_for_text(board: &CiBoardOutput, show_all: bool) -> (Vec<&CiBoardEntry>, usize) {
    if show_all {
        return (board.entries.iter().collect(), 0);
    }

    let has_attention = board.entries.iter().any(entry_needs_attention);
    if !has_attention {
        return (board.entries.iter().collect(), 0);
    }

    let mut visible = Vec::with_capacity(board.entries.len());
    let mut hidden_success = 0usize;
    for entry in &board.entries {
        if is_clean_success_entry(entry) {
            hidden_success += 1;
            continue;
        }
        visible.push(entry);
    }
    (visible, hidden_success)
}

fn entry_needs_attention(entry: &CiBoardEntry) -> bool {
    entry.query_error.is_some()
        || entry
            .report
            .as_ref()
            .is_some_and(|report| report.state != CiState::Success)
}

fn is_clean_success_entry(entry: &CiBoardEntry) -> bool {
    entry.query_error.is_none()
        && entry
            .report
            .as_ref()
            .is_some_and(|report| report.state == CiState::Success)
}

fn watch_interval_for_state(state: CiState) -> Duration {
    match state {
        CiState::Pending | CiState::NoRuns => Duration::from_secs(WATCH_PENDING_INTERVAL_SECS),
        CiState::Running => Duration::from_secs(WATCH_RUNNING_INTERVAL_SECS),
        CiState::Success | CiState::Failed | CiState::Cancelled | CiState::Skipped => {
            Duration::from_secs(0)
        }
    }
}

fn report_digest(report: &CommitCiReport) -> String {
    let mut digest = format!(
        "{}:{}:{}:{}:{}:{}",
        report.state.as_str(),
        report.summary.pending,
        report.summary.running,
        report.summary.success,
        report.summary.failed,
        report.summary.cancelled + report.summary.skipped
    );
    for run in &report.runs {
        digest.push(':');
        digest.push_str(&format!(
            "{}:{}:{}",
            run.id,
            run.state.as_str(),
            run.run_attempt.unwrap_or_default()
        ));
    }
    digest
}

fn print_watch_update(report: &CommitCiReport) {
    for line in render_watch_update_lines(report) {
        println!("{line}");
    }
}

fn print_commit_report(report: &CommitCiReport) {
    for line in render_commit_report_lines(report) {
        println!("{line}");
    }
}

fn print_board_output(board: &CiBoardOutput, show_all: bool) {
    for line in render_board_output_lines(board, show_all) {
        println!("{line}");
    }
}

fn format_summary_compact(summary: &CiSummary) -> String {
    let mut parts = Vec::new();
    if summary.failed > 0 {
        parts.push(ci_summary_token(summary.failed, "fail", CiState::Failed));
    }
    if summary.cancelled > 0 {
        parts.push(ci_summary_token(
            summary.cancelled,
            "cancel",
            CiState::Cancelled,
        ));
    }
    if summary.running > 0 {
        parts.push(ci_summary_token(summary.running, "run", CiState::Running));
    }
    if summary.pending > 0 {
        parts.push(ci_summary_token(summary.pending, "pend", CiState::Pending));
    }
    if summary.success > 0 {
        parts.push(ci_summary_token(summary.success, "ok", CiState::Success));
    }
    if summary.skipped > 0 {
        parts.push(ci_summary_token(summary.skipped, "skip", CiState::Skipped));
    }
    if parts.is_empty() {
        tty_style::dim("no runs")
    } else {
        parts.join(&format!(" {} ", tty_style::dim("·")))
    }
}

fn format_board_summary(summary: &CiBoardSummary) -> String {
    let mut parts = Vec::new();
    if summary.errors > 0 {
        parts.push(tty_style::error(format!("{} err", summary.errors)));
    }
    if summary.failed > 0 {
        parts.push(ci_summary_token(summary.failed, "fail", CiState::Failed));
    }
    if summary.cancelled > 0 {
        parts.push(ci_summary_token(
            summary.cancelled,
            "cancel",
            CiState::Cancelled,
        ));
    }
    if summary.running > 0 {
        parts.push(ci_summary_token(summary.running, "run", CiState::Running));
    }
    if summary.pending > 0 {
        parts.push(ci_summary_token(summary.pending, "pend", CiState::Pending));
    }
    if summary.success > 0 {
        parts.push(ci_summary_token(summary.success, "ok", CiState::Success));
    }
    if summary.no_runs > 0 {
        parts.push(ci_summary_token(summary.no_runs, "none", CiState::NoRuns));
    }
    if parts.is_empty() {
        tty_style::dim("no targets")
    } else {
        parts.join(&format!(" {} ", tty_style::dim("·")))
    }
}

fn render_commit_report_lines(report: &CommitCiReport) -> Vec<String> {
    let sha = report
        .sha
        .as_deref()
        .map(short_sha)
        .unwrap_or_else(|| "-".to_string());
    let updated = age_label(report.latest_update_at.as_deref()).unwrap_or_else(|| "-".to_string());
    let mut lines = vec![format!(
        "{} {}  {}  {} {}  {}",
        style_ci_badge(report.state, 5),
        tty_style::header(&report.repo),
        tty_style::dim(sha),
        tty_style::dim("updated"),
        tty_style::dim(updated),
        format_summary_compact(&report.summary)
    )];

    if report.runs.is_empty() {
        lines.push(tty_style::dim("no workflow runs found for this commit"));
        return lines;
    }

    lines.push(tty_style::header("actions"));
    for run in ordered_review_runs(&report.runs) {
        lines.push(render_run_detail_line(run, report));
    }
    if report
        .runs
        .iter()
        .any(|run| matches!(run.state, CiState::Failed | CiState::Cancelled))
    {
        lines.push(String::new());
        lines.push(tty_style::dim("inspect failures with `za gh ci inspect`"));
    }

    lines
}

fn render_board_output_lines(board: &CiBoardOutput, show_all: bool) -> Vec<String> {
    let mut lines = vec![format!(
        "{}  {} {}  {}",
        tty_style::header("CI"),
        tty_style::dim("total"),
        tty_style::header(board.summary.total.to_string()),
        format_board_summary(&board.summary)
    )];
    if board.entries.is_empty() {
        lines.push(tty_style::dim("No CI targets found."));
        return lines;
    }
    let (visible_entries, hidden_success) = board_entries_for_text(board, show_all);

    lines.push(tty_style::dim(format!(
        "{:<5} {:<28} {:<12} {:<7} {:<5} {:>3} {:>4} {:>2}  DETAIL",
        "ST", "REPO", "BRANCH", "SHA", "AGE", "RUN", "FAIL", "OK"
    )));

    for entry in visible_entries {
        match (&entry.report, &entry.query_error) {
            (_, Some(err)) => {
                lines.push(format!(
                    "{} {:<28} {:<12} {:<7} {:<5} {:>3} {:>4} {:>2}  {}",
                    style_error_badge(5),
                    truncate_end(&entry.target, 28),
                    style_ci_dim_field("-", 12),
                    style_ci_dim_field("-", 7),
                    style_ci_dim_field("-", 5),
                    style_ci_dim_number("-", 3),
                    style_ci_dim_number("-", 4),
                    style_ci_dim_number("-", 2),
                    truncate_end(err, 80)
                ));
            }
            (Some(report), None) => {
                let active = report.summary.running + report.summary.pending;
                let failures = report.summary.failed + report.summary.cancelled;
                let success = report.summary.success;
                let sha = report
                    .sha
                    .as_deref()
                    .map(short_sha)
                    .unwrap_or_else(|| "-".to_string());
                let age = age_label(report.latest_update_at.as_deref())
                    .unwrap_or_else(|| "-".to_string());
                lines.push(format!(
                    "{} {} {:<12} {:<7} {:<5} {:>3} {:>4} {:>2}  {}",
                    style_ci_badge(report.state, 5),
                    style_ci_repo_field(&report.repo, 28),
                    style_ci_dim_field(report.branch.as_deref().unwrap_or("-"), 12),
                    style_ci_dim_field(&sha, 7),
                    style_ci_dim_field(&age, 5),
                    style_ci_metric(active, 3, CiState::Running),
                    style_ci_metric(failures, 4, CiState::Failed),
                    style_ci_metric(success, 2, CiState::Success),
                    truncate_end(&board_detail(report), 80)
                ));
            }
            _ => {}
        }
    }

    if hidden_success > 0 {
        lines.push(String::new());
        lines.push(format!(
            "{} {} clean green target(s) hidden; pass `--all` to show them",
            tty_style::dim("..."),
            hidden_success
        ));
    }

    lines
}

fn render_watch_update_lines(report: &CommitCiReport) -> Vec<String> {
    let sha = report
        .sha
        .as_deref()
        .map(short_sha)
        .unwrap_or_else(|| "-".to_string());
    let updated = age_label(report.latest_update_at.as_deref()).unwrap_or_else(|| "-".to_string());
    let mut lines = vec![format!(
        "{} {}  {}  {} {}  {}",
        style_ci_badge(report.state, 5),
        tty_style::header(&report.repo),
        tty_style::dim(sha),
        tty_style::dim("updated"),
        tty_style::dim(updated),
        format_summary_compact(&report.summary)
    )];

    if !matches!(report.state, CiState::Pending | CiState::Running) {
        return lines;
    }

    let detail_runs = watch_detail_runs(report);
    let hidden_runs = detail_runs.len().saturating_sub(WATCH_DETAIL_LIMIT);
    for run in detail_runs.iter().take(WATCH_DETAIL_LIMIT) {
        lines.push(render_run_detail_line(run, report));
    }
    if hidden_runs > 0 {
        lines.push(format!(
            "  {} {} more active workflow{}",
            tty_style::dim("..."),
            hidden_runs,
            if hidden_runs == 1 { "" } else { "s" }
        ));
    }
    lines
}

fn render_inspect_report_lines(report: &CommitCiInspectReport) -> Vec<String> {
    let sha = report
        .sha
        .as_deref()
        .map(short_sha)
        .unwrap_or_else(|| "-".to_string());
    let selected = report.workflows.len();
    let mut lines = vec![format!(
        "{} {}  {}  {} {}",
        style_ci_badge(report.state, 5),
        tty_style::header(&report.repo),
        tty_style::dim(sha),
        tty_style::header(selected.to_string()),
        tty_style::dim(if selected == 1 {
            "workflow inspected"
        } else {
            "workflows inspected"
        })
    )];

    if report.workflows.is_empty() {
        lines.push(tty_style::dim(if report.selected_all_runs {
            "No workflows matched for this commit."
        } else {
            "No failed or cancelled workflows for this commit."
        }));
        return lines;
    }

    lines.push(tty_style::header("inspect"));
    for workflow in &report.workflows {
        lines.push(format!(
            "  {} {}",
            style_ci_badge(workflow.run.state, 5),
            style_ci_subject(&truncate_end(&workflow.run.name, 92), workflow.run.state)
        ));
        if let Some(url) = &workflow.run.html_url {
            lines.push(format!(
                "    {} {}",
                tty_style::dim("url"),
                tty_style::dim(url)
            ));
        }
        if let Some(err) = &workflow.job_query_error {
            lines.push(format!(
                "    {} {}",
                tty_style::warning("jobs"),
                truncate_end(err, 120)
            ));
            continue;
        }
        if workflow.jobs.is_empty() {
            lines.push(format!(
                "    {} {}",
                tty_style::dim("jobs"),
                tty_style::dim("no matching jobs")
            ));
            continue;
        }
        for job in &workflow.jobs {
            lines.push(format!(
                "    {} {}",
                style_ci_badge(job.state, 5),
                style_ci_subject(&truncate_end(&job.name, 88), job.state)
            ));
            if let Some(url) = &job.html_url {
                lines.push(format!(
                    "      {} {}",
                    tty_style::dim("url"),
                    tty_style::dim(url)
                ));
            }
            for step in &job.attention_steps {
                lines.push(format!(
                    "      {} {}",
                    tty_style::dim("step"),
                    truncate_end(step, 96)
                ));
            }
        }
    }

    lines
}

fn ordered_review_runs(runs: &[WorkflowRunReport]) -> Vec<&WorkflowRunReport> {
    let mut runs = runs.iter().collect::<Vec<_>>();
    runs.sort_by(|a, b| {
        review_detail_priority(a.state)
            .cmp(&review_detail_priority(b.state))
            .then_with(|| a.name.cmp(&b.name))
    });
    runs
}

fn watch_detail_runs(report: &CommitCiReport) -> Vec<&WorkflowRunReport> {
    let mut runs = report
        .runs
        .iter()
        .filter(|run| !matches!(run.state, CiState::Success | CiState::Skipped))
        .collect::<Vec<_>>();
    runs.sort_by(|a, b| {
        review_detail_priority(a.state)
            .cmp(&review_detail_priority(b.state))
            .then_with(|| a.name.cmp(&b.name))
    });
    runs
}

fn review_detail_priority(state: CiState) -> u8 {
    match state {
        CiState::Failed => 0,
        CiState::Cancelled => 1,
        CiState::Running => 2,
        CiState::Pending => 3,
        CiState::NoRuns => 4,
        CiState::Skipped => 5,
        CiState::Success => 6,
    }
}

fn render_run_detail_line(run: &WorkflowRunReport, report: &CommitCiReport) -> String {
    let age = age_label(run.updated_at.as_deref()).unwrap_or_else(|| "-".to_string());
    let mut detail = if has_mixed_events(&report.runs) {
        format!(
            "{} {}",
            style_ci_dim_field(&truncate_end(run.event.as_deref().unwrap_or("-"), 8), 8),
            style_ci_subject(&truncate_end(&run.name, 80), run.state)
        )
    } else {
        style_ci_subject(&truncate_end(&run.name, 88), run.state)
    };
    if let Some(attempt) = run.run_attempt
        && attempt > 1
    {
        detail.push_str(&format!("  {}", tty_style::dim(format!("#{attempt}"))));
    }
    format!(
        "  {} {} {}",
        style_ci_badge(run.state, 5),
        style_ci_dim_field(&age, 5),
        detail
    )
}

fn board_detail(report: &CommitCiReport) -> String {
    let mut parts = Vec::new();
    if report.summary.cancelled > 0 {
        parts.push(tty_style::dim(format!(
            "{} cancel",
            report.summary.cancelled
        )));
    }
    if report.summary.skipped > 0 {
        parts.push(tty_style::dim(format!("{} skip", report.summary.skipped)));
    }
    if report.state == CiState::NoRuns {
        parts.push(tty_style::dim("no runs"));
    }
    if parts.is_empty() {
        tty_style::dim("-")
    } else {
        parts.join(&format!(" {} ", tty_style::dim("·")))
    }
}

fn ci_summary_token(count: usize, label: &str, state: CiState) -> String {
    let text = if count == 0 && label == "no runs" {
        label.to_string()
    } else {
        format!("{count} {label}")
    };
    match state {
        CiState::Success => tty_style::success(text),
        CiState::Failed => tty_style::error(text),
        CiState::Cancelled | CiState::Pending => tty_style::warning(text),
        CiState::Running => tty_style::active(text),
        CiState::Skipped | CiState::NoRuns => tty_style::dim(text),
    }
}

fn style_ci_badge(state: CiState, width: usize) -> String {
    let label = format!("{:<width$}", state.badge());
    match state {
        CiState::Success => tty_style::success(label),
        CiState::Failed => tty_style::error(label),
        CiState::Cancelled | CiState::Pending => tty_style::warning(label),
        CiState::Running => tty_style::active(label),
        CiState::Skipped | CiState::NoRuns => tty_style::dim(label),
    }
}

fn style_error_badge(width: usize) -> String {
    tty_style::error(format!("{:<width$}", "ERR"))
}

fn style_ci_repo_field(value: &str, width: usize) -> String {
    tty_style::header(format!("{:<width$}", truncate_end(value, width)))
}

fn style_ci_dim_field(value: &str, width: usize) -> String {
    tty_style::dim(format!("{:<width$}", truncate_end(value, width)))
}

fn style_ci_dim_number(value: &str, width: usize) -> String {
    tty_style::dim(format!("{value:>width$}"))
}

fn style_ci_metric(value: usize, width: usize, state: CiState) -> String {
    let plain = format!("{value:>width$}");
    if value == 0 {
        return tty_style::dim(plain);
    }
    match state {
        CiState::Success => tty_style::success(plain),
        CiState::Failed => tty_style::error(plain),
        CiState::Cancelled | CiState::Pending => tty_style::warning(plain),
        CiState::Running => tty_style::active(plain),
        CiState::Skipped | CiState::NoRuns => tty_style::dim(plain),
    }
}

fn style_ci_subject(value: &str, state: CiState) -> String {
    match state {
        CiState::Success => tty_style::success(value),
        CiState::Failed => tty_style::error(value),
        CiState::Cancelled | CiState::Pending => tty_style::warning(value),
        CiState::Running => tty_style::active(value),
        CiState::Skipped | CiState::NoRuns => tty_style::header(value),
    }
}

fn has_mixed_events(runs: &[WorkflowRunReport]) -> bool {
    let mut distinct = BTreeSet::new();
    for run in runs {
        distinct.insert(run.event.as_deref().unwrap_or("-"));
        if distinct.len() > 1 {
            return true;
        }
    }
    false
}

fn age_label(rfc3339: Option<&str>) -> Option<String> {
    let value = rfc3339?;
    let ts = parse_rfc3339_weak(value).ok()?;
    let elapsed = match SystemTime::now().duration_since(ts) {
        Ok(duration) => duration,
        Err(_) => Duration::from_secs(0),
    };
    Some(format_duration_short(elapsed))
}

fn format_duration_short(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        return format!("{secs}s");
    }
    if secs < 3_600 {
        return format!("{}m", secs / 60);
    }
    if secs < 86_400 {
        return format!("{}h", secs / 3_600);
    }
    format!("{}d", secs / 86_400)
}

fn short_sha(value: &str) -> String {
    value.chars().take(7).collect()
}

fn truncate_end(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let mut out = String::new();
    for c in value.chars().take(max.saturating_sub(1)) {
        out.push(c);
    }
    out.push('…');
    out
}

fn latest_head_sha(runs: &[GitHubWorkflowRun]) -> Option<String> {
    runs.iter().find_map(|run| normalize_ref(&run.head_sha))
}

fn normalize_owned(value: Option<String>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

fn normalize_ref(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

fn resolve_local_repo_context(path: &Path) -> Result<LocalRepoContext> {
    let top_level = git_capture(path, &["rev-parse", "--show-toplevel"])
        .with_context(|| format!("resolve git repository root for {}", path.display()))?;
    let repo_path = fs::canonicalize(top_level.trim())
        .with_context(|| format!("canonicalize git repository root `{}`", top_level.trim()))?;

    let remotes = git_capture(path, &["remote"]).context("list git remotes")?;
    let remote_names = remotes
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let remote = if remote_names.iter().any(|name| name == "origin") {
        "origin".to_string()
    } else {
        remote_names
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("no git remotes configured"))?
    };

    let remote_url = git_capture(path, &["remote", "get-url", &remote])
        .with_context(|| format!("read git remote `{remote}` URL"))?;
    let slug = parse_repo_slug(remote_url.trim())
        .ok_or_else(|| anyhow!("git remote `{remote}` is not a GitHub repository URL"))?;
    let sha = git_capture(path, &["rev-parse", "HEAD"]).context("read git HEAD SHA")?;
    let branch =
        normalize_ref(&git_capture(path, &["branch", "--show-current"]).unwrap_or_default());

    Ok(LocalRepoContext {
        repo_path,
        slug,
        branch,
        sha: sha.trim().to_string(),
    })
}

fn git_capture(path: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .with_context(|| format!("run `git {}`", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("`git {}` failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn parse_repo_slug(input: &str) -> Option<RepoSlug> {
    let trimmed = input.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }

    if let Some(path) = trimmed.strip_prefix("git@github.com:") {
        return parse_owner_repo(path);
    }
    if let Some(path) = parse_github_url_path(trimmed) {
        return parse_owner_repo(path);
    }

    if !trimmed.contains("://") && trimmed.matches('/').count() == 1 {
        return parse_owner_repo(trimmed);
    }

    None
}

fn parse_owner_repo(path: &str) -> Option<RepoSlug> {
    let clean = path
        .split('?')
        .next()
        .unwrap_or(path)
        .split('#')
        .next()
        .unwrap_or(path)
        .trim_matches('/');
    let mut parts = clean.split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim().trim_end_matches(".git");
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(RepoSlug {
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

fn parse_github_url_path(input: &str) -> Option<&str> {
    let (_, rest) = input.split_once("://")?;
    let authority = rest.split('/').next()?;
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    let host = host.split(':').next().unwrap_or(host).trim();
    if !host.eq_ignore_ascii_case("github.com") {
        return None;
    }
    Some(rest[authority.len()..].trim_start_matches('/'))
}

fn default_ci_manifest_path() -> Result<PathBuf> {
    let path = if let Some(base) = env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(base)
            .join(CONFIG_DIR_NAME)
            .join(CI_CONFIG_FILE_NAME)
    } else if let Some(home) = env::var_os("HOME") {
        PathBuf::from(home)
            .join(".config")
            .join(CONFIG_DIR_NAME)
            .join(CI_CONFIG_FILE_NAME)
    } else {
        bail!("cannot resolve ci manifest path: set HOME or XDG_CONFIG_HOME");
    };
    Ok(path)
}

fn build_http_client(base_url: &str) -> Result<Client> {
    let mut builder = Client::builder(base_url)
        .profile(ClientProfile::StandardSdk)
        .client_name("za-ci");
    let scheme = base_url
        .split_once("://")
        .map(|(scheme, _)| scheme)
        .unwrap_or("https");
    builder = apply_proxy_with_scope(builder, scheme, ProxyScope::Ci)
        .with_context(|| format!("configure HTTP client proxy for `{base_url}`"))?;
    builder
        .build()
        .with_context(|| format!("build HTTP client for `{base_url}`"))
}

fn resolve_github_token(override_token: Option<String>) -> Result<Option<String>> {
    if let Some(token) = normalize_owned(override_token) {
        return Ok(Some(token));
    }
    for key in ["ZA_GITHUB_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(value) = env::var(key)
            && let Some(token) = normalize_owned(Some(value))
        {
            return Ok(Some(token));
        }
    }
    za_config::load_github_token()
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

fn apply_proxy_with_scope(
    mut builder: ClientBuilder,
    scheme: &str,
    proxy_scope: ProxyScope,
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

fn proxy_env_keys_for_scheme(scheme: &str) -> &'static [&'static str] {
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
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(((*name).to_string(), trimmed.to_string()));
        }
    }
    None
}

fn split_no_proxy_rules(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|rule| !rule.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        CiManifest, CiSourceKind, CiState, CommitCiInspectReport, CommitCiReport,
        WorkflowInspectReport, WorkflowJobReport, WorkflowRunReport, aggregate_commit_state,
        latest_head_sha, parse_owner_repo, parse_repo_slug, render_board_output_lines,
        render_commit_report_lines, render_inspect_report_lines, render_watch_update_lines,
        workflow_run_state,
    };

    #[test]
    fn parse_repo_slug_supports_slug_https_and_ssh() {
        let slug = parse_repo_slug("openai/codex").unwrap();
        assert_eq!(slug.as_str(), "openai/codex");

        let slug = parse_repo_slug("https://github.com/openai/codex.git").unwrap();
        assert_eq!(slug.as_str(), "openai/codex");

        let slug = parse_repo_slug("git@github.com:openai/codex.git").unwrap();
        assert_eq!(slug.as_str(), "openai/codex");

        let slug = parse_repo_slug("ssh://git@github.com/openai/codex.git").unwrap();
        assert_eq!(slug.as_str(), "openai/codex");
    }

    #[test]
    fn parse_owner_repo_rejects_invalid_values() {
        assert!(parse_owner_repo("owner").is_none());
        assert!(parse_repo_slug("https://example.com/openai/codex").is_none());
        assert!(parse_repo_slug("https://gist.github.com/openai/codex").is_none());
        assert!(parse_repo_slug("https://notgithub.com/github.com/openai/codex").is_none());
        assert!(parse_repo_slug("").is_none());
    }

    #[test]
    fn workflow_run_state_maps_github_statuses() {
        assert_eq!(workflow_run_state(Some("queued"), None), CiState::Pending);
        assert_eq!(
            workflow_run_state(Some("in_progress"), None),
            CiState::Running
        );
        assert_eq!(
            workflow_run_state(Some("completed"), Some("success")),
            CiState::Success
        );
        assert_eq!(
            workflow_run_state(Some("completed"), Some("cancelled")),
            CiState::Cancelled
        );
        assert_eq!(
            workflow_run_state(Some("completed"), Some("failure")),
            CiState::Failed
        );
    }

    #[test]
    fn aggregate_commit_state_prioritizes_active_and_failed_runs() {
        let runs = vec![
            WorkflowRunReport {
                id: 1,
                name: "build".to_string(),
                event: Some("push".to_string()),
                state: CiState::Success,
                status: Some("completed".to_string()),
                conclusion: Some("success".to_string()),
                run_attempt: Some(1),
                updated_at: None,
                html_url: None,
            },
            WorkflowRunReport {
                id: 2,
                name: "test".to_string(),
                event: Some("push".to_string()),
                state: CiState::Running,
                status: Some("in_progress".to_string()),
                conclusion: None,
                run_attempt: Some(1),
                updated_at: None,
                html_url: None,
            },
        ];
        assert_eq!(aggregate_commit_state(&runs), CiState::Running);

        let failed = vec![WorkflowRunReport {
            state: CiState::Failed,
            ..runs[0].clone()
        }];
        assert_eq!(aggregate_commit_state(&failed), CiState::Failed);
    }

    #[test]
    fn latest_head_sha_uses_first_recent_run_sha() {
        let runs = vec![
            super::GitHubWorkflowRun {
                id: 1,
                name: Some("ci".to_string()),
                display_title: None,
                event: Some("push".to_string()),
                head_branch: Some("main".to_string()),
                head_sha: "abc123".to_string(),
                status: Some("completed".to_string()),
                conclusion: Some("success".to_string()),
                run_attempt: Some(1),
                updated_at: Some("2026-03-06T00:00:00Z".to_string()),
                html_url: None,
            },
            super::GitHubWorkflowRun {
                id: 2,
                name: Some("lint".to_string()),
                display_title: None,
                event: Some("push".to_string()),
                head_branch: Some("main".to_string()),
                head_sha: "def456".to_string(),
                status: Some("completed".to_string()),
                conclusion: Some("success".to_string()),
                run_attempt: Some(1),
                updated_at: Some("2026-03-05T00:00:00Z".to_string()),
                html_url: None,
            },
        ];
        assert_eq!(latest_head_sha(&runs).as_deref(), Some("abc123"));
    }

    #[test]
    fn parse_ci_manifest_groups() {
        let raw = r#"
[groups.work]
repos = ["openai/codex", "/code/za"]
"#;
        let manifest = toml::from_str::<CiManifest>(raw).unwrap();
        let group = manifest.groups.get("work").unwrap();
        assert_eq!(group.repos.len(), 2);
        assert_eq!(group.repos[0], "openai/codex");
    }

    #[test]
    fn render_watch_update_lines_includes_active_workflow_details() {
        let report = CommitCiReport {
            repo: "lvillis/tele-rs".to_string(),
            branch: Some("main".to_string()),
            sha: Some("babf70d123456789".to_string()),
            state: CiState::Running,
            summary: super::CiSummary {
                running: 1,
                success: 2,
                ..Default::default()
            },
            latest_update_at: Some("2026-03-09T00:00:00Z".to_string()),
            source: CiSourceKind::CurrentRepo,
            source_path: None,
            runs: vec![
                WorkflowRunReport {
                    id: 2,
                    name: "ci / test".to_string(),
                    event: Some("push".to_string()),
                    state: CiState::Running,
                    status: Some("in_progress".to_string()),
                    conclusion: None,
                    run_attempt: Some(1),
                    updated_at: Some("2026-03-09T00:00:00Z".to_string()),
                    html_url: None,
                },
                WorkflowRunReport {
                    id: 1,
                    name: "ci / lint".to_string(),
                    event: Some("push".to_string()),
                    state: CiState::Success,
                    status: Some("completed".to_string()),
                    conclusion: Some("success".to_string()),
                    run_attempt: Some(1),
                    updated_at: Some("2026-03-09T00:00:00Z".to_string()),
                    html_url: None,
                },
            ],
        };

        let lines = render_watch_update_lines(&report);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("RUN"));
        assert!(lines[0].contains("lvillis/tele-rs"));
        assert!(lines[0].contains("babf70d"));
        assert!(lines[0].contains("1 run"));
        assert!(lines[0].contains("2 ok"));
        assert!(lines[0].contains("updated"));
        assert!(lines[1].contains("ci / test"));
        assert!(lines[1].contains("RUN"));
    }

    #[test]
    fn render_watch_update_lines_caps_non_terminal_workflow_details() {
        let report = CommitCiReport {
            repo: "lvillis/tele-rs".to_string(),
            branch: Some("main".to_string()),
            sha: Some("babf70d123456789".to_string()),
            state: CiState::Running,
            summary: super::CiSummary {
                running: 2,
                pending: 2,
                ..Default::default()
            },
            latest_update_at: Some("2026-03-09T00:00:00Z".to_string()),
            source: CiSourceKind::CurrentRepo,
            source_path: None,
            runs: vec![
                WorkflowRunReport {
                    id: 1,
                    name: "run-1".to_string(),
                    event: Some("push".to_string()),
                    state: CiState::Pending,
                    status: Some("queued".to_string()),
                    conclusion: None,
                    run_attempt: Some(1),
                    updated_at: None,
                    html_url: None,
                },
                WorkflowRunReport {
                    id: 2,
                    name: "run-2".to_string(),
                    event: Some("push".to_string()),
                    state: CiState::Running,
                    status: Some("in_progress".to_string()),
                    conclusion: None,
                    run_attempt: Some(1),
                    updated_at: None,
                    html_url: None,
                },
                WorkflowRunReport {
                    id: 3,
                    name: "run-3".to_string(),
                    event: Some("push".to_string()),
                    state: CiState::Failed,
                    status: Some("completed".to_string()),
                    conclusion: Some("failure".to_string()),
                    run_attempt: Some(1),
                    updated_at: None,
                    html_url: None,
                },
                WorkflowRunReport {
                    id: 4,
                    name: "run-4".to_string(),
                    event: Some("push".to_string()),
                    state: CiState::Pending,
                    status: Some("queued".to_string()),
                    conclusion: None,
                    run_attempt: Some(1),
                    updated_at: None,
                    html_url: None,
                },
            ],
        };

        let lines = render_watch_update_lines(&report);
        assert_eq!(lines.len(), 5);
        assert!(lines[1].contains("FAIL") || lines[1].contains("RUN"));
        assert!(lines[1].contains("run-3") || lines[1].contains("run-2"));
        assert!(
            lines[2].contains("run-2") || lines[2].contains("run-1") || lines[2].contains("run-4")
        );
        assert!(lines[4].contains("1 more active workflow"));
    }

    #[test]
    fn render_commit_report_lines_list_all_actions_in_review_order() {
        let report = CommitCiReport {
            repo: "lvillis/za".to_string(),
            branch: Some("main".to_string()),
            sha: Some("15ff429123456789".to_string()),
            state: CiState::Failed,
            summary: super::CiSummary {
                failed: 1,
                running: 1,
                success: 1,
                ..Default::default()
            },
            latest_update_at: Some("2026-03-09T00:00:00Z".to_string()),
            source: CiSourceKind::CurrentRepo,
            source_path: None,
            runs: vec![
                WorkflowRunReport {
                    id: 1,
                    name: "build-linux-musl".to_string(),
                    event: Some("push".to_string()),
                    state: CiState::Failed,
                    status: Some("completed".to_string()),
                    conclusion: Some("failure".to_string()),
                    run_attempt: Some(1),
                    updated_at: Some("2026-03-09T00:00:00Z".to_string()),
                    html_url: None,
                },
                WorkflowRunReport {
                    id: 2,
                    name: "test-linux-arm64".to_string(),
                    event: Some("push".to_string()),
                    state: CiState::Running,
                    status: Some("in_progress".to_string()),
                    conclusion: None,
                    run_attempt: Some(2),
                    updated_at: Some("2026-03-09T00:00:00Z".to_string()),
                    html_url: None,
                },
                WorkflowRunReport {
                    id: 3,
                    name: "lint".to_string(),
                    event: Some("push".to_string()),
                    state: CiState::Success,
                    status: Some("completed".to_string()),
                    conclusion: Some("success".to_string()),
                    run_attempt: Some(1),
                    updated_at: Some("2026-03-09T00:00:00Z".to_string()),
                    html_url: None,
                },
            ],
        };

        let lines = render_commit_report_lines(&report);
        assert!(lines[0].contains("FAIL"));
        assert!(lines[0].contains("1 fail"));
        assert!(lines[0].contains("1 run"));
        assert_eq!(lines[1], "actions");
        assert!(lines[2].contains("FAIL"));
        assert!(lines[2].contains("build-linux-musl"));
        assert!(lines[3].contains("RUN"));
        assert!(lines[3].contains("test-linux-arm64"));
        assert!(lines[4].contains("OK"));
        assert!(lines[4].contains("lint"));
    }

    #[test]
    fn render_commit_report_lines_show_all_green_actions() {
        let report = CommitCiReport {
            repo: "lvillis/za".to_string(),
            branch: Some("main".to_string()),
            sha: Some("15ff429123456789".to_string()),
            state: CiState::Success,
            summary: super::CiSummary {
                success: 2,
                ..Default::default()
            },
            latest_update_at: Some("2026-03-09T00:00:00Z".to_string()),
            source: CiSourceKind::CurrentRepo,
            source_path: None,
            runs: vec![
                WorkflowRunReport {
                    id: 1,
                    name: "ci / test".to_string(),
                    event: Some("push".to_string()),
                    state: CiState::Success,
                    status: Some("completed".to_string()),
                    conclusion: Some("success".to_string()),
                    run_attempt: Some(1),
                    updated_at: Some("2026-03-09T00:00:00Z".to_string()),
                    html_url: None,
                },
                WorkflowRunReport {
                    id: 2,
                    name: "ci / lint".to_string(),
                    event: Some("push".to_string()),
                    state: CiState::Success,
                    status: Some("completed".to_string()),
                    conclusion: Some("success".to_string()),
                    run_attempt: Some(1),
                    updated_at: Some("2026-03-09T00:00:00Z".to_string()),
                    html_url: None,
                },
            ],
        };

        let lines = render_commit_report_lines(&report);
        assert_eq!(lines.len(), 4);
        assert!(lines[0].contains("OK"));
        assert!(lines[0].contains("2 ok"));
        assert_eq!(lines[1], "actions");
        assert!(lines[2].contains("ci / lint") || lines[2].contains("ci / test"));
        assert!(lines[3].contains("ci / lint") || lines[3].contains("ci / test"));
    }

    #[test]
    fn render_board_output_lines_use_compact_dashboard_columns() {
        let board = super::CiBoardOutput {
            summary: super::CiBoardSummary {
                total: 2,
                failed: 1,
                success: 1,
                ..Default::default()
            },
            entries: vec![
                super::CiBoardEntry {
                    target: "lvillis/za".to_string(),
                    report: Some(CommitCiReport {
                        repo: "lvillis/za".to_string(),
                        branch: Some("main".to_string()),
                        sha: Some("15ff429123456789".to_string()),
                        state: CiState::Failed,
                        summary: super::CiSummary {
                            failed: 1,
                            success: 5,
                            ..Default::default()
                        },
                        latest_update_at: Some("2026-03-09T00:00:00Z".to_string()),
                        source: CiSourceKind::Repo,
                        source_path: None,
                        runs: Vec::new(),
                    }),
                    query_error: None,
                },
                super::CiBoardEntry {
                    target: "broken/repo".to_string(),
                    report: None,
                    query_error: Some("GitHub API returned 404".to_string()),
                },
            ],
        };

        let lines = render_board_output_lines(&board, true);
        assert!(lines[0].contains("total 2"));
        assert!(lines[0].contains("1 fail"));
        assert!(lines[0].contains("1 ok"));
        assert!(lines[1].contains("ST"));
        assert!(lines[1].contains("RUN"));
        assert!(lines[1].contains("FAIL"));
        assert!(lines.iter().any(|line| line.contains("lvillis/za")));
        assert!(lines.iter().any(|line| line.contains("ERR")));
    }

    #[test]
    fn render_board_output_lines_hide_clean_success_by_default() {
        let board = super::CiBoardOutput {
            summary: super::CiBoardSummary {
                total: 2,
                running: 1,
                success: 1,
                ..Default::default()
            },
            entries: vec![
                super::CiBoardEntry {
                    target: "lvillis/za".to_string(),
                    report: Some(CommitCiReport {
                        repo: "lvillis/za".to_string(),
                        branch: Some("main".to_string()),
                        sha: Some("15ff429123456789".to_string()),
                        state: CiState::Running,
                        summary: super::CiSummary {
                            running: 1,
                            ..Default::default()
                        },
                        latest_update_at: Some("2026-03-09T00:00:00Z".to_string()),
                        source: CiSourceKind::Repo,
                        source_path: None,
                        runs: Vec::new(),
                    }),
                    query_error: None,
                },
                super::CiBoardEntry {
                    target: "lvillis/green".to_string(),
                    report: Some(CommitCiReport {
                        repo: "lvillis/green".to_string(),
                        branch: Some("main".to_string()),
                        sha: Some("abcdef0123456789".to_string()),
                        state: CiState::Success,
                        summary: super::CiSummary {
                            success: 3,
                            ..Default::default()
                        },
                        latest_update_at: Some("2026-03-09T00:00:00Z".to_string()),
                        source: CiSourceKind::Repo,
                        source_path: None,
                        runs: Vec::new(),
                    }),
                    query_error: None,
                },
            ],
        };

        let lines = render_board_output_lines(&board, false);
        assert!(lines.iter().any(|line| line.contains("lvillis/za")));
        assert!(!lines.iter().any(|line| line.contains("lvillis/green")));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("1 clean green target(s) hidden"))
        );
    }

    #[test]
    fn render_inspect_report_lines_show_workflow_jobs_and_steps() {
        let report = CommitCiInspectReport {
            repo: "lvillis/za".to_string(),
            sha: Some("15ff429123456789".to_string()),
            selected_all_runs: false,
            state: CiState::Failed,
            summary: super::CiSummary {
                failed: 1,
                ..Default::default()
            },
            workflows: vec![WorkflowInspectReport {
                run: WorkflowRunReport {
                    id: 1,
                    name: "build-linux-musl".to_string(),
                    event: Some("push".to_string()),
                    state: CiState::Failed,
                    status: Some("completed".to_string()),
                    conclusion: Some("failure".to_string()),
                    run_attempt: Some(1),
                    updated_at: Some("2026-03-09T00:00:00Z".to_string()),
                    html_url: Some("https://github.com/lvillis/za/actions/runs/1".to_string()),
                },
                jobs: vec![WorkflowJobReport {
                    id: 11,
                    name: "cargo-test".to_string(),
                    state: CiState::Failed,
                    status: Some("completed".to_string()),
                    conclusion: Some("failure".to_string()),
                    html_url: Some(
                        "https://github.com/lvillis/za/actions/runs/1/job/11".to_string(),
                    ),
                    attention_steps: vec!["cargo test".to_string()],
                }],
                job_query_error: None,
            }],
        };

        let lines = render_inspect_report_lines(&report);
        let output = lines.join("\n");
        assert!(output.contains("FAIL"));
        assert!(output.contains("build-linux-musl"));
        assert!(output.contains("cargo-test"));
        assert!(output.contains("cargo test"));
        assert!(output.contains("actions/runs/1"));
        assert!(output.contains("job/11"));
    }
}
