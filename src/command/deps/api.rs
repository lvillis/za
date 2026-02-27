use super::*;

pub(super) struct ApiClient {
    crates_http: Client,
    github_http: Client,
    github_token: Option<String>,
    github_api_blocked: AtomicBool,
    github_cache: Mutex<BTreeMap<String, GitHubCacheEntry>>,
    cache: Mutex<DepsCacheState>,
}

#[derive(Debug, Serialize, Deserialize)]
struct DepsCacheFile {
    schema_version: u32,
    #[serde(default)]
    crates: BTreeMap<String, CachedCrateSnapshot>,
    #[serde(default)]
    github: BTreeMap<String, CachedGitHubSnapshot>,
}

impl Default for DepsCacheFile {
    fn default() -> Self {
        Self {
            schema_version: DEPS_CACHE_SCHEMA_VERSION,
            crates: BTreeMap::new(),
            github: BTreeMap::new(),
        }
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

#[derive(Debug, Default)]
struct DepsCacheState {
    path: Option<PathBuf>,
    data: DepsCacheFile,
    dirty: bool,
}

impl DepsCacheState {
    fn load() -> Self {
        let Some(path) = deps_cache_path() else {
            return Self::default();
        };

        let data = match fs::read(&path) {
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
        };

        Self {
            path: Some(path),
            data,
            dirty: false,
        }
    }

    fn save_if_dirty(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        self.data.schema_version = DEPS_CACHE_SCHEMA_VERSION;
        let content =
            serde_json::to_vec_pretty(&self.data).context("serialize dependency cache")?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create cache directory {}", parent.display()))?;
        }
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, content).with_context(|| format!("write {}", tmp.display()))?;
        fs::rename(&tmp, &path)
            .with_context(|| format!("replace cache {} -> {}", path.display(), tmp.display()))?;
        self.dirty = false;
        Ok(())
    }
}

impl ApiClient {
    pub(super) fn new(github_token_override: Option<String>) -> Result<Self> {
        let crates_http = Client::builder("https://crates.io")
            .request_timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .total_timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .retry_policy(RetryPolicy::disabled())
            .client_name("za-deps-audit")
            .build()
            .context("build crates.io HTTP client")?;
        let github_http = Client::builder("https://api.github.com")
            .request_timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .total_timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .retry_policy(RetryPolicy::disabled())
            .client_name("za-deps-audit")
            .build()
            .context("build GitHub HTTP client")?;
        let github_token = resolve_github_token(github_token_override)?;
        Ok(Self {
            crates_http,
            github_http,
            github_token,
            github_api_blocked: AtomicBool::new(false),
            github_cache: Mutex::new(BTreeMap::new()),
            cache: Mutex::new(DepsCacheState::load()),
        })
    }

    pub(super) fn flush_cache(&self) -> Result<()> {
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| anyhow!("dependency cache lock poisoned"))?;
        cache.save_if_dirty()
    }

    pub(super) fn audit_one(&self, spec: DependencySpec) -> Result<DepAuditRecord> {
        let mut record = DepAuditRecord {
            name: spec.name.clone(),
            requirement: spec.requirement.clone(),
            kinds: spec.kinds,
            optional: spec.optional,
            latest_version: None,
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

        match self.fetch_crate(&spec.name) {
            Ok(crate_resp) => {
                record.latest_version = Some(crate_resp.max_version.clone());
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

    fn fetch_crate(&self, name: &str) -> Result<CrateSnapshot> {
        if let Some(snapshot) = self.cache_get_crate(name)? {
            return Ok(snapshot);
        }

        let parsed = self.retry_with_backoff("request crates.io API", || {
            let mut req = self.crates_http.get(format!("/api/v1/crates/{name}"));
            req = req
                .try_header("user-agent", HTTP_USER_AGENT)
                .map_err(|err| AttemptError::Fatal(anyhow!("set user-agent header: {err}")))?;
            let response = req.send_with_status().map_err(|err| {
                AttemptError::Retryable(anyhow!("request crates.io API failed: {err}"))
            })?;
            let status = response.status();
            if !status.is_success() {
                let body = truncate(&response.text_lossy(), 200);
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
        let latest_release_at = parsed
            .versions
            .iter()
            .find(|v| v.num == max_version)
            .map(|v| v.created_at.clone())
            .or_else(|| parsed.krate.updated_at.clone());

        let snapshot = CrateSnapshot {
            max_version,
            updated_at: parsed.krate.updated_at,
            latest_release_at,
            repository: parsed.krate.repository,
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

            let response = req.send_with_status().map_err(|err| {
                AttemptError::Retryable(anyhow!("request GitHub API failed: {err}"))
            })?;
            let status = response.status();
            if !status.is_success() {
                let body = truncate(&response.text_lossy(), 200);
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

    fn cache_get_crate(&self, name: &str) -> Result<Option<CrateSnapshot>> {
        let now = now_unix_secs();
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| anyhow!("dependency cache lock poisoned"))?;
        if let Some(entry) = cache.data.crates.get(name) {
            if now.saturating_sub(entry.fetched_at_unix_secs) <= CRATES_CACHE_TTL_SECS {
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
        let now = now_unix_secs();
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| anyhow!("dependency cache lock poisoned"))?;
        if let Some(entry) = cache.data.github.get(repo_key) {
            if now.saturating_sub(entry.fetched_at_unix_secs) <= GITHUB_CACHE_TTL_SECS {
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

fn resolve_github_token(override_token: Option<String>) -> Result<Option<String>> {
    if let Some(token) = override_token {
        let trimmed = token.trim();
        if !trimmed.is_empty() {
            return Ok(Some(trimmed.to_string()));
        }
    }

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

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn deps_cache_path() -> Option<PathBuf> {
    if let Some(base) = env::var_os("XDG_CACHE_HOME") {
        return Some(PathBuf::from(base).join("za").join(DEPS_CACHE_FILE_NAME));
    }
    let home = env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".cache")
            .join("za")
            .join(DEPS_CACHE_FILE_NAME),
    )
}
