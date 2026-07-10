use super::*;
use crate::command::http::build_client;
use reqx::{advanced::ClientProfile, blocking::Client};
use std::fs::{File, OpenOptions};

pub(super) struct ApiClient {
    crates_http: Client,
    github_http: Client,
    github_token: Option<String>,
    github_api_blocked: AtomicBool,
    github_cache: Mutex<BTreeMap<String, GitHubCacheEntry>>,
    github_tags_cache: Mutex<BTreeMap<String, GitHubTagsCacheEntry>>,
    cache: Mutex<DepsCacheState>,
    refresh_cache: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct DepsCacheFile {
    schema_version: u32,
    #[serde(default)]
    crates: BTreeMap<String, CachedCrateSnapshot>,
    #[serde(default)]
    github: BTreeMap<String, CachedGitHubSnapshot>,
    #[serde(default)]
    github_tags: BTreeMap<String, CachedGitHubTagsSnapshot>,
}

impl Default for DepsCacheFile {
    fn default() -> Self {
        Self {
            schema_version: DEPS_CACHE_SCHEMA_VERSION,
            crates: BTreeMap::new(),
            github: BTreeMap::new(),
            github_tags: BTreeMap::new(),
        }
    }
}

impl DepsCacheFile {
    fn prune_expired(&mut self, now: u64) -> bool {
        let crates_before = self.crates.len();
        let github_before = self.github.len();
        let tags_before = self.github_tags.len();
        self.crates.retain(|_, entry| {
            cache_entry_is_fresh(now, entry.fetched_at_unix_secs, CRATES_CACHE_TTL_SECS)
        });
        self.github.retain(|_, entry| {
            cache_entry_is_fresh(now, entry.fetched_at_unix_secs, GITHUB_CACHE_TTL_SECS)
        });
        self.github_tags.retain(|_, entry| {
            cache_entry_is_fresh(now, entry.fetched_at_unix_secs, GITHUB_CACHE_TTL_SECS)
        });
        crates_before != self.crates.len()
            || github_before != self.github.len()
            || tags_before != self.github_tags.len()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedCrateSnapshot {
    fetched_at_unix_secs: u64,
    snapshot: CrateSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedGitHubSnapshot {
    fetched_at_unix_secs: u64,
    snapshot: GitHubRepoResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedGitHubTagsSnapshot {
    fetched_at_unix_secs: u64,
    tags: Vec<GitHubTagSnapshot>,
}

#[derive(Debug, Default)]
struct DepsCacheState {
    path: Option<PathBuf>,
    data: DepsCacheFile,
    dirty: bool,
}

#[derive(Debug)]
struct DepsCacheLock {
    _file: File,
}

#[derive(Clone)]
enum GitHubTagsCacheEntry {
    Hit(Vec<GitHubTagSnapshot>),
    Miss(String),
}

impl GitHubTagsCacheEntry {
    fn into_result(self) -> Result<Vec<GitHubTagSnapshot>> {
        match self {
            Self::Hit(tags) => Ok(tags),
            Self::Miss(err) => bail!("{err}"),
        }
    }
}

impl DepsCacheState {
    fn load() -> Self {
        let Some(path) = deps_cache_path() else {
            return Self::default();
        };

        let mut data = read_cache_file(&path);

        let dirty = data.prune_expired(now_unix_secs());
        Self {
            path: Some(path),
            data,
            dirty,
        }
    }

    fn save_if_dirty(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        let _lock = DepsCacheLock::acquire(&path)?;
        let mut merged = read_cache_file(&path);
        merged.prune_expired(now_unix_secs());
        merge_cache_entries(
            &mut merged.crates,
            std::mem::take(&mut self.data.crates),
            |entry| entry.fetched_at_unix_secs,
        );
        merge_cache_entries(
            &mut merged.github,
            std::mem::take(&mut self.data.github),
            |entry| entry.fetched_at_unix_secs,
        );
        merge_cache_entries(
            &mut merged.github_tags,
            std::mem::take(&mut self.data.github_tags),
            |entry| entry.fetched_at_unix_secs,
        );
        merged.schema_version = DEPS_CACHE_SCHEMA_VERSION;
        let content = serde_json::to_vec_pretty(&merged).context("serialize dependency cache")?;
        write_file_atomically(&path, content)
            .with_context(|| format!("write dependency cache {}", path.display()))?;
        self.data = merged;
        self.dirty = false;
        Ok(())
    }
}

fn read_cache_file(path: &Path) -> DepsCacheFile {
    match fs::read(path) {
        Ok(raw) => match serde_json::from_slice::<DepsCacheFile>(&raw) {
            Ok(parsed) if parsed.schema_version == DEPS_CACHE_SCHEMA_VERSION => parsed,
            Ok(_) => DepsCacheFile::default(),
            Err(err) => {
                eprintln!(
                    "warning: dependency cache parse failed at {}: {err}",
                    path.display()
                );
                DepsCacheFile::default()
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => DepsCacheFile::default(),
        Err(err) => {
            eprintln!(
                "warning: dependency cache read failed at {}: {err}",
                path.display()
            );
            DepsCacheFile::default()
        }
    }
}

pub(super) fn merge_cache_entries<T>(
    target: &mut BTreeMap<String, T>,
    incoming: BTreeMap<String, T>,
    fetched_at: impl Fn(&T) -> u64,
) {
    for (key, value) in incoming {
        if target
            .get(&key)
            .is_none_or(|existing| fetched_at(&value) >= fetched_at(existing))
        {
            target.insert(key, value);
        }
    }
}

impl DepsCacheLock {
    fn acquire(path: &Path) -> Result<Self> {
        let lock_path = path.with_extension("lock");
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create cache directory {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("open cache lock {}", lock_path.display()))?;
        file.lock()
            .with_context(|| format!("acquire cache lock {}", lock_path.display()))?;
        Ok(Self { _file: file })
    }
}

impl Drop for DepsCacheLock {
    fn drop(&mut self) {
        let _ = self._file.unlock();
    }
}

impl ApiClient {
    pub(super) fn new(refresh_cache: bool) -> Result<Self> {
        let crates_http =
            build_http_client("https://crates.io").context("build crates.io HTTP client")?;
        let github_http =
            build_http_client("https://api.github.com").context("build GitHub HTTP client")?;
        let github_token = resolve_github_token()?;
        Ok(Self {
            crates_http,
            github_http,
            github_token,
            github_api_blocked: AtomicBool::new(false),
            github_cache: Mutex::new(BTreeMap::new()),
            github_tags_cache: Mutex::new(BTreeMap::new()),
            cache: Mutex::new(DepsCacheState::load()),
            refresh_cache,
        })
    }

    pub(super) fn flush_cache(&self) -> Result<()> {
        let result = self
            .cache
            .lock()
            .map_err(|_| anyhow!("dependency cache lock poisoned"))
            .and_then(|mut cache| cache.save_if_dirty());
        if let Err(err) = result {
            eprintln!("warning: dependency cache write skipped: {err:#}");
        }
        Ok(())
    }

    pub(super) fn audit_one(&self, spec: DependencySpec) -> Result<DepAuditRecord> {
        let source = spec.source.clone();
        let mut record = DepAuditRecord {
            name: spec.name.clone(),
            source: spec.source,
            requirement: spec.requirement.clone(),
            kinds: spec.kinds,
            project_rust_version: spec.project_rust_version,
            optional: spec.optional,
            latest_version: None,
            update_plan: None,
            suggested_requirement: None,
            update_note: None,
            latest_version_license: None,
            latest_version_rust_version: None,
            msrv_compatible: None,
            latest_version_yanked: None,
            crate_updated_at: None,
            latest_release_at: None,
            latest_release_age_days: None,
            repository: None,
            github_stars: None,
            github_archived: None,
            github_pushed_at: None,
            github_push_age_days: None,
            std_alternative: std_alternative(&spec.name).map(ToOwned::to_owned),
            risk: RiskLevel::Unknown,
            notes: Vec::new(),
        };

        match source {
            DependencySource::CratesIo => match self.fetch_crate(&spec.name) {
                Ok(crate_resp) => {
                    let (update_plan, suggested_requirement, update_note) =
                        model::build_manifest_update_plan(
                            &spec.requirement,
                            &crate_resp.max_version,
                        );
                    record.latest_version = Some(crate_resp.max_version.clone());
                    record.update_plan = Some(update_plan);
                    record.suggested_requirement = suggested_requirement;
                    record.update_note = update_note;
                    record.latest_version_license = crate_resp.latest_version_license.clone();
                    record.latest_version_rust_version =
                        crate_resp.latest_version_rust_version.clone();
                    model::apply_dependency_msrv_policy(&mut record);
                    record.latest_version_yanked = crate_resp.latest_version_yanked;
                    record.crate_updated_at = crate_resp.updated_at.clone();
                    record.latest_release_at = crate_resp.latest_release_at.clone();
                    record.latest_release_age_days = crate_resp
                        .latest_release_at
                        .as_deref()
                        .and_then(age_days_from_now);
                    record.repository = crate_resp.repository.clone();
                }
                Err(err) => {
                    record.notes.push(format!("crates.io query failed: {err}"));
                    classify_risk(&mut record);
                    return Ok(record);
                }
            },
            DependencySource::Git(url) => {
                record.repository = Some(url);
                record.update_plan = Some(DependencyUpdatePlan::Review);
                record.update_note =
                    Some("git dependency revision requires manual review".to_string());
            }
            DependencySource::Registry(registry) => {
                record.update_plan = Some(DependencyUpdatePlan::Review);
                record.update_note = Some(format!(
                    "alternate registry `{registry}` requires registry-specific resolution"
                ));
                record
                    .notes
                    .push("alternate registry metadata not queried".to_string());
                classify_risk(&mut record);
                return Ok(record);
            }
            DependencySource::Path(path) => {
                record.update_plan = Some(DependencyUpdatePlan::Review);
                record.update_note = Some(format!("unexpected path dependency `{path}`"));
                classify_risk(&mut record);
                return Ok(record);
            }
        }

        if let Some(repo_url) = record.repository.as_deref() {
            if let Some((owner, repo)) = github_repo_from_url(repo_url) {
                match self.fetch_github_repo_cached(&owner, &repo) {
                    Ok(gh) => {
                        record.github_stars = Some(gh.stargazers_count);
                        record.github_archived = Some(gh.archived);
                        record.github_pushed_at = gh.pushed_at.clone();
                        record.github_push_age_days =
                            gh.pushed_at.as_deref().and_then(age_days_from_now);
                    }
                    Err(err) => {
                        record.notes.push(format!("GitHub query failed: {err}"));
                    }
                }
            } else {
                record
                    .notes
                    .push("repository is not a GitHub repo URL".to_string());
            }
        } else {
            record.notes.push("repository URL missing".to_string());
        }

        classify_risk(&mut record);
        Ok(record)
    }

    pub(super) fn audit_action(&self, spec: WorkflowActionSpec) -> Result<ActionAuditRecord> {
        let latest_tags = self
            .fetch_github_tags_cached(&spec.owner, &spec.repo)
            .map_err(|err| err.to_string());
        Ok(build_action_audit_record(spec, latest_tags))
    }

    fn fetch_github_repo_cached(&self, owner: &str, repo: &str) -> Result<GitHubRepoResponse> {
        let key = format!("{owner}/{repo}");
        if let Some(snapshot) = self.cache_get_github(&key)? {
            return Ok(snapshot);
        }

        if let Some(entry) = self
            .github_cache
            .lock()
            .map_err(|_| anyhow!("github cache lock poisoned"))?
            .get(&key)
            .cloned()
        {
            return entry.into_result();
        }

        let fetched = self.fetch_github_repo(owner, repo);
        let entry = match fetched {
            Ok(repo) => {
                self.cache_put_github(&key, repo.clone())?;
                GitHubCacheEntry::Hit(repo)
            }
            Err(err) => GitHubCacheEntry::Miss(err.to_string()),
        };

        self.github_cache
            .lock()
            .map_err(|_| anyhow!("github cache lock poisoned"))?
            .insert(key, entry.clone());

        entry.into_result()
    }

    fn fetch_github_tags_cached(&self, owner: &str, repo: &str) -> Result<Vec<GitHubTagSnapshot>> {
        let key = format!("{owner}/{repo}");
        if let Some(tags) = self.cache_get_github_tags(&key)? {
            return Ok(tags);
        }

        if let Some(entry) = self
            .github_tags_cache
            .lock()
            .map_err(|_| anyhow!("github tags cache lock poisoned"))?
            .get(&key)
            .cloned()
        {
            return entry.into_result();
        }

        let fetched = self.fetch_github_tags(owner, repo);
        let entry = match fetched {
            Ok(tags) => {
                self.cache_put_github_tags(&key, tags.clone())?;
                GitHubTagsCacheEntry::Hit(tags)
            }
            Err(err) => GitHubTagsCacheEntry::Miss(err.to_string()),
        };

        self.github_tags_cache
            .lock()
            .map_err(|_| anyhow!("github tags cache lock poisoned"))?
            .insert(key, entry.clone());

        entry.into_result()
    }

    pub(super) fn fetch_crate(&self, name: &str) -> Result<CrateSnapshot> {
        if let Some(snapshot) = self.cache_get_crate(name)? {
            return Ok(snapshot);
        }

        let parsed = self.retry_with_backoff("request crates.io API", || {
            let mut req = self.crates_http.get(format!("/api/v1/crates/{name}"));
            req = req
                .try_header("user-agent", HTTP_USER_AGENT)
                .map_err(|err| AttemptError::Fatal(anyhow!("set user-agent header: {err}")))?;
            let response = req.send_response().map_err(|err| {
                AttemptError::Retryable(anyhow!("request crates.io API failed: {err}"))
            })?;
            let status = response.status();
            if !status.is_success() {
                let body = text_render::truncate_end(&response.text_lossy(), 200);
                if is_retryable_status(status.as_u16()) {
                    return Err(AttemptError::Retryable(anyhow!(
                        "status {} body {}",
                        status,
                        body
                    )));
                }
                return Err(AttemptError::Fatal(anyhow!(
                    "status {} body {}",
                    status,
                    body
                )));
            }
            response
                .json::<CratesApiResponse>()
                .map_err(|err| AttemptError::Fatal(anyhow!("parse crates.io JSON: {err}")))
        })?;

        let max_version = parsed
            .krate
            .max_stable_version
            .clone()
            .or(parsed.krate.max_version.clone())
            .ok_or_else(|| anyhow!("missing max version in crates.io response"))?;
        let latest_version = parsed.versions.iter().find(|v| v.num == max_version);
        let latest_release_at = latest_version
            .map(|v| v.created_at.clone())
            .or_else(|| parsed.krate.updated_at.clone());

        let snapshot = CrateSnapshot {
            max_version,
            updated_at: parsed.krate.updated_at,
            latest_release_at,
            repository: parsed.krate.repository,
            latest_version_license: latest_version
                .and_then(|v| normalize_optional_string(v.license.clone())),
            latest_version_rust_version: latest_version
                .and_then(|v| normalize_optional_string(v.rust_version.clone())),
            latest_version_yanked: latest_version.map(|v| v.yanked),
        };
        self.cache_put_crate(name, snapshot.clone())?;
        Ok(snapshot)
    }

    fn fetch_github_repo(&self, owner: &str, repo: &str) -> Result<GitHubRepoResponse> {
        if self.github_api_blocked.load(Ordering::Relaxed) {
            bail!("skipped after GitHub API 403 (set GITHUB_TOKEN for stable quota)");
        }

        self.retry_with_backoff("request GitHub API", || {
            let mut req = self.github_http.get(format!("/repos/{owner}/{repo}"));
            req = req
                .try_header("user-agent", HTTP_USER_AGENT)
                .map_err(|err| AttemptError::Fatal(anyhow!("set user-agent header: {err}")))?;
            req = req
                .try_header("accept", "application/vnd.github+json")
                .map_err(|err| {
                    AttemptError::Fatal(anyhow!("set accept header for GitHub request: {err}"))
                })?;
            if let Some(token) = self.github_token.as_deref() {
                req = req
                    .try_header("authorization", &format!("Bearer {token}"))
                    .map_err(|err| {
                        AttemptError::Fatal(anyhow!(
                            "set authorization header for GitHub request: {err}"
                        ))
                    })?;
            }

            let response = req.send_response().map_err(|err| {
                AttemptError::Retryable(anyhow!("request GitHub API failed: {err}"))
            })?;
            let status = response.status();
            if !status.is_success() {
                let body = text_render::truncate_end(&response.text_lossy(), 200);
                if status.as_u16() == 403 {
                    self.github_api_blocked.store(true, Ordering::Relaxed);
                    return Err(AttemptError::Fatal(anyhow!(
                        "status {} (rate-limited or forbidden); body {}",
                        status,
                        body
                    )));
                }
                if is_retryable_status(status.as_u16()) {
                    return Err(AttemptError::Retryable(anyhow!(
                        "status {} body {}",
                        status,
                        body
                    )));
                }
                return Err(AttemptError::Fatal(anyhow!(
                    "status {} body {}",
                    status,
                    body
                )));
            }

            response
                .json::<GitHubRepoResponse>()
                .map_err(|err| AttemptError::Fatal(anyhow!("parse GitHub JSON: {err}")))
        })
    }

    fn fetch_github_tags(&self, owner: &str, repo: &str) -> Result<Vec<GitHubTagSnapshot>> {
        if self.github_api_blocked.load(Ordering::Relaxed) {
            bail!("skipped after GitHub API 403 (set GITHUB_TOKEN for stable quota)");
        }

        let mut tags = Vec::new();
        for page in 1..=WORKFLOW_ACTION_REF_MAX_PAGES {
            let page_tags = self.retry_with_backoff("request GitHub tags API", || {
                let mut req = self.github_http.get(format!(
                    "/repos/{owner}/{repo}/tags?per_page={WORKFLOW_ACTION_REF_MAX_TAGS}&page={page}"
                ));
                req = req
                    .try_header("user-agent", HTTP_USER_AGENT)
                    .map_err(|err| AttemptError::Fatal(anyhow!("set user-agent header: {err}")))?;
                req = req
                    .try_header("accept", "application/vnd.github+json")
                    .map_err(|err| {
                        AttemptError::Fatal(anyhow!("set accept header for GitHub request: {err}"))
                    })?;
                if let Some(token) = self.github_token.as_deref() {
                    req = req
                        .try_header("authorization", &format!("Bearer {token}"))
                        .map_err(|err| {
                            AttemptError::Fatal(anyhow!(
                                "set authorization header for GitHub request: {err}"
                            ))
                        })?;
                }

                let response = req.send_response().map_err(|err| {
                    AttemptError::Retryable(anyhow!("request GitHub tags API failed: {err}"))
                })?;
                let status = response.status();
                if !status.is_success() {
                    let body = text_render::truncate_end(&response.text_lossy(), 200);
                    if status.as_u16() == 403 {
                        self.github_api_blocked.store(true, Ordering::Relaxed);
                        return Err(AttemptError::Fatal(anyhow!(
                            "status {} (rate-limited or forbidden); body {}",
                            status,
                            body
                        )));
                    }
                    if is_retryable_status(status.as_u16()) {
                        return Err(AttemptError::Retryable(anyhow!(
                            "status {} body {}",
                            status,
                            body
                        )));
                    }
                    return Err(AttemptError::Fatal(anyhow!(
                        "status {} body {}",
                        status,
                        body
                    )));
                }

                response
                    .json::<Vec<GitHubTagResponse>>()
                    .map_err(|err| AttemptError::Fatal(anyhow!("parse GitHub tags JSON: {err}")))
            })?;
            let page_len = page_tags.len();
            tags.extend(page_tags.into_iter().map(|tag| GitHubTagSnapshot {
                name: tag.name,
                sha: tag.commit.sha,
            }));
            if page_len < WORKFLOW_ACTION_REF_MAX_TAGS {
                break;
            }
        }
        Ok(tags)
    }

    fn cache_get_crate(&self, name: &str) -> Result<Option<CrateSnapshot>> {
        if self.refresh_cache {
            return Ok(None);
        }
        let now = now_unix_secs();
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| anyhow!("dependency cache lock poisoned"))?;
        if let Some(entry) = cache.data.crates.get(name) {
            if cache_entry_is_fresh(now, entry.fetched_at_unix_secs, CRATES_CACHE_TTL_SECS) {
                return Ok(Some(entry.snapshot.clone()));
            }
            cache.data.crates.remove(name);
            cache.dirty = true;
        }
        Ok(None)
    }

    fn cache_put_crate(&self, name: &str, snapshot: CrateSnapshot) -> Result<()> {
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| anyhow!("dependency cache lock poisoned"))?;
        cache.data.crates.insert(
            name.to_string(),
            CachedCrateSnapshot {
                fetched_at_unix_secs: now_unix_secs(),
                snapshot,
            },
        );
        cache.dirty = true;
        Ok(())
    }

    fn cache_get_github(&self, repo_key: &str) -> Result<Option<GitHubRepoResponse>> {
        if self.refresh_cache {
            return Ok(None);
        }
        let now = now_unix_secs();
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| anyhow!("dependency cache lock poisoned"))?;
        if let Some(entry) = cache.data.github.get(repo_key) {
            if cache_entry_is_fresh(now, entry.fetched_at_unix_secs, GITHUB_CACHE_TTL_SECS) {
                return Ok(Some(entry.snapshot.clone()));
            }
            cache.data.github.remove(repo_key);
            cache.dirty = true;
        }
        Ok(None)
    }

    fn cache_put_github(&self, repo_key: &str, snapshot: GitHubRepoResponse) -> Result<()> {
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| anyhow!("dependency cache lock poisoned"))?;
        cache.data.github.insert(
            repo_key.to_string(),
            CachedGitHubSnapshot {
                fetched_at_unix_secs: now_unix_secs(),
                snapshot,
            },
        );
        cache.dirty = true;
        Ok(())
    }

    fn cache_get_github_tags(&self, repo_key: &str) -> Result<Option<Vec<GitHubTagSnapshot>>> {
        if self.refresh_cache {
            return Ok(None);
        }
        let now = now_unix_secs();
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| anyhow!("dependency cache lock poisoned"))?;
        if let Some(entry) = cache.data.github_tags.get(repo_key) {
            if cache_entry_is_fresh(now, entry.fetched_at_unix_secs, GITHUB_CACHE_TTL_SECS) {
                return Ok(Some(entry.tags.clone()));
            }
            cache.data.github_tags.remove(repo_key);
            cache.dirty = true;
        }
        Ok(None)
    }

    fn cache_put_github_tags(&self, repo_key: &str, tags: Vec<GitHubTagSnapshot>) -> Result<()> {
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| anyhow!("dependency cache lock poisoned"))?;
        cache.data.github_tags.insert(
            repo_key.to_string(),
            CachedGitHubTagsSnapshot {
                fetched_at_unix_secs: now_unix_secs(),
                tags,
            },
        );
        cache.dirty = true;
        Ok(())
    }

    fn retry_with_backoff<T, F>(&self, op_name: &str, mut f: F) -> Result<T>
    where
        F: FnMut() -> std::result::Result<T, AttemptError>,
    {
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 1..=HTTP_MAX_ATTEMPTS {
            match f() {
                Ok(value) => return Ok(value),
                Err(AttemptError::Fatal(err)) => return Err(err),
                Err(AttemptError::Retryable(err)) => {
                    last_err = Some(err);
                    if attempt == HTTP_MAX_ATTEMPTS {
                        break;
                    }
                    let backoff = HTTP_BACKOFF_BASE_MS.saturating_mul(1 << (attempt - 1));
                    thread::sleep(Duration::from_millis(backoff));
                }
            }
        }

        let err = last_err.unwrap_or_else(|| anyhow!("unknown retry failure"));
        Err(err).with_context(|| format!("{op_name} failed after {HTTP_MAX_ATTEMPTS} attempts"))
    }
}

fn normalize_optional_string(input: Option<String>) -> Option<String> {
    let value = input?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

fn build_http_client(base_url: &str) -> Result<Client> {
    build_client(
        base_url,
        "za-deps-audit",
        ClientProfile::StandardSdk,
        false,
        Duration::from_secs(HTTP_TIMEOUT_SECS),
        za_config::ProxyScope::Deps,
    )
}

fn resolve_github_token() -> Result<Option<String>> {
    if let Ok(token) = env::var("GITHUB_TOKEN") {
        let trimmed = token.trim();
        if !trimmed.is_empty() {
            return Ok(Some(trimmed.to_string()));
        }
    }

    if let Ok(token) = env::var("GH_TOKEN") {
        let trimmed = token.trim();
        if !trimmed.is_empty() {
            return Ok(Some(trimmed.to_string()));
        }
    }

    za_config::load_github_token()
}

enum AttemptError {
    Retryable(anyhow::Error),
    Fatal(anyhow::Error),
}

fn is_retryable_status(status_code: u16) -> bool {
    status_code == 408 || status_code == 429 || (500..=599).contains(&status_code)
}

pub(super) fn cache_entry_is_fresh(now: u64, fetched_at: u64, ttl_secs: u64) -> bool {
    fetched_at <= now && now - fetched_at <= ttl_secs
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn deps_cache_path() -> Option<PathBuf> {
    if let Some(base) = env::var_os("XDG_CACHE_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(base).join("za").join(DEPS_CACHE_FILE_NAME));
    }
    let home = env::var_os("HOME").filter(|value| !value.is_empty())?;
    Some(
        PathBuf::from(home)
            .join(".cache")
            .join("za")
            .join(DEPS_CACHE_FILE_NAME),
    )
}
